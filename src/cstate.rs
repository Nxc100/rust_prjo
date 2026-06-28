#![allow(dead_code)]
//! Workflow C 生产化批量层 · 客户级状态机（移植 `c_batch.py` 的 manifest 逻辑，去掉浏览器那套）。
//!
//! 一个客户一份 manifest：每张人物原片一个单子，状态流
//!   `pending → prompted → awaiting_qc → selected → done`（外加 `qc_fail`）。
//! 提供：建账(init)、改计划字段(set，含失效回退规则)、自动选景排重(auto_plan)、
//! 装配(assemble，调 `cmode` + 复用即变构图)、记候选/选片/判废、归集(collect)、持久化。
//!
//! 与 `c_batch.py` 的差异：出图走 `foursapi`（不写提示词到磁盘，直接内联在单子里）；
//! QC 用工作台的全分辨率对比器（不再拼静态 qc_sheet）。

use crate::cmode;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const C_IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "jfif"];
/// 选片长边低于此像素 → 标记需 Topaz/RunningHub 放大。
pub const UPSCALE_EDGE: u32 = 1900;

/// #2 复用即变构图：某场景被多单共用时，给「第 2+ 次使用」的单追加此句（概念无关，守四轴）。
const VARIATION_TAG: &str = "【同场景·换构图";
const VARIATION_SENTENCE: &str = "【同场景·换构图（本张复用了同一参考场景）】本张与另外几张共用同一张图B参考场景，请当作\"同一地点的不同取景\"来拍：换一个机位高度/取景角度/人物站位与画面比例（取景更紧或更松、人物左右移位、前后景占比不同、视角偏侧），使背景构图与共用此场景的其他张明显不同、不雷同复制；但场景本身的真实地貌与光线基调保持一致。";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CStatus {
    Pending,
    Prompted,
    Generating, // 正在调 4sapi 出图（并发时多张同时）
    AwaitingQc,
    Selected,
    Done,
    QcFail,
    Skipped, // 断点续跑：输出已存在、本轮未重出
    Failed,  // 出图失败（4sapi 报错）
}
impl Default for CStatus {
    fn default() -> Self {
        CStatus::Pending
    }
}
impl CStatus {
    pub fn label(self) -> &'static str {
        match self {
            CStatus::Pending => "待装配",
            CStatus::Prompted => "待出图",
            CStatus::Generating => "生成中",
            CStatus::AwaitingQc => "待QC",
            CStatus::Selected => "已选片",
            CStatus::Done => "已归集",
            CStatus::QcFail => "已判废",
            CStatus::Skipped => "已跳过",
            CStatus::Failed => "出图失败",
        }
    }
    /// 是否已产出可看的结果（成功/已选/已归集/已跳过 都有图）。
    pub fn has_result(self) -> bool {
        matches!(
            self,
            CStatus::AwaitingQc | CStatus::Selected | CStatus::Done | CStatus::Skipped
        )
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct CJob {
    pub no: String,
    pub input: String, // 人物原片绝对路径
    pub subjects: Option<u8>,
    pub shot: Option<String>,
    pub scene: Option<String>,
    pub scene_file: Option<String>, // 相对 assets base：scenes/<file>
    pub candidates: Option<u32>,
    pub prompt: Option<String>, // 内联装配好的提示词（foursapi 直接用）
    pub tpl_md5: Option<String>,
    pub status: CStatus,
    pub outputs: Vec<String>, // 候选出图的绝对路径
    pub selected: Option<String>,
    pub needs_upscale: Option<bool>,
    pub qc_notes: String,
}
impl Default for CJob {
    fn default() -> Self {
        Self {
            no: String::new(),
            input: String::new(),
            subjects: None,
            shot: None,
            scene: None,
            scene_file: None,
            candidates: None,
            prompt: None,
            tpl_md5: None,
            status: CStatus::Pending,
            outputs: Vec::new(),
            selected: None,
            needs_upscale: None,
            qc_notes: String::new(),
        }
    }
}
impl CJob {
    fn new(no: String, input: String) -> Self {
        Self { no, input, ..Default::default() }
    }
    /// 该单出几张候选（不填按默认 1）。
    pub fn candidate_count(&self) -> u32 {
        self.candidates.unwrap_or(1).max(1)
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct CManifest {
    pub client: String,
    pub key: String,             // natural|warm|overcast（全册一个基调）
    pub series: Option<String>,  // 场景概念池（bbsh/jiaoshi…），None=全部
    pub reuse_policy: String,    // relaxed（默认）|distinct
    pub jobs: Vec<CJob>,
}
impl Default for CManifest {
    fn default() -> Self {
        Self {
            client: String::new(),
            key: "natural".into(),
            series: None,
            reuse_policy: "relaxed".into(),
            jobs: Vec::new(),
        }
    }
}

fn tpl_hash(tpl: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tpl.hash(&mut h);
    format!("{:016x}", h.finish())
}

impl CManifest {
    // ---------- 建账 / 持久化 ----------

    /// 扫输入文件夹建 manifest（每张图一个单子，no=01..）。
    pub fn init(client: &str, input_dir: &Path) -> Result<CManifest> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(input_dir)
            .map_err(|e| anyhow::anyhow!("读输入目录失败 {}：{e}", input_dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|s| s.to_str())
                        .map(|e| C_IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
                        .unwrap_or(false)
            })
            .collect();
        files.sort();
        if files.is_empty() {
            bail!("{} 里没有图片（支持 {}）", input_dir.display(), C_IMAGE_EXTS.join("/"));
        }
        let jobs = files
            .iter()
            .enumerate()
            .map(|(i, p)| CJob::new(format!("{:02}", i + 1), p.display().to_string()))
            .collect();
        Ok(CManifest {
            client: client.to_string(),
            jobs,
            ..Default::default()
        })
    }

    pub fn load(path: &Path) -> Result<CManifest> {
        let s = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&s)?)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).ok();
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn job(&self, no: &str) -> Option<&CJob> {
        self.jobs.iter().find(|j| j.no == no)
    }
    pub fn job_mut(&mut self, no: &str) -> Option<&mut CJob> {
        self.jobs.iter_mut().find(|j| j.no == no)
    }

    // ---------- 改计划字段（带失效回退）----------

    /// 改单子的 shot/subjects/scene（计划字段）。规则同 c_batch：
    /// - awaiting_qc/selected/done 上改计划字段 → 拒绝（先 fail）；
    /// - prompted/qc_fail 上改 → 作废提示词/候选、回 pending；
    ///   shot 宽→窄保留旧场景（同片远近复用＝系列感），窄→宽且旧场景撑不起 → 清空待重选。
    pub fn set_job(
        &mut self,
        no: &str,
        shot: Option<String>,
        subjects: Option<u8>,
        scene: Option<String>,
        scenes: &[cmode::SceneEntry],
    ) -> Result<()> {
        let idx = self
            .jobs
            .iter()
            .position(|j| j.no == no)
            .ok_or_else(|| anyhow::anyhow!("没有单子 no={no}"))?;
        let (old_shot, old_scene, old_subjects, status) = {
            let j = &self.jobs[idx];
            (j.shot.clone(), j.scene.clone(), j.subjects, j.status)
        };
        let plan_change = shot.as_ref().map_or(false, |s| Some(s) != old_shot.as_ref())
            || scene.as_ref().map_or(false, |s| Some(s) != old_scene.as_ref())
            || subjects.map_or(false, |s| Some(s) != old_subjects);

        if plan_change && matches!(status, CStatus::AwaitingQc | CStatus::Selected | CStatus::Done) {
            bail!("单子 {no} 状态={:?}：已出候选/已选片，不允许直接改 shot/scene/subjects（先判废 fail）", status);
        }

        // 校验 scene 合法
        if let Some(sc) = &scene {
            if !scenes.iter().any(|s| &s.id == sc) {
                bail!("catalog 里没有场景 id={sc}");
            }
        }
        if let Some(s) = &shot {
            if !cmode::SHOT_SET.contains(&s.as_str()) {
                bail!("--shot 必须是 {:?}", cmode::SHOT_SET);
            }
        }

        let j = &mut self.jobs[idx];
        if let Some(s) = shot.clone() {
            j.shot = Some(s);
        }
        if let Some(s) = subjects {
            j.subjects = Some(s);
        }
        if let Some(sc) = scene.clone() {
            let file = scenes.iter().find(|s| s.id == sc).map(cmode::plate_rel_path);
            j.scene = Some(sc);
            j.scene_file = file;
        }

        // 计划字段变了 → 作废，退回 pending
        if plan_change && matches!(status, CStatus::Prompted | CStatus::QcFail) {
            j.status = CStatus::Pending;
            j.prompt = None;
            j.tpl_md5 = None;
            j.outputs.clear();
            j.selected = None;
            j.needs_upscale = None;
            // shot 改了：旧场景撑不起更宽的新景别(窄→宽)才清空；宽→窄保留（本次显式指定 scene 的除外）
            if let (Some(new_shot), Some(cur_scene)) = (&shot, j.scene.clone()) {
                if scene.is_none() {
                    if let Some(ent) = scenes.iter().find(|s| s.id == cur_scene) {
                        if !cmode::can_serve(ent, new_shot) {
                            j.scene = None;
                            j.scene_file = None;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ---------- 自动选景排重 ----------

    /// 对已填 shot、未填 scene 的单子自动选景（最配优先→排重→复用换景别）。
    pub fn auto_plan(&mut self, scenes: &[cmode::SceneEntry]) {
        let relaxed = self.reuse_policy == "relaxed";
        let series = self.series.clone();
        for i in 0..self.jobs.len() {
            let (shot, has_scene) = {
                let j = &self.jobs[i];
                (j.shot.clone(), j.scene.is_some())
            };
            let Some(shot) = shot else { continue };
            if has_scene {
                continue;
            }
            // 用其它单当前已分配的场景构建 used 表
            let mut used: HashMap<String, HashSet<String>> = HashMap::new();
            for (k, x) in self.jobs.iter().enumerate() {
                if k == i {
                    continue;
                }
                if let Some(sc) = &x.scene {
                    let e = used.entry(sc.clone()).or_default();
                    if let Some(sh) = &x.shot {
                        e.insert(sh.clone());
                    }
                }
            }
            let (pick, _why) =
                cmode::auto_pick_scene(scenes, &shot, series.as_deref(), None, relaxed, &used);
            if let Some(id) = pick {
                let file = scenes.iter().find(|s| s.id == id).map(cmode::plate_rel_path);
                self.jobs[i].scene = Some(id);
                self.jobs[i].scene_file = file;
            }
        }
    }

    // ---------- 装配 ----------

    /// 对 pending/qc_fail 且已填 shot+scene 的单子装配提示词（内联存单子），并应用复用即变构图。
    /// 返回成功装配的单号列表。
    pub fn assemble(&mut self, assets: &cmode::Assets) -> Vec<String> {
        let h = tpl_hash(&assets.tpl);
        let mut done = Vec::new();
        for i in 0..self.jobs.len() {
            let (status, shot, scene, subjects) = {
                let j = &self.jobs[i];
                (j.status, j.shot.clone(), j.scene.clone(), j.subjects)
            };
            if !matches!(status, CStatus::Pending | CStatus::QcFail) {
                continue;
            }
            let (Some(shot), Some(scene)) = (shot, scene) else { continue };
            let subj = subjects.unwrap_or(2);
            let Some(entry) = assets.scenes.iter().find(|s| s.id == scene) else { continue };
            if let Ok(p) = cmode::assemble_prompt(assets, entry, &shot, &self.key, subj) {
                let j = &mut self.jobs[i];
                j.prompt = Some(p);
                j.scene_file = Some(cmode::plate_rel_path(entry));
                j.tpl_md5 = Some(h.clone());
                if j.status == CStatus::QcFail {
                    j.outputs.clear();
                    j.selected = None;
                    j.needs_upscale = None;
                }
                j.status = CStatus::Prompted;
                done.push(j.no.clone());
            }
        }
        self.apply_reuse_variation(&done);
        done
    }

    /// #2 复用即变构图：某场景被多单共用 → 给「第 2+ 次使用」且本次刚装配的单追加换构图句。
    fn apply_reuse_variation(&mut self, just_done: &[String]) {
        let mut order: HashMap<String, Vec<String>> = HashMap::new();
        let mut jobs_sorted: Vec<&CJob> = self.jobs.iter().collect();
        jobs_sorted.sort_by(|a, b| a.no.cmp(&b.no));
        for j in jobs_sorted {
            if let Some(sc) = &j.scene {
                order.entry(sc.clone()).or_default().push(j.no.clone());
            }
        }
        for i in 0..self.jobs.len() {
            let (no, scene, has_prompt) = {
                let j = &self.jobs[i];
                (j.no.clone(), j.scene.clone(), j.prompt.is_some())
            };
            if !just_done.contains(&no) || !has_prompt {
                continue;
            }
            let Some(scene) = scene else { continue };
            let seq = order.get(&scene);
            let is_reuse = seq.map_or(false, |v| v.iter().position(|n| n == &no).unwrap_or(0) >= 1);
            if is_reuse {
                let j = &mut self.jobs[i];
                if let Some(p) = &j.prompt {
                    if !p.contains(VARIATION_TAG) {
                        j.prompt = Some(format!("{}\n\n{}\n", p.trim_end(), VARIATION_SENTENCE));
                    }
                }
            }
        }
    }

    // ---------- 出图 / QC / 选片 / 归集 ----------

    pub fn record_outputs(&mut self, no: &str, paths: Vec<String>) {
        if let Some(j) = self.job_mut(no) {
            let empty = paths.is_empty();
            j.outputs = paths;
            j.status = if empty { CStatus::Prompted } else { CStatus::AwaitingQc };
        }
    }

    /// 选片：pick 必须在该单候选里；long_edge < UPSCALE_EDGE → 标记需放大。
    pub fn select(&mut self, no: &str, pick: &str, long_edge: u32) -> Result<()> {
        let j = self.job_mut(no).ok_or_else(|| anyhow::anyhow!("没有单子 {no}"))?;
        if !j.outputs.iter().any(|o| o == pick) {
            bail!("候选 {pick} 不在该单候选里：{:?}", j.outputs);
        }
        j.selected = Some(pick.to_string());
        j.needs_upscale = Some(long_edge < UPSCALE_EDGE);
        j.status = CStatus::Selected;
        Ok(())
    }

    pub fn fail(&mut self, no: &str, notes: &str) -> Result<()> {
        let j = self.job_mut(no).ok_or_else(|| anyhow::anyhow!("没有单子 {no}"))?;
        j.status = CStatus::QcFail;
        if !notes.is_empty() {
            if !j.qc_notes.is_empty() {
                j.qc_notes.push_str(" | ");
            }
            j.qc_notes.push_str(notes);
        }
        Ok(())
    }

    /// 归集：把 selected 单的选片复制到 final_dir/<no>.<ext>，状态置 done。
    /// 返回 (已归集单号, 需放大单号)。
    pub fn collect(&mut self, final_dir: &Path) -> Result<(Vec<String>, Vec<String>)> {
        std::fs::create_dir_all(final_dir)?;
        let mut done = Vec::new();
        let mut ups = Vec::new();
        for j in self.jobs.iter_mut() {
            if j.status != CStatus::Selected {
                continue;
            }
            let Some(sel) = &j.selected else { continue };
            let src = Path::new(sel);
            let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("png");
            let dst = final_dir.join(format!("{}.{}", j.no, ext));
            if src.exists() {
                std::fs::copy(src, &dst)?;
                j.status = CStatus::Done;
                done.push(j.no.clone());
                if j.needs_upscale == Some(true) {
                    ups.push(j.no.clone());
                }
            }
        }
        Ok((done, ups))
    }

    // ---------- 统计 ----------

    /// (总数, 待装配, 待出图, 待QC, 已选, 已归集, 判废)
    /// (总数, 进行中, 成功[有结果], 跳过, 失败) — 与列表筛选/进度条对齐。
    pub fn counts(&self) -> (usize, usize, usize, usize, usize) {
        let (mut active, mut ok, mut skip, mut fail) = (0, 0, 0, 0);
        for j in &self.jobs {
            match j.status {
                CStatus::Pending | CStatus::Prompted | CStatus::Generating => active += 1,
                CStatus::AwaitingQc | CStatus::Selected | CStatus::Done => ok += 1,
                CStatus::Skipped => skip += 1,
                CStatus::QcFail | CStatus::Failed => fail += 1,
            }
        }
        (self.jobs.len(), active, ok, skip, fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn assets() -> cmode::Assets {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/wedding");
        cmode::load_from_dir(&base).unwrap()
    }

    fn temp_client(name: &str, n: usize) -> (PathBuf, CManifest) {
        let dir = std::env::temp_dir().join(format!("cstate_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 1..=n {
            std::fs::write(dir.join(format!("p{i}.jpg")), b"x").unwrap();
        }
        let m = CManifest::init(name, &dir).unwrap();
        (dir, m)
    }

    #[test]
    fn state_machine_dryrun() {
        let a = assets();
        let (dir, mut m) = temp_client("dry", 3);
        assert_eq!(m.jobs.len(), 3);
        m.series = Some("bbsh".into());
        m.key = "warm".into();
        m.set_job("01", Some("full".into()), Some(2), None, &a.scenes).unwrap();
        m.set_job("02", Some("medium".into()), Some(1), None, &a.scenes).unwrap();
        m.set_job("03", Some("closeup".into()), Some(1), None, &a.scenes).unwrap();

        // 非法场景 id 被拦
        assert!(m.set_job("02", None, None, Some("不存在".into()), &a.scenes).is_err());

        // 自动选景：3 单都选上、且排重（互不相同）
        m.auto_plan(&a.scenes);
        let scs: Vec<String> = m.jobs.iter().map(|j| j.scene.clone().unwrap()).collect();
        assert!(scs.iter().all(|s| !s.is_empty()), "应都选上场景");
        assert_eq!(scs.len(), scs.iter().collect::<HashSet<_>>().len(), "应排重互不相同");

        // 装配 → 全部 prompted、提示词非空且含场景构图、无残留占位符
        let done = m.assemble(&a);
        assert_eq!(done.len(), 3);
        for j in &m.jobs {
            assert_eq!(j.status, CStatus::Prompted);
            let p = j.prompt.as_ref().unwrap();
            assert!(!p.is_empty() && !p.contains("{SCENE") && !p.contains("{KEY"));
        }

        // 宽→窄(full→medium)：作废回 pending，旧全身场景仍可服务中景 → 保留
        let scene01 = m.job("01").unwrap().scene.clone().unwrap();
        m.set_job("01", Some("medium".into()), None, None, &a.scenes).unwrap();
        let j1 = m.job("01").unwrap();
        assert_eq!(j1.status, CStatus::Pending);
        assert!(j1.prompt.is_none());
        assert_eq!(j1.scene.as_deref(), Some(scene01.as_str()), "宽→窄应保留旧场景");

        // 窄→宽(closeup→full)：花毯特写撑不起全身 → 清空待重选
        m.set_job("03", Some("closeup".into()), None, Some("bbsh_field01".into()), &a.scenes).unwrap();
        m.set_job("03", Some("full".into()), None, None, &a.scenes).unwrap();
        let j3 = m.job("03").unwrap();
        assert_eq!(j3.status, CStatus::Pending);
        assert!(j3.scene.is_none(), "窄→宽撑不起应清空场景");

        // 已出候选后改计划字段被拒
        m.record_outputs("02", vec!["x.png".into()]);
        assert_eq!(m.job("02").unwrap().status, CStatus::AwaitingQc);
        assert!(m.set_job("02", Some("full".into()), None, None, &a.scenes).is_err());

        // 选片 + 需放大判定
        m.select("02", "x.png", 1200).unwrap();
        assert_eq!(m.job("02").unwrap().status, CStatus::Selected);
        assert_eq!(m.job("02").unwrap().needs_upscale, Some(true));

        // 持久化往返
        let mp = dir.join("manifest.json");
        m.save(&mp).unwrap();
        let back = CManifest::load(&mp).unwrap();
        assert_eq!(back.jobs.len(), 3);
        assert_eq!(back.key, "warm");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reuse_variation_appended() {
        let a = assets();
        let (dir, mut m) = temp_client("reuse", 2);
        m.series = Some("bbsh".into());
        m.reuse_policy = "relaxed".into();
        // 两单同景别同场景（手动指定）→ 第 2 单应被追加换构图句、第 1 单不加
        m.set_job("01", Some("closeup".into()), Some(1), Some("bbsh_field01".into()), &a.scenes).unwrap();
        m.set_job("02", Some("closeup".into()), Some(1), Some("bbsh_field01".into()), &a.scenes).unwrap();
        m.assemble(&a);
        let p1 = m.job("01").unwrap().prompt.clone().unwrap();
        let p2 = m.job("02").unwrap().prompt.clone().unwrap();
        assert!(!p1.contains(VARIATION_TAG), "首个使用者不加换构图句");
        assert!(p2.contains(VARIATION_TAG), "第 2 个使用者应加换构图句");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
