#![allow(dead_code)]
//! Workflow C「人物入景」大脑（移植自同事的 `match_scene.py`）。
//!
//! 职责：读场景目录 `catalog.json` + 场景配置卡 `series_profiles.json` + 双图暖金模板，
//! 按「景别(SHOTS) × 调色(KEY) × 场景概念(配置卡) × plate(comp_prompt)」四轴正交装配出图提示词；
//! 并提供按景别的选景排序 / 自动选景排重（供生产层状态机调用）。
//!
//! 设计纪律（与交接文档 §9 一致）：模板(.md) + catalog.json + series_profiles.json 一律当
//! **外部可热插拔数据**读取，同事更新场景/配置卡时本程序无需改代码。
//!
//! 正确性保障：`cargo test` 的 `prompt_parity` / `rank_parity` 用例与 Python `match_scene.py`
//! 的输出逐字对拍（黄金样本在 assets/wedding/_golden/）。

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 四档景别（由人/视觉 API 判定后传入）。
pub const SHOT_SET: [&str; 4] = ["full", "medium", "close", "closeup"];

// ============ 数据结构（对齐 catalog.json schema v2.1）============

#[derive(Deserialize, Clone, Default)]
pub struct FigureAnchor {
    #[serde(default)]
    pub foot: Option<Vec<f64>>,
    #[serde(default)]
    pub ground_y: Option<f64>,
    #[serde(default)]
    pub max_shot: Option<String>,
}

/// catalog.json 里的一条场景 plate。只声明装配/选景用得到的字段，
/// 其余（light/source/zone/ground_amount/backdrop/depth…）serde 默认忽略。
#[derive(Deserialize, Clone, Default)]
pub struct SceneEntry {
    pub id: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub series: String,
    #[serde(default)]
    pub composition: String,
    #[serde(default)]
    pub has_foreground_path: bool,
    #[serde(default)]
    pub fits_shot: Vec<String>,
    #[serde(default)]
    pub comp_prompt: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub figure_anchor: Option<FigureAnchor>,
    #[serde(default)]
    pub has_fg_elements: bool,
    #[serde(default)]
    pub view_angle: Option<String>,
    #[serde(default)]
    pub lead_dir: Option<String>,
    #[serde(default)]
    pub supports_train: bool,
    #[serde(default)]
    pub mood: Option<String>,
}

#[derive(Deserialize)]
struct Catalog {
    #[serde(default)]
    scenes: Vec<SceneEntry>,
}

/// 看图判定出的人物侧维度（person_attrs），用于「最配优先」选景。
#[derive(Default, Clone)]
pub struct PersonAttrs {
    pub has_train: Option<bool>,
    pub open_space: Option<String>, // left|right|center
    pub view_angle: Option<String>,
    pub mood: Option<String>,
}

/// 装配所需的全部素材（模板 + 场景库 + 配置卡），一次加载、多次装配。
#[derive(Clone)]
pub struct Assets {
    pub tpl: String,
    pub scenes: Vec<SceneEntry>,
    pub profiles: Value,
}

// ============ 加载 ============

/// 统一换行为 \n（Python 文本模式写出的黄金/模板可能是 CRLF；装配只认 \n）。
pub fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

pub fn load_catalog_str(s: &str) -> Result<Vec<SceneEntry>> {
    let c: Catalog = serde_json::from_str(s)?;
    Ok(c.scenes)
}

pub fn load_profiles_str(s: &str) -> Result<Value> {
    Ok(serde_json::from_str(s)?)
}

/// 从一个「场景库目录」加载全部素材：
///   <base>/templates/人物入景_双图暖金版.md
///   <base>/scenes/catalog.json
///   <base>/scenes/series_profiles.json
pub fn load_from_dir(base: &Path) -> Result<Assets> {
    let tpl_path = base.join("templates").join("人物入景_双图暖金版.md");
    let tpl = normalize_newlines(
        &std::fs::read_to_string(&tpl_path)
            .map_err(|e| anyhow!("读模板失败 {}：{e}", tpl_path.display()))?,
    );
    let cat_path = base.join("scenes").join("catalog.json");
    let scenes = load_catalog_str(
        &std::fs::read_to_string(&cat_path)
            .map_err(|e| anyhow!("读 catalog 失败 {}：{e}", cat_path.display()))?,
    )?;
    let prof_path = base.join("scenes").join("series_profiles.json");
    let profiles = load_profiles_str(
        &std::fs::read_to_string(&prof_path)
            .map_err(|e| anyhow!("读配置卡失败 {}：{e}", prof_path.display()))?,
    )?;
    Ok(Assets { tpl, scenes, profiles })
}

// ============ 模板区块解析 ============

/// 取出 `===TAG===\n ... \n===/TAG===` 之间的内容并 trim（对齐 match_scene._section）。
fn section(text: &str, tag: &str) -> Option<String> {
    let open = format!("==={}===", tag);
    let start = text.find(&open)? + open.len();
    // 跳过开标记后到（含）第一个换行
    let nl = text[start..].find('\n')?;
    let content_start = start + nl + 1;
    // 结束标记必须紧跟换行：\n===/TAG===
    let close = format!("\n===/{}===", tag);
    let rel = text[content_start..].find(&close)?;
    let content_end = content_start + rel;
    Some(text[content_start..content_end].trim().to_string())
}

#[derive(Clone)]
struct ShotDef {
    name: String,
    aspect: String,
    visible: String,
    dof: String,
    placement: String,
}

/// 解析 SHOTS 区块：每行 `shot|名称|比例|可见范围|景深句|安置句`。
fn load_shots(text: &str) -> HashMap<String, ShotDef> {
    let mut m = HashMap::new();
    let sec = match section(text, "SHOTS") {
        Some(s) => s,
        None => return m,
    };
    for line in sec.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let p: Vec<&str> = line.split('|').map(|x| x.trim()).collect();
        if p.len() >= 6 {
            m.insert(
                p[0].to_string(),
                ShotDef {
                    name: p[1].to_string(),
                    aspect: p[2].to_string(),
                    visible: p[3].to_string(),
                    dof: p[4].to_string(),
                    placement: p[5].to_string(),
                },
            );
        }
    }
    m
}

/// 调色档别名归一：暖/golden→warm；冷/cool→overcast；自然/写实/原色/real→natural。
pub fn normalize_key(k: &str) -> String {
    match k {
        "golden" | "暖" => "warm",
        "cool" | "冷" => "overcast",
        "自然" | "写实" | "原色" | "real" => "natural",
        other => other,
    }
    .to_string()
}

// ============ 景别层级 + 选景排序（对齐 match_scene）============

fn shot_order(s: &str) -> i32 {
    match s {
        "full" => 0,
        "medium" => 1,
        "close" => 2,
        "closeup" => 3,
        _ => 99,
    }
}

fn widest_shot_idx(e: &SceneEntry) -> i32 {
    e.fits_shot
        .iter()
        .map(|f| shot_order(f))
        .filter(|&i| i != 99)
        .min()
        .unwrap_or(99)
}

/// plate 能否服务该景别：支持的最宽景别 ≤ 请求景别（宽→窄可借，窄→宽不行）。
pub fn can_serve(e: &SceneEntry, shot: &str) -> bool {
    widest_shot_idx(e) <= shot_order(shot)
}

/// 组内细排键：full 优先有前景路径且长路径；medium 优先散花/花海；close/closeup 任意。
fn rank_key(shot: &str, s: &SceneEntry) -> (i32, i32) {
    match shot {
        "full" => (
            if s.has_foreground_path { 0 } else { 1 },
            if s.composition == "long_path_Scurve" || s.composition == "straight_path" {
                0
            } else {
                1
            },
        ),
        "medium" => (
            if s.composition == "scattered_petals" || s.composition == "dense_field" {
                0
            } else {
                1
            },
            0,
        ),
        _ => (0, 0),
    }
}

/// 该景别的排序候选（原生景别优先、借来的宽图排后；组内按 rank_key；同键保 catalog 序——稳定排序）。
pub fn rank_candidates<'a>(
    scenes: &'a [SceneEntry],
    shot: &str,
    series: Option<&str>,
) -> Vec<&'a SceneEntry> {
    let mut v: Vec<&SceneEntry> = scenes
        .iter()
        .filter(|s| series.map_or(true, |ser| s.series == ser))
        .filter(|s| can_serve(s, shot))
        .collect();
    v.sort_by_key(|s| {
        (
            if s.fits_shot.iter().any(|f| f == shot) { 0 } else { 1 },
            rank_key(shot, s),
        )
    });
    v
}

pub fn rank_ids(scenes: &[SceneEntry], shot: &str, series: Option<&str>) -> Vec<String> {
    rank_candidates(scenes, shot, series)
        .into_iter()
        .map(|s| s.id.clone())
        .collect()
}

fn nonempty(o: &Option<String>) -> Option<&str> {
    o.as_deref().filter(|s| !s.is_empty())
}

/// person_attrs ↔ plate 标签契合度（手册§八「最配优先」）：分越高越配。
/// has_train 只在 full 计权（窄景别看不见拖尾）；shot=None 保旧行为。
pub fn attr_score(attrs: Option<&PersonAttrs>, plate: &SceneEntry, shot: Option<&str>) -> i32 {
    let Some(a) = attrs else { return 0 };
    let mut s = 0;
    if a.has_train == Some(true) && (shot.is_none() || shot == Some("full")) {
        s += if plate.supports_train { 2 } else { -2 };
    }
    if let Some(osp) = nonempty(&a.open_space) {
        if osp == "left" || osp == "right" {
            match plate.lead_dir.as_deref() {
                Some(ld) if ld == osp => s += 2,
                Some("center") => s += 1,
                Some("left") | Some("right") => s -= 1,
                _ => {}
            }
        }
    }
    if nonempty(&a.view_angle).is_some() && plate.view_angle.as_deref() == a.view_angle.as_deref() {
        s += 1;
    }
    if nonempty(&a.mood).is_some() && plate.mood.as_deref() == a.mood.as_deref() {
        s += 1;
    }
    s
}

/// 自动选景（手册§八）：最配优先(attr_score) → 排重 → 目录序兜底。
/// `used`：场景 id → 已被「其它单」占用的景别集合。
/// `relaxed`：跨景别复用同一场景视同没用过（只避免同景别重复）。
/// 返回 (场景 id|None, 选择原因)。
pub fn auto_pick_scene(
    scenes: &[SceneEntry],
    shot: &str,
    series: Option<&str>,
    attrs: Option<&PersonAttrs>,
    relaxed: bool,
    used: &HashMap<String, HashSet<String>>,
) -> (Option<String>, String) {
    let cands = rank_ids(scenes, shot, series);
    if cands.is_empty() {
        return (None, "无候选".to_string());
    }
    let cat: HashMap<&str, &SceneEntry> = scenes.iter().map(|s| (s.id.as_str(), s)).collect();
    let base: HashMap<&str, i32> = cands
        .iter()
        .enumerate()
        .map(|(i, c)| (c.as_str(), i as i32))
        .collect();
    let reuse_tier = |c: &str| -> i32 {
        match used.get(c) {
            None => 0,
            Some(shots) => {
                if !shots.contains(shot) {
                    if relaxed { 0 } else { 1 }
                } else {
                    2
                }
            }
        }
    };
    let score = |c: &str| -> i32 {
        cat.get(c).map_or(0, |e| attr_score(attrs, e, Some(shot)))
    };
    let mut ordered = cands.clone();
    ordered.sort_by_key(|c| (-score(c), reuse_tier(c), base[c.as_str()]));
    let pick = ordered[0].clone();
    let mut why = match used.get(&pick) {
        None => "排重·未用过".to_string(),
        Some(shots) => {
            if !shots.contains(shot) {
                if relaxed {
                    "复用·跨景别(relaxed允许)".to_string()
                } else {
                    "复用·已换景别".to_string()
                }
            } else {
                "⚠️候选耗尽·同景别复用，成册请隔开".to_string()
            }
        }
    };
    if attrs.is_some() {
        why = format!("{why}·契合分{}", score(&pick));
    }
    (Some(pick), why)
}

// ============ 提示词装配（双图暖金版 --ref）============

/// 【⑤b 人景比例·透视】块的四个子句（对齐 match_scene.proportion_clauses）。
fn proportion_clauses(
    chosen: &SceneEntry,
    shot: &str,
    occlusion_fg: &str,
) -> [(&'static str, String); 4] {
    let fa = chosen.figure_anchor.clone().unwrap_or_default();
    let foot = fa.foot.clone().unwrap_or_else(|| vec![0.5, 0.9]);
    let fx = foot.first().copied().unwrap_or(0.5);
    let gy = fa.ground_y.unwrap_or_else(|| foot.get(1).copied().unwrap_or(0.9));
    let xside = if fx < 0.4 {
        "偏左"
    } else if fx > 0.6 {
        "偏右"
    } else {
        "居中"
    };
    let yband = if gy > 0.66 {
        "下三分之一"
    } else if gy > 0.45 {
        "中部偏下"
    } else {
        "中部"
    };
    let anchor = format!(
        "把人物安置在画面{xside}的可信站立区、脚部／裙摆接地线落在画面{yband}、与地面有合理接触并落下柔和接地阴影"
    );
    let va = chosen.view_angle.as_deref().unwrap_or("eye");
    let horizon = match va {
        "eye" => "机位平视、视平线约在画面中部，近大远小自然真实",
        "high" => "机位略带俯视（与图B一致）、地面占画面主体，人物从略高角度被自然收入、不变形",
        "low" => "机位略带仰视、视平线偏低，仰角不夸张",
        _ => "机位平视、视平线约在画面中部",
    }
    .to_string();
    let scale = match shot {
        "full" => "全身完整入镜、含完整裙摆／拖尾，人物约占画面高度 70–85%",
        "medium" => "约腰／胯以上、脚不入镜，人物约占画面高度 60–75%",
        "close" => "胸部以上、头肩胸占画面主体",
        "closeup" => "脸部与肩颈紧凑特写",
        _ => "按景别合理占比",
    }
    .to_string();
    let occ = chosen.has_fg_elements
        && !occlusion_fg.trim().is_empty()
        && (shot == "full" || shot == "medium");
    let occlusion = if occ {
        format!("让前景的{occlusion_fg}自然遮挡裙摆最下缘，增强真实前后层次与“踩进场景”的实拍感")
    } else {
        "前景无需额外遮挡物，保持人物脚下干净自然".to_string()
    };
    [
        ("{ANCHOR_CLAUSE}", anchor),
        ("{HORIZON_CLAUSE}", horizon),
        ("{SCALE_CLAUSE}", scale),
        ("{OCCLUSION_CLAUSE}", occlusion),
    ]
}

// 模板里 13 个场景占位符 ← 配置卡字段（四轴拆开落地）。
const SCENE_FILL: [(&str, &str); 13] = [
    ("{SCENE_LABEL}", "scene_label"),
    ("{SCENE_STRUCTURE}", "scene_structure"),
    ("{GROUND_FEAT}", "ground_feat"),
    ("{GROUND_FEAT2}", "ground_feat2"),
    ("{COLOR_ANCHOR}", "color_anchor"),
    ("{SCENE_AREA}", "scene_area"),
    ("{SCENE_ELEMENTS}", "scene_elements"),
    ("{OVERCAST_COLOR}", "overcast_color"),
    ("{OVERCAST_COLOR2}", "overcast_color2"),
    ("{DOF_SCENE_FULL}", "dof_scene_full"),
    ("{DOF_SCENE_MEDIUM}", "dof_scene_medium"),
    ("{PLACEMENT_FULL}", "placement_full"),
    ("{PLACEMENT_MEDIUM}", "placement_medium"),
];

/// 装配双图暖金提示词（图A=人物、图B=plate）。`key` 接受别名（内部归一）。
/// 与 Python `match_scene.py --ref` 逐字一致（见 prompt_parity 测试）。
pub fn assemble_prompt(
    a: &Assets,
    chosen: &SceneEntry,
    shot: &str,
    key: &str,
    subjects: u8,
) -> Result<String> {
    let key = normalize_key(key);
    if subjects != 1 && subjects != 2 {
        bail!("subjects 必须是 1 或 2");
    }
    if chosen.series.is_empty() {
        bail!("场景 {} 没有 series 字段", chosen.id);
    }
    let profile = a
        .profiles
        .get(&chosen.series)
        .filter(|v| v.is_object())
        .ok_or_else(|| {
            anyhow!(
                "找不到 series「{}」的场景配置卡（series_profiles.json）；新概念需先加一条配置卡。",
                chosen.series
            )
        })?;

    let skeleton = section(&a.tpl, "SKELETON").ok_or_else(|| anyhow!("模板缺 ===SKELETON==="))?;
    let key_block = section(&a.tpl, &format!("KEY:{key}"))
        .ok_or_else(|| anyhow!("模板缺 ===KEY:{key}===（仅 warm/overcast/natural）"))?;
    let shots = load_shots(&a.tpl);
    let sd = shots
        .get(shot)
        .ok_or_else(|| anyhow!("模板 SHOTS 里没有景别 {shot}"))?;
    let comp = chosen.comp_prompt.trim();

    // 第一段：景别/比例/可见范围/景深句/安置句/场景构图/基调块
    let mut p = skeleton
        .replace("{SHOT_NAME}", &sd.name)
        .replace("{ASPECT}", &sd.aspect)
        .replace("{VISIBLE_RANGE}", &sd.visible)
        .replace("{DOF_CLAUSE}", &sd.dof)
        .replace("{PLACEMENT_CLAUSE}", &sd.placement)
        .replace("{SCENE_COMP}", comp)
        .replace("{KEY_BLOCK}", &key_block);

    // 人数相关块
    let subjdesc = section(&a.tpl, &format!("SUBJDESC:{subjects}"))
        .ok_or_else(|| anyhow!("模板缺 SUBJDESC:{subjects}"))?;
    let subjnoun = section(&a.tpl, &format!("SUBJNOUN:{subjects}"))
        .ok_or_else(|| anyhow!("模板缺 SUBJNOUN:{subjects}"))?;
    let subjblock = section(&a.tpl, &format!("SUBJBLOCK:{subjects}"))
        .ok_or_else(|| anyhow!("模板缺 SUBJBLOCK:{subjects}"))?;
    p = p
        .replace("{SUBJECTS_DESC}", &subjdesc)
        .replace("{SUBJECTS_NOUN}", &subjnoun)
        .replace("{SUBJECTS_BLOCK}", &subjblock);

    // 【⑤b 人景比例】子句（occlusion 名词由配置卡注入，空→省略遮挡句）
    let occ_fg = profile
        .get("occlusion_fg")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    for (k, v) in proportion_clauses(chosen, shot, occ_fg) {
        p = p.replace(k, &v);
    }

    // 四轴拆开：13 个场景占位符 ← 配置卡（仅当字段存在时替换）
    for (ph, field) in SCENE_FILL {
        if let Some(val) = profile.get(field).and_then(|v| v.as_str()) {
            p = p.replace(ph, val);
        }
    }

    Ok(p)
}

/// 双图上传要带的 plate 相对路径（scenes/<file>）。
pub fn plate_rel_path(chosen: &SceneEntry) -> String {
    let file = if chosen.file.is_empty() {
        format!("{}.jpg", chosen.id)
    } else {
        chosen.file.clone()
    };
    format!("scenes/{file}")
}

/// 按原片宽高比选 4sapi gpt-image-2 的出图尺寸（错比例会被强制重构图+漂脸）。
pub fn size_for_aspect(w: u32, h: u32) -> &'static str {
    if w == 0 || h == 0 {
        return "1024x1536";
    }
    let r = w as f64 / h as f64;
    if r > 1.15 {
        "1536x1024" // 横
    } else if r < 0.87 {
        "1024x1536" // 竖
    } else {
        "1024x1024" // 方
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/wedding")
    }
    fn assets() -> Assets {
        load_from_dir(&base()).expect("加载 assets/wedding 失败")
    }
    fn norm(s: &str) -> String {
        s.replace("\r\n", "\n").trim_end().to_string()
    }
    fn find<'a>(a: &'a Assets, id: &str) -> &'a SceneEntry {
        a.scenes.iter().find(|s| s.id == id).expect("scene 不存在")
    }

    #[test]
    fn prompt_parity_with_python() {
        // 与 Python match_scene.py --ref 输出逐字对拍（覆盖 3 基调 × 4 景别 × 单/双 × 两 series）。
        let a = assets();
        let cases = [
            ("g1_full_warm_2_bbshopen01.md", "full", "warm", 2u8, "bbsh_open01"),
            ("g2_full_natural_1_bbshopen01.md", "full", "natural", 1, "bbsh_open01"),
            ("g3_medium_overcast_2_scatter05.md", "medium", "overcast", 2, "bbsh_scatter05"),
            ("g4_closeup_natural_1_field01.md", "closeup", "natural", 1, "bbsh_field01"),
            ("g5_full_warm_2_jiaoshi01.md", "full", "warm", 2, "jiaoshi_01"),
            ("g6_medium_natural_2_jiaoshi06.md", "medium", "natural", 2, "jiaoshi_06"),
        ];
        for (file, shot, key, subj, scene) in cases {
            let got = assemble_prompt(&a, find(&a, scene), shot, key, subj).unwrap();
            let want = std::fs::read_to_string(base().join("_golden").join(file)).unwrap();
            assert_eq!(norm(&got), norm(&want), "装配与 Python 黄金不一致：{file}");
        }
    }

    #[test]
    fn rank_parity_with_python() {
        let a = assets();
        let cases: [(&str, &str, Option<&str>); 4] = [
            ("rank_full_all.txt", "full", None),
            ("rank_medium_bbsh.txt", "medium", Some("bbsh")),
            ("rank_closeup_bbsh.txt", "closeup", Some("bbsh")),
            ("rank_full_jiaoshi.txt", "full", Some("jiaoshi")),
        ];
        for (file, shot, series) in cases {
            let got = rank_ids(&a.scenes, shot, series);
            let raw = std::fs::read_to_string(base().join("_golden").join(file)).unwrap();
            let want: Vec<String> = raw
                .trim_start_matches('\u{feff}')
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            assert_eq!(got, want, "排序与 Python 不一致：{file}");
        }
    }

    #[test]
    fn auto_pick_reuse_logic() {
        let a = assets();
        let mut used: HashMap<String, HashSet<String>> = HashMap::new();
        // 没用过 → full 排序第一 = bbsh_open01
        let (pick, _) = auto_pick_scene(&a.scenes, "full", Some("bbsh"), None, false, &used);
        assert_eq!(pick.as_deref(), Some("bbsh_open01"));
        // open01 被另一单同景别占用 → distinct 下应跳到 open02
        used.insert("bbsh_open01".into(), HashSet::from(["full".to_string()]));
        let (pick2, _) = auto_pick_scene(&a.scenes, "full", Some("bbsh"), None, false, &used);
        assert_eq!(pick2.as_deref(), Some("bbsh_open02"));
    }

    #[test]
    fn attr_score_open_space_and_train() {
        let a = assets();
        // jiaoshi_03 lead_dir=right、supports_train=true
        let j03 = find(&a, "jiaoshi_03");
        let attrs = PersonAttrs {
            has_train: Some(true),
            open_space: Some("right".into()),
            ..Default::default()
        };
        // full：拖尾计权(+2) + open_space right==lead_dir right(+2) = 4
        assert_eq!(attr_score(Some(&attrs), j03, Some("full")), 4);
        // medium：拖尾不计权（看不见），只剩 open_space +2
        assert_eq!(attr_score(Some(&attrs), j03, Some("medium")), 2);
    }

    #[test]
    fn key_aliases_normalize() {
        assert_eq!(normalize_key("暖"), "warm");
        assert_eq!(normalize_key("自然"), "natural");
        assert_eq!(normalize_key("冷"), "overcast");
        assert_eq!(normalize_key("warm"), "warm");
    }

    #[test]
    fn size_by_aspect() {
        assert_eq!(size_for_aspect(2000, 3000), "1024x1536"); // 竖
        assert_eq!(size_for_aspect(3000, 2000), "1536x1024"); // 横
        assert_eq!(size_for_aspect(2000, 2000), "1024x1024"); // 方
    }

    #[test]
    fn catalog_integrity() {
        // 移植 selftest ③：id 唯一 / plate 文件在 / fits_shot 合法 / comp_prompt 非空且无光照天气词 / figure_anchor 数值。
        let a = assets();
        let b = base();
        const LIGHT: [&str; 10] = ["光", "阳", "夕", "晨", "霞", "雾", "黄昏", "日落", "晴天", "阴天"];
        let mut ids = std::collections::HashSet::new();
        for s in &a.scenes {
            assert!(ids.insert(s.id.clone()), "场景 id 重复：{}", s.id);
            let file = if s.file.is_empty() { format!("{}.jpg", s.id) } else { s.file.clone() };
            assert!(b.join("scenes").join(&file).exists(), "plate 缺失：{file}");
            assert!(!s.fits_shot.is_empty(), "{}: fits_shot 为空", s.id);
            for f in &s.fits_shot {
                assert!(SHOT_SET.contains(&f.as_str()), "{}: fits_shot 非法 {f}", s.id);
            }
            assert!(!s.comp_prompt.trim().is_empty(), "{}: comp_prompt 为空", s.id);
            for w in LIGHT {
                assert!(!s.comp_prompt.contains(w), "{}: comp_prompt 残留光照/天气词「{w}」", s.id);
            }
            assert!(!s.series.is_empty(), "{}: 缺 series", s.id);
            if let Some(fa) = &s.figure_anchor {
                let mut nums = fa.foot.clone().unwrap_or_default();
                if let Some(g) = fa.ground_y {
                    nums.push(g);
                }
                for v in nums {
                    assert!((0.0..=1.0).contains(&v), "{}: figure_anchor 数值越界 {v}", s.id);
                }
            }
        }
        assert_eq!(a.scenes.len(), 18, "当前应有 18 张 plate");
    }

    #[test]
    fn decouple_beach_probe() {
        // 移植 selftest ④b：四轴拆开——用临时 beach 配置卡装配，场景词必须全来自配置卡。
        // 反向：不得含草坪词；正向：必须含注入的沙滩/海（防占位符填空→假绿）。
        let real = assets();
        let beach = SceneEntry {
            id: "_probe01".into(),
            file: "_probe01.jpg".into(),
            series: "_probe".into(),
            composition: "beach".into(),
            has_foreground_path: false,
            fits_shot: vec!["full".into(), "medium".into(), "close".into(), "closeup".into()],
            comp_prompt: "开阔海滩、湿润沙面与海平线、礁石点缀".into(),
            has_fg_elements: true,
            view_angle: Some("eye".into()),
            figure_anchor: Some(FigureAnchor {
                foot: Some(vec![0.5, 0.9]),
                ground_y: Some(0.9),
                max_shot: Some("full".into()),
            }),
            ..Default::default()
        };
        let profiles = serde_json::json!({ "_probe": {
            "scene_label": "黄昏海滩外景", "scene_structure": "海平线、沙滩纹理、礁石与浪花的空间结构",
            "ground_feat": "沙滩/浪线", "ground_feat2": "沙滩或礁石", "color_anchor": "暖金沙色与海蓝",
            "scene_area": "沙滩", "scene_elements": "沙滩与浪花", "overcast_color": "沙滩的清新固有色",
            "overcast_color2": "海色与天光", "dof_scene_full": "海面与远景", "dof_scene_medium": "沙滩海浪",
            "placement_full": "让人物站在湿润沙滩上、拖尾铺在沙面、身后是开阔海平线",
            "placement_medium": "让人物处在沙滩海浪环境中、画面下半部由沙滩与浪花柔和填充",
            "occlusion_fg": "浪花泡沫", "default_key": "warm"
        }});
        let a = Assets { tpl: real.tpl, scenes: vec![beach.clone()], profiles };
        let grass = ["草地", "草坪", "花瓣", "花径", "花丛", "三叶草"];
        let beach_words = ["沙滩", "海"];
        let mut combined = String::new();
        for (shot, key) in [("full", "natural"), ("full", "warm"), ("full", "overcast"), ("medium", "natural")] {
            combined.push_str(&assemble_prompt(&a, &beach, shot, key, 2).unwrap());
            combined.push('\n');
        }
        for w in grass {
            assert!(!combined.contains(w), "四轴解耦失败：沙滩探针仍含草坪词「{w}」");
        }
        for w in beach_words {
            assert!(combined.contains(w), "四轴解耦失败：沙滩探针缺注入词「{w}」");
        }
    }
}
