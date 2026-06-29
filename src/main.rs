// 视觉海岸批量处理工作台
// —— RunningHub 工作流批量自动化（上传 → 提交 → 轮询 → 下载）的 Windows 桌面程序。
// 功能对齐命令行脚本 rh_batch.py，并提供：分图进度阶段、处理前/后对比预览、实时日志、
// 断点续跑、并发与重试、账户状态、处理清单 _manifest.csv、汇总统计。
//
// 发布版隐藏控制台黑窗；debug 版保留控制台便于看输出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(non_snake_case)]

mod api;
mod cmode;
mod cstate;
mod foursapi;
mod workflow;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{AccountInfo, CreateOutcome, PollState, RhClient, RhSettings};

const APP_TITLE: &str = "视觉海岸批量处理工作台";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION"); // 版本号（取自 Cargo.toml，迭代时改那里）
const CONFIG_FILE: &str = "vc_batch_config.json";
const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "bmp"];
const PREVIEW_MAX: u32 = 8192; // 预览大图最长边像素（再按 GPU 纹理上限夹紧）；放大对比需要全分辨率才不发虚
const LIST_THUMB_MAX: u32 = 96; // 列表小缩略图最长边像素

// ============ 现代浅色配色（海岸青绿强调，高级浅色）============
mod pal {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(239, 242, 246); // 画布（浅冷灰）
    pub const PANEL: Color32 = Color32::from_rgb(246, 248, 250); // 面板（近白）
    pub const SURFACE: Color32 = Color32::from_rgb(255, 255, 255); // 卡片（白）
    pub const SURFACE_HI: Color32 = Color32::from_rgb(234, 238, 243); // 悬停
    pub const FIELD: Color32 = Color32::from_rgb(242, 245, 248); // 输入框底
    pub const BTN: Color32 = Color32::from_rgb(237, 240, 245); // 次按钮底
    pub const BTN_HI: Color32 = Color32::from_rgb(226, 232, 240);
    pub const STROKE: Color32 = Color32::from_rgb(224, 229, 236); // 细边框（卡片/面板）
    pub const STROKE_HI: Color32 = Color32::from_rgb(190, 198, 209); // 输入框/按钮静态边框
    pub const STROKE_STRONG: Color32 = Color32::from_rgb(150, 160, 174); // 悬停/聚焦更明显的边框
    pub const TEXT: Color32 = Color32::from_rgb(30, 41, 59); // slate-800
    pub const TEXT_WEAK: Color32 = Color32::from_rgb(100, 116, 139); // slate-500
    pub const ACCENT: Color32 = Color32::from_rgb(13, 148, 136); // teal-600
    pub const ACCENT_DK: Color32 = Color32::from_rgb(15, 118, 110); // teal-700（按下）
    pub const SUCCESS: Color32 = Color32::from_rgb(22, 163, 74); // green-600
    pub const WARN: Color32 = Color32::from_rgb(202, 138, 4); // amber-600
    pub const ERROR: Color32 = Color32::from_rgb(220, 38, 38); // red-600
    pub const INFO: Color32 = Color32::from_rgb(2, 132, 199); // sky-600
    pub const ON_ACCENT: Color32 = Color32::from_rgb(255, 255, 255); // 强调色上的文字
}

// ============ 可持久化的界面设置（保存上次填写）============
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct UiConfig {
    api_key: String,
    workflow_id: String,
    input_dir: String,
    output_dir: String,
    workflow_json: String,
    concurrency: usize,
    skip_processed: bool, // true=断点续跑(跳过已有结果)；false=全部重新处理

    // —— 高级设置 ——
    input_node_override: String,
    output_node_override: String,
    input_field: String,
    extra_overrides: String,
    add_metadata: bool,
    poll_interval: u64,
    task_timeout: u64,
    create_retry: u32,
    create_retry_wait: u64,
    net_retry: u32,

    // —— 收尾与预警 ——
    auto_open_output: bool, // 跑完自动打开输出文件夹
    notify_sound: bool,     // 跑完播放提示音
    coins_per_image: f64,   // 每张预估消耗（>0 时用于跑前余额预警）
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            workflow_id: String::new(),
            input_dir: String::new(),
            output_dir: String::new(),
            workflow_json: String::new(),
            concurrency: 2,
            skip_processed: true,

            input_node_override: String::new(),
            output_node_override: String::new(),
            input_field: "image".into(),
            extra_overrides: String::new(),
            add_metadata: true,
            poll_interval: 5,
            task_timeout: 1800,
            create_retry: 20,
            create_retry_wait: 15,
            net_retry: 4,

            auto_open_output: false,
            notify_sound: true,
            coins_per_image: 0.0,
        }
    }
}

// ============ 配置预设 + 窗口布局（持久化的整体存档）============
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Preset {
    name: String,
    cfg: UiConfig,
}
impl Default for Preset {
    fn default() -> Self {
        Self {
            name: "默认".into(),
            cfg: UiConfig::default(),
        }
    }
}

// 人物入景（C 模式）持久化配置：场景库路径（局域网共享）+ 常用选项，设一次记住。
// 凭据（sk-/系统令牌/用户ID）不存这里，走 key.txt。
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct WeddingCfg {
    scene_dir: String,
    output_dir: String,
    tone: String,
    series: String,
    concurrency: usize,
    skip_done: bool,
}
impl Default for WeddingCfg {
    fn default() -> Self {
        Self {
            scene_dir: String::new(),
            output_dir: String::new(),
            tone: "natural".into(),
            series: String::new(),
            concurrency: 2,
            skip_done: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Store {
    presets: Vec<Preset>,
    current: usize,
    win_w: f32,
    win_h: f32,
    left_w: f32,
    logs_h: f32,
    #[serde(default = "default_win_scale")]
    win_scale: f32, // 界面缩放（与系统 DPI 无关，跨机一致；用户可 ± 调）
    wedding: WeddingCfg,
}
fn default_win_scale() -> f32 {
    1.0
}
impl Default for Store {
    fn default() -> Self {
        Self {
            presets: vec![Preset::default()],
            current: 0,
            win_w: 1180.0,
            win_h: 800.0,
            left_w: 340.0,
            logs_h: 140.0,
            win_scale: 1.0,
            wedding: WeddingCfg::default(),
        }
    }
}
impl Store {
    fn normalized(mut self) -> Self {
        if self.presets.is_empty() {
            self.presets.push(Preset::default());
        }
        if self.current >= self.presets.len() {
            self.current = self.presets.len() - 1;
        }
        if !(self.win_w.is_finite() && self.win_w >= 640.0) {
            self.win_w = 1180.0;
        }
        if !(self.win_h.is_finite() && self.win_h >= 480.0) {
            self.win_h = 800.0;
        }
        if !(self.left_w.is_finite() && self.left_w >= 160.0) {
            self.left_w = 340.0;
        }
        if !(self.logs_h.is_finite() && self.logs_h >= 60.0) {
            self.logs_h = 140.0;
        }
        if !(self.win_scale.is_finite() && (0.7..=2.0).contains(&self.win_scale)) {
            self.win_scale = 1.0;
        }
        self
    }
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(CONFIG_FILE)))
        .unwrap_or_else(|| PathBuf::from(CONFIG_FILE))
}
fn parse_store(txt: &str) -> Store {
    // 用是否“真的带 presets 字段”来区分新旧格式：
    // 注意不能只看 from_str::<Store> 是否成功——因为 #[serde(default)] 会把缺失的
    // presets 用 Store::default()（含一个默认预设）补上，导致旧配置被误判为新格式而丢失。
    let has_presets = serde_json::from_str::<serde_json::Value>(txt)
        .ok()
        .and_then(|v| v.get("presets").and_then(|p| p.as_array()).map(|a| !a.is_empty()))
        .unwrap_or(false);
    if has_presets {
        if let Ok(s) = serde_json::from_str::<Store>(txt) {
            return s.normalized();
        }
    }
    // 兼容旧格式：裸 UiConfig，包成单个预设。
    if let Ok(cfg) = serde_json::from_str::<UiConfig>(txt) {
        return Store {
            presets: vec![Preset {
                name: "默认".into(),
                cfg,
            }],
            ..Default::default()
        }
        .normalized();
    }
    Store::default()
}
fn load_store() -> Store {
    parse_store(&std::fs::read_to_string(config_path()).unwrap_or_default())
}
fn save_store(s: &Store) {
    if let Ok(txt) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(config_path(), txt);
    }
}

// ============ 处理阶段 ============
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Queued,
    Uploading,
    Submitting,
    Waiting,
    Downloading,
    Done,
    Skipped,
    Failed,
}
impl Stage {
    fn label(self) -> &'static str {
        match self {
            Stage::Queued => "等待中",
            Stage::Uploading => "上传中",
            Stage::Submitting => "提交中",
            Stage::Waiting => "生成中",
            Stage::Downloading => "下载中",
            Stage::Done => "已完成",
            Stage::Skipped => "已跳过",
            Stage::Failed => "失败",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Stage::Queued => "•",
            Stage::Uploading => "⬆",
            Stage::Submitting => "✈",
            Stage::Waiting => "⏳",
            Stage::Downloading => "⬇",
            Stage::Done => "✓",
            Stage::Skipped => "⏭",
            Stage::Failed => "✗",
        }
    }
    fn color(self) -> egui::Color32 {
        match self {
            Stage::Queued => pal::TEXT_WEAK,
            Stage::Uploading | Stage::Submitting | Stage::Waiting | Stage::Downloading => pal::INFO,
            Stage::Done => pal::SUCCESS,
            Stage::Skipped => pal::WARN,
            Stage::Failed => pal::ERROR,
        }
    }
    fn is_active(self) -> bool {
        matches!(
            self,
            Stage::Uploading | Stage::Submitting | Stage::Waiting | Stage::Downloading
        )
    }
}

// 列表状态筛选
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListFilter {
    All,
    Active,
    Ok,
    Skipped,
    Failed,
}
impl ListFilter {
    fn label(self) -> &'static str {
        match self {
            ListFilter::All => "全部",
            ListFilter::Active => "进行中",
            ListFilter::Ok => "成功",
            ListFilter::Skipped => "跳过",
            ListFilter::Failed => "失败",
        }
    }
    fn matches(self, st: Stage) -> bool {
        match self {
            ListFilter::All => true,
            ListFilter::Active => st.is_active() || st == Stage::Queued,
            ListFilter::Ok => st == Stage::Done,
            ListFilter::Skipped => st == Stage::Skipped,
            ListFilter::Failed => st == Stage::Failed,
        }
    }
}

// 日志级别过滤
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogFilter {
    All,
    Info,
    Success,
    Warn,
    Error,
}
impl LogFilter {
    fn label(self) -> &'static str {
        match self {
            LogFilter::All => "全部",
            LogFilter::Info => "信息",
            LogFilter::Success => "成功",
            LogFilter::Warn => "警告",
            LogFilter::Error => "错误",
        }
    }
}

// 对比模式
#[derive(Clone, Copy, PartialEq, Eq)]
enum CmpMode {
    Side, // 并排
    Wipe, // 滑动对比
}

#[derive(Clone, Copy, PartialEq)]
enum Which {
    Input,
    Output,
    List,
}
impl Which {
    fn key(self) -> &'static str {
        match self {
            Which::Input => "in",
            Which::Output => "out",
            Which::List => "list",
        }
    }
}

// 工作台模式：RunningHub 精修/放大 ｜ 人物入景（Workflow C）
#[derive(Clone, Copy, PartialEq, Eq)]
enum AppMode {
    RunningHub,
    Wedding,
}

// 人物入景列表筛选（对齐 RunningHub 的 全部/进行中/成功/跳过/失败）
#[derive(Clone, Copy, PartialEq, Eq)]
enum WListFilter {
    All,
    Active,
    Ok,
    Skipped,
    Failed,
}
impl WListFilter {
    fn label(self) -> &'static str {
        match self {
            WListFilter::All => "全部",
            WListFilter::Active => "进行中",
            WListFilter::Ok => "成功",
            WListFilter::Skipped => "跳过",
            WListFilter::Failed => "失败",
        }
    }
    fn matches(self, st: cstate::CStatus) -> bool {
        use cstate::CStatus::*;
        match self {
            WListFilter::All => true,
            WListFilter::Active => matches!(st, Pending | Prompted | Generating),
            WListFilter::Ok => matches!(st, AwaitingQc | Selected | Done),
            WListFilter::Skipped => st == Skipped,
            WListFilter::Failed => matches!(st, QcFail | Failed),
        }
    }
}

// 局域网共享场景库（不再随程序打包；主机维护、全员共用）。同事程序默认从这里读。
const SHARED_SCENE_DIR: &str = r"\\DESKTOP-66773HC\ai_work\wedding_scene_lib";

// 默认场景库目录：① exe 同目录捆绑的 assets/wedding（若存在，开发/便携用）；② 否则用局域网共享。
// 实际值会被持久化的配置覆盖（用户在界面改过就记住）。
fn default_scene_dir() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let c = d.join("assets").join("wedding");
            if c.join("scenes").join("catalog.json").exists() {
                return c.display().to_string();
            }
        }
    }
    SHARED_SCENE_DIR.to_string()
}

// 单选/多选图片对话框（支持的图片扩展名）。
fn pick_image_files() -> Vec<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("图片", &IMAGE_EXTS)
        .pick_files()
        .unwrap_or_default()
}
fn is_image_file(p: &Path) -> bool {
    p.is_file()
        && p.extension()
            .and_then(|s| s.to_str())
            .map(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
}

// ============ 后台→UI 的消息 ============
enum Msg {
    Files(Vec<(String, PathBuf)>),
    Stage {
        idx: usize,
        stage: Stage,
        detail: String,
    },
    Outputs {
        idx: usize,
        paths: Vec<PathBuf>,
        task_id: String,
    },
    Log(String),
    Thumb {
        idx: usize,
        which: Which,
        w: usize,
        h: usize,
        rgba: Vec<u8>,
    },
    Account(AccountInfo),
    AccountErr(String),
    Finished,

    // —— 人物入景（C 模式）——
    WOutput { no: String, path: PathBuf }, // 某单出图完成，落地路径
    WJob {
        // 自动流水线对某单的字段更新（只更新非 None 项 + 状态）
        no: String,
        shot: Option<String>,
        subjects: Option<u8>,
        scene: Option<String>,
        scene_file: Option<String>,
        output: Option<String>,
        status: cstate::CStatus,
    },
    WThumb { job: usize, is_out: bool, w: usize, h: usize, rgba: Vec<u8> }, // 选中单的原片/结果预览
    WAccount(foursapi::FsAccount),
    WAccountErr(String),
    WFinished,
}

// ============ 单张图片状态（UI 侧）============
struct Item {
    name: String,
    input: PathBuf,
    stage: Stage,
    detail: String,
    outputs: Vec<PathBuf>,
    task_id: String,
    in_tex: Option<egui::TextureHandle>,
    out_tex: Option<egui::TextureHandle>,
    list_tex: Option<egui::TextureHandle>,
    in_req: bool,
    out_req: bool,
    list_req: bool,
}

// ============ 运行模式 ============
#[derive(Clone)]
enum RunMode {
    Full { extra: Vec<PathBuf> }, // 读输入目录 + 追加拖入的文件
    Subset(Vec<(usize, PathBuf)>), // 仅重跑指定 UI 索引（失败重试 / 重跑选中）
}

// ============ 跑批配置快照 ============
#[derive(Clone)]
struct BatchConfig {
    cfg: UiConfig,
    input_node: String,
    output_node: Option<String>,
    extra_overrides: Vec<serde_json::Value>,
    mode: RunMode,
}

// ============ 应用状态 ============
struct App {
    store: Store,
    cfg: UiConfig, // 当前预设的工作副本，表单直接绑定它
    detected_in: String,
    detected_out: String,
    running: bool,
    items: Vec<Item>,
    selected: Option<usize>,
    follow: bool,
    show_settings: bool,
    logs: Vec<String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    stop: Arc<AtomicBool>,

    // 账户
    account: Option<AccountInfo>,
    account_err: Option<String>,
    account_loading: bool,

    // 拖拽追加的额外图片（本会话有效，不持久化）
    extra_files: Vec<PathBuf>,

    // 工作流参数（P1.8）
    parsed_wf: String,                       // 上次解析过的工作流路径，用于检测变化
    detected_params: Vec<workflow::DetectedParam>,
    param_on: Vec<bool>,                     // 是否覆盖
    param_val: Vec<String>,                  // 编辑中的值（字符串）

    // 列表与日志过滤
    list_filter: ListFilter,
    log_filter: LogFilter,
    log_search: String,

    // 对比查看器状态
    cmp_mode: CmpMode,
    cmp_wipe: f32,
    cmp_zoom: f32,
    cmp_offset: egui::Vec2,
    cmp_drag_wipe: bool,
    cmp_fit_w: f32, // 上一帧 zoom=1 时的显示宽，用于 1:1 计算

    // 完成横幅
    finished_banner: bool,

    // 主题/字体是否已应用到当前 ctx（首帧懒应用，便于 headless 测试）
    themed: bool,
    // 进度条平滑动画的当前显示值（视觉用，非真实进度）
    prog_anim: f32,

    // —— 人物入景（C 模式）——
    mode: AppMode,
    w_4sapi_key: String,
    w_access_token: String, // 4sapi 系统访问令牌（查额度用，非 sk-）
    w_user_id: String,      // 可选 New-Api-User
    w_account: Option<foursapi::FsAccount>,
    w_account_err: Option<String>,
    w_account_loading: bool,
    w_input_dir: String,
    w_input_files: Vec<PathBuf>, // 单独选/拖入的图片（与文件夹叠加）
    w_output_dir: String,
    w_tone: String,      // 调色 natural|warm|overcast
    w_series: String,    // 场景概念池（空=全部）
    w_scene_dir: String, // 场景库目录（含 templates/ scenes/）
    w_concurrency: usize, // 并发出图数（1-8）
    w_skip_done: bool,    // 断点续跑：跳过已有结果
    w_list_filter: WListFilter,
    w_assets: Option<cmode::Assets>,
    w_assets_err: Option<String>,
    w_manifest: Option<cstate::CManifest>,
    w_sel: Option<usize>, // 选中的 job 下标
    w_running: bool,
    w_stop: Arc<AtomicBool>,
    w_in_tex: Option<egui::TextureHandle>,
    w_out_tex: Option<egui::TextureHandle>,
    w_tex_job: Option<usize>, // 当前预览纹理属于哪个 job
    w_in_req: bool,
    w_out_req: bool,
}

impl App {
    // 只建结构体（不碰 egui ctx、不联网），供正式启动与 headless 测试共用。
    fn build() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let store = load_store();
        let cfg = store.presets[store.current].cfg.clone();
        let (w_key, w_tok, w_uid) = foursapi::read_credentials();
        let wc = store.wedding.clone();
        let w_scene_dir = if wc.scene_dir.trim().is_empty() {
            default_scene_dir()
        } else {
            wc.scene_dir.clone()
        };
        let w_tone = if wc.tone.trim().is_empty() { "natural".to_string() } else { wc.tone.clone() };
        let w_output_dir = wc.output_dir.clone();
        let w_series = wc.series.clone();
        let w_concurrency = wc.concurrency.clamp(1, 8);
        let w_skip_done = wc.skip_done;
        Self {
            store,
            cfg,
            detected_in: String::new(),
            detected_out: String::new(),
            running: false,
            items: Vec::new(),
            selected: None,
            follow: true,
            show_settings: true,
            logs: Vec::new(),
            tx,
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            account: None,
            account_err: None,
            account_loading: false,
            extra_files: Vec::new(),
            parsed_wf: String::new(),
            detected_params: Vec::new(),
            param_on: Vec::new(),
            param_val: Vec::new(),
            list_filter: ListFilter::All,
            log_filter: LogFilter::All,
            log_search: String::new(),
            cmp_mode: CmpMode::Side,
            cmp_wipe: 0.5,
            cmp_zoom: 1.0,
            cmp_offset: egui::Vec2::ZERO,
            cmp_drag_wipe: false,
            cmp_fit_w: 1.0,
            finished_banner: false,
            themed: false,
            prog_anim: 0.0,

            mode: AppMode::RunningHub,
            w_4sapi_key: w_key,
            w_access_token: w_tok,
            w_user_id: w_uid,
            w_account: None,
            w_account_err: None,
            w_account_loading: false,
            w_input_dir: String::new(),
            w_input_files: Vec::new(),
            w_output_dir,
            w_tone,
            w_series,
            w_scene_dir,
            w_concurrency,
            w_skip_done,
            w_list_filter: WListFilter::All,
            w_assets: None,
            w_assets_err: None,
            w_manifest: None,
            w_sel: None,
            w_running: false,
            w_stop: Arc::new(AtomicBool::new(false)),
            w_in_tex: None,
            w_out_tex: None,
            w_tex_job: None,
            w_in_req: false,
            w_out_req: false,
        }
    }

    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let mut app = Self::build();
        app.themed = true;
        // 启动即解析一次工作流参数 + 异步刷新账户。
        app.reparse_workflow_if_changed();
        app.refresh_account();
        app
    }

    // headless 构造：用于 egui_kittest UI 测试（无 CreationContext、不联网、不读盘）。
    #[cfg(test)]
    fn new_headless() -> Self {
        let mut a = Self::build();
        a.store = Store::default(); // 不受本机已存配置影响，保证测试确定性
        a.cfg = a.store.presets[0].cfg.clone();
        a
    }

    // 把当前工作副本写回当前预设并落盘。
    fn save_all(&mut self) {
        let i = self.store.current.min(self.store.presets.len().saturating_sub(1));
        if let Some(p) = self.store.presets.get_mut(i) {
            p.cfg = self.cfg.clone();
        }
        // 人物入景配置（含场景库共享路径）一并落盘，下次启动记住。
        self.store.wedding = WeddingCfg {
            scene_dir: self.w_scene_dir.clone(),
            output_dir: self.w_output_dir.clone(),
            tone: self.w_tone.clone(),
            series: self.w_series.clone(),
            concurrency: self.w_concurrency,
            skip_done: self.w_skip_done,
        };
        save_store(&self.store);
    }

    fn counts(&self) -> (usize, usize, usize, usize) {
        let mut ok = 0;
        let mut skip = 0;
        let mut fail = 0;
        for it in &self.items {
            match it.stage {
                Stage::Done => ok += 1,
                Stage::Skipped => skip += 1,
                Stage::Failed => fail += 1,
                _ => {}
            }
        }
        let done = ok + skip + fail;
        (done, ok, skip, fail)
    }

    fn failed_indices(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.stage == Stage::Failed)
            .map(|(i, _)| i)
            .collect()
    }

    // 后台异步查询账户（不卡 UI）。
    fn refresh_account(&mut self) {
        let key = self.cfg.api_key.trim().to_string();
        if key.is_empty() || self.account_loading {
            return;
        }
        self.account_loading = true;
        self.account_err = None;
        let tx = self.tx.clone();
        let net_retry = self.cfg.net_retry.max(1);
        std::thread::spawn(move || match RhClient::new(key, RhSettings { net_retry }) {
            Ok(c) => match c.account_status_checked() {
                Ok(info) => {
                    let _ = tx.send(Msg::Account(info));
                }
                Err(e) => {
                    let _ = tx.send(Msg::AccountErr(e.to_string()));
                }
            },
            Err(e) => {
                let _ = tx.send(Msg::AccountErr(e.to_string()));
            }
        });
    }

    // 当工作流 JSON 路径变化时，重新识别节点 + 解析可调参数。
    fn reparse_workflow_if_changed(&mut self) {
        let name = self.cfg.workflow_json.trim().to_string();
        if name == self.parsed_wf {
            return;
        }
        self.parsed_wf = name.clone();
        self.detected_in.clear();
        self.detected_out.clear();
        self.detected_params.clear();
        self.param_on.clear();
        self.param_val.clear();
        if name.is_empty() {
            return;
        }
        let path = resolve_workflow_path(&name);
        if !path.exists() {
            return;
        }
        if let Ok(io) = workflow::detect_io_nodes(&path) {
            self.detected_in = io.input_node.clone().unwrap_or_default();
            self.detected_out = io.output_node.clone().unwrap_or_default();
        }
        if let Ok(params) = workflow::detect_params(&path) {
            self.param_on = vec![false; params.len()];
            self.param_val = params.iter().map(|p| json_value_to_edit(&p.value)).collect();
            self.detected_params = params;
        }
    }

    // 根据用户勾选的参数覆盖，拼出 nodeInfoList 覆盖项。
    fn param_override_values(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for (i, p) in self.detected_params.iter().enumerate() {
            if !self.param_on.get(i).copied().unwrap_or(false) {
                continue;
            }
            let raw = self.param_val.get(i).cloned().unwrap_or_default();
            let value = edit_to_json_value(&raw, &p.value);
            out.push(serde_json::json!({
                "nodeId": p.node_id,
                "fieldName": p.field,
                "fieldValue": value,
            }));
        }
        out
    }

    fn start_full(&mut self, ctx: &egui::Context) {
        let extra = self.extra_files.clone();
        self.start_run(ctx, RunMode::Full { extra });
    }

    fn start_subset(&mut self, ctx: &egui::Context, idxs: Vec<usize>) {
        let subset: Vec<(usize, PathBuf)> = idxs
            .into_iter()
            .filter_map(|i| self.items.get(i).map(|it| (i, it.input.clone())))
            .collect();
        if subset.is_empty() {
            self.logs.push("ℹ 没有可重跑的项目。".into());
            return;
        }
        self.start_run(ctx, RunMode::Subset(subset));
    }

    fn start_run(&mut self, ctx: &egui::Context, mode: RunMode) {
        // 清空旧通道里的残留消息
        while self.rx.try_recv().is_ok() {}
        self.finished_banner = false;

        let is_full = matches!(mode, RunMode::Full { .. });
        if is_full {
            self.logs.clear();
        }

        // 1) 必填校验
        let missing: Vec<&str> = [
            ("apiKey", self.cfg.api_key.trim().is_empty()),
            ("workflowId", self.cfg.workflow_id.trim().is_empty()),
            ("输入文件夹", self.cfg.input_dir.trim().is_empty() && self.extra_files.is_empty()),
            ("输出文件夹", self.cfg.output_dir.trim().is_empty()),
        ]
        .iter()
        .filter(|(_, empty)| *empty)
        .map(|(name, _)| *name)
        .collect();
        if !missing.is_empty() {
            self.logs
                .push(format!("❌ 还有参数没填：{}", missing.join("、")));
            return;
        }

        // 2) 识别工作流节点
        let mut detected_in: Option<String> = None;
        let mut detected_out: Option<String> = None;
        let wf_name = self.cfg.workflow_json.trim();
        if !wf_name.is_empty() {
            let wf_path = resolve_workflow_path(wf_name);
            if wf_path.exists() {
                match workflow::detect_io_nodes(&wf_path) {
                    Ok(io) => {
                        detected_in = io.input_node.clone();
                        detected_out = io.output_node.clone();
                        self.detected_in = io.input_node.clone().unwrap_or_default();
                        self.detected_out = io.output_node.clone().unwrap_or_default();
                        self.logs.push(format!(
                            "ℹ 工作流识别：LoadImage={} | SaveImage={}",
                            fmt_ids(&io.all_loads),
                            fmt_ids(&io.all_saves)
                        ));
                        if io.all_loads.len() > 1 {
                            self.logs.push(
                                "⚠ 多个 LoadImage，请在高级设置用“输入节点ID”手动指定。".into(),
                            );
                        }
                        if io.all_saves.len() > 1 {
                            self.logs.push(
                                "⚠ 多个 SaveImage，默认下载全部输出（或指定输出节点ID）。".into(),
                            );
                        }
                    }
                    Err(e) => self.logs.push(format!("⚠ 读取工作流失败：{e}（依赖手动节点ID）")),
                }
            } else {
                self.logs
                    .push(format!("⚠ 没找到工作流文件 {wf_name}，将依赖手动节点ID。"));
            }
        }

        let manual_in = self.cfg.input_node_override.trim();
        let input_node = if !manual_in.is_empty() {
            manual_in.to_string()
        } else if let Some(n) = detected_in {
            n
        } else {
            self.logs.push(
                "❌ 未能确定 LoadImage 输入节点。请放置工作流 .json，或在高级设置手动填“输入节点ID”（如 642）。"
                    .into(),
            );
            return;
        };
        let manual_out = self.cfg.output_node_override.trim();
        let output_node = if !manual_out.is_empty() {
            Some(manual_out.to_string())
        } else {
            detected_out
        };

        // 额外覆盖 = 手写 JSON + 表单勾选的参数
        let mut extra_overrides = match parse_extra_overrides(&self.cfg.extra_overrides) {
            Ok(v) => v,
            Err(e) => {
                self.logs.push(format!("❌ 额外节点参数 JSON 无效：{e}"));
                return;
            }
        };
        extra_overrides.extend(self.param_override_values());

        self.save_all();

        // 3) 重置运行状态并启动后台
        self.stop = Arc::new(AtomicBool::new(false));
        self.running = true;
        self.prog_anim = 0.0;
        if is_full {
            self.items.clear();
            self.selected = None;
            self.follow = true;
            self.show_settings = false; // 跑起来后自动收起设置，腾出空间看进度/对比
        } else {
            // 子集重跑：把这些项的状态重置为等待，清掉旧结果显示
            if let RunMode::Subset(ref subset) = mode {
                for (idx, _) in subset {
                    if let Some(it) = self.items.get_mut(*idx) {
                        it.stage = Stage::Queued;
                        it.detail.clear();
                        it.outputs.clear();
                        it.out_tex = None;
                        it.out_req = false;
                    }
                }
            }
        }

        let batch = BatchConfig {
            cfg: self.cfg.clone(),
            input_node,
            output_node,
            extra_overrides,
            mode,
        };
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || run_batch(batch, tx, stop, ctx2));
    }

    // 处理拖入文件：文件夹→设输入目录；图片→追加到 extra_files
    fn handle_drop(&mut self, dropped: Vec<egui::DroppedFile>) {
        let mut added = 0;
        for f in dropped {
            let Some(p) = f.path else { continue };
            if p.is_dir() {
                self.cfg.input_dir = p.display().to_string();
                self.logs.push(format!("📂 已设输入文件夹：{}", p.display()));
            } else if p.is_file() {
                let ok = p
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
                    .unwrap_or(false);
                if ok && !self.extra_files.contains(&p) {
                    self.extra_files.push(p);
                    added += 1;
                }
            }
        }
        if added > 0 {
            self.logs
                .push(format!("➕ 追加 {added} 张图片（共 {} 张附加）", self.extra_files.len()));
        }
    }

    // 处理后台消息，必要时上传纹理
    fn drain(&mut self, ctx: &egui::Context) {
        loop {
            let m = match self.rx.try_recv() {
                Ok(m) => m,
                Err(_) => break,
            };
            match m {
                Msg::Files(list) => {
                    self.items = list
                        .into_iter()
                        .map(|(name, input)| Item {
                            name,
                            input,
                            stage: Stage::Queued,
                            detail: String::new(),
                            outputs: Vec::new(),
                            task_id: String::new(),
                            in_tex: None,
                            out_tex: None,
                            list_tex: None,
                            in_req: false,
                            out_req: false,
                            list_req: false,
                        })
                        .collect();
                    if !self.items.is_empty() {
                        self.selected = Some(0);
                    }
                }
                Msg::Stage { idx, stage, detail } => {
                    if let Some(it) = self.items.get_mut(idx) {
                        it.stage = stage;
                        it.detail = detail;
                    }
                    if self.follow && stage.is_active() {
                        self.selected = Some(idx);
                        self.reset_compare_view();
                    }
                }
                Msg::Outputs { idx, paths, task_id } => {
                    if let Some(it) = self.items.get_mut(idx) {
                        it.outputs = paths;
                        if !task_id.is_empty() {
                            it.task_id = task_id;
                        }
                        it.out_tex = None;
                        it.out_req = false;
                    }
                }
                Msg::Log(s) => {
                    self.logs.push(s);
                    if self.logs.len() > 2000 {
                        let cut = self.logs.len() - 2000;
                        self.logs.drain(0..cut);
                    }
                }
                Msg::Thumb { idx, which, w, h, rgba } => {
                    let color = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                    let tex = ctx.load_texture(
                        format!("thumb-{idx}-{}", which.key()),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    if let Some(it) = self.items.get_mut(idx) {
                        match which {
                            Which::Input => it.in_tex = Some(tex),
                            Which::Output => it.out_tex = Some(tex),
                            Which::List => it.list_tex = Some(tex),
                        }
                    }
                }
                Msg::Account(info) => {
                    self.account = Some(info);
                    self.account_err = None;
                    self.account_loading = false;
                }
                Msg::AccountErr(e) => {
                    self.account = None;
                    self.account_err = Some(e);
                    self.account_loading = false;
                }
                Msg::Finished => {
                    self.running = false;
                    self.finished_banner = true;
                    // 写清单（从 items 汇总，子集重跑后也是完整的）
                    if !self.items.is_empty() {
                        match write_manifest(&self.cfg.output_dir, &self.items) {
                            Ok(p) => self.logs.push(format!("清单已写入：{p}")),
                            Err(e) => self.logs.push(format!("⚠ 写清单失败：{e}")),
                        }
                    }
                    // 收尾：任务栏闪烁 + 可选提示音 + 可选打开输出目录
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                        egui::UserAttentionType::Informational,
                    ));
                    if self.cfg.notify_sound {
                        notify_beep();
                    }
                    if self.cfg.auto_open_output && !self.cfg.output_dir.trim().is_empty() {
                        open_folder(&self.cfg.output_dir);
                    }
                    // 余额变了，刷新一下
                    self.refresh_account();
                }
                Msg::WOutput { no, path } => {
                    if let Some(m) = self.w_manifest.as_mut() {
                        m.record_outputs(&no, vec![path.display().to_string()]);
                    }
                }
                Msg::WJob { no, shot, subjects, scene, scene_file, output, status } => {
                    if let Some(m) = self.w_manifest.as_mut() {
                        if let Some(j) = m.jobs.iter_mut().find(|j| j.no == no) {
                            if shot.is_some() {
                                j.shot = shot;
                            }
                            if subjects.is_some() {
                                j.subjects = subjects;
                            }
                            if scene.is_some() {
                                j.scene = scene;
                            }
                            if scene_file.is_some() {
                                j.scene_file = scene_file;
                            }
                            if let Some(o) = output {
                                j.outputs = vec![o];
                            }
                            j.status = status;
                        }
                    }
                }
                Msg::WThumb { job, is_out, w, h, rgba } => {
                    let color = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                    let tex = ctx.load_texture(
                        format!("w-thumb-{job}-{}", if is_out { "out" } else { "in" }),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    if self.w_sel == Some(job) {
                        if is_out {
                            self.w_out_tex = Some(tex);
                        } else {
                            self.w_in_tex = Some(tex);
                        }
                    }
                }
                Msg::WAccount(info) => {
                    self.w_account = Some(info);
                    self.w_account_err = None;
                    self.w_account_loading = false;
                }
                Msg::WAccountErr(e) => {
                    self.w_account = None;
                    self.w_account_err = Some(e);
                    self.w_account_loading = false;
                }
                Msg::WFinished => {
                    self.w_running = false;
                    self.w_save();
                    if self.cfg.notify_sound {
                        notify_beep();
                    }
                    self.w_refresh_account(ctx); // 出图后额度变了，刷新
                }
            }
        }
    }

    fn reset_compare_view(&mut self) {
        self.cmp_zoom = 1.0;
        self.cmp_offset = egui::Vec2::ZERO;
        self.cmp_wipe = 0.5;
    }

    // 进度条平滑动画：跑动中按真实进度映射到 0..90% 作下限，再叠加缓慢「涓流」（带轻微抖动、
    // 越接近越慢，自然「卡」在 90~95% 的最后一段），真完成后快速补满到 100%。
    // 纯视觉用——计数文本仍显示真实数；real ∈ [0,1] 是真实进度。
    fn anim_progress(&mut self, ctx: &egui::Context, real: f32, running: bool) -> f32 {
        let dt = ctx.input(|i| i.stable_dt).clamp(0.001, 0.1);
        if running {
            let base = (real * 0.9).clamp(0.0, 0.9); // 真实进度映射到 0..90%（长批量跟着真实走）
            let t = ctx.input(|i| i.time) as f32;
            let jitter = 0.4 + 0.6 * (0.5 + 0.5 * (t * 0.8).sin()); // 0.4..1.0 起伏 → 偶尔「卡一下」
            let rate = 0.22 * jitter;
            let creep = self.prog_anim + (0.95 - self.prog_anim).max(0.0) * (1.0 - (-rate * dt).exp());
            self.prog_anim = creep.max(base).min(0.95); // 封顶 95%，最后 5% 留给真完成
        } else {
            // 不在跑：吸附到真实进度（完成→100%；中途停→保持当前不回退），1 帧 settle、不无限重绘。
            self.prog_anim = self.prog_anim.max(real).clamp(0.0, 1.0);
        }
        self.prog_anim.clamp(0.0, 1.0)
    }

    // 为选中项按需请求大图预览解码（后台线程解码，避免卡 UI）。
    // 预览用「全分辨率」纹理（按 GPU 纹理上限夹紧），这样放大对比时仍清晰，
    // 而不是把上限 ~1100px 的小图拉大导致发虚。为控制显存，只保留当前选中项的大图，
    // 切换选中时释放其它项的预览纹理（列表小缩略图 list_tex 不动）。
    fn request_thumbs(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.selected else { return };

        // 释放非选中项的大图纹理，保证同时只驻留一张原图 + 一张结果图。
        for (i, it) in self.items.iter_mut().enumerate() {
            if i != idx && (it.in_tex.is_some() || it.out_tex.is_some()) {
                it.in_tex = None;
                it.out_tex = None;
                it.in_req = false;
                it.out_req = false;
            }
        }

        // 解码上限：取 GPU 允许的最大纹理边，再夹到 PREVIEW_MAX；绝不超过 GPU 上限以免上传失败。
        let cap = (ctx.input(|i| i.max_texture_side) as u32)
            .min(PREVIEW_MAX)
            .max(LIST_THUMB_MAX);

        let (need_in, need_out, in_path, out_path) = {
            let Some(it) = self.items.get(idx) else { return };
            let need_in = it.in_tex.is_none() && !it.in_req;
            let need_out = it.out_tex.is_none() && !it.out_req && !it.outputs.is_empty();
            (
                need_in,
                need_out,
                it.input.clone(),
                it.outputs.first().cloned(),
            )
        };
        if need_in {
            self.items[idx].in_req = true;
            spawn_decode(self.tx.clone(), ctx.clone(), idx, Which::Input, in_path, cap);
        }
        if need_out {
            if let Some(p) = out_path {
                self.items[idx].out_req = true;
                spawn_decode(self.tx.clone(), ctx.clone(), idx, Which::Output, p, cap);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render(ui);
    }
}

impl App {
    fn render(&mut self, ui: &mut egui::Ui) {
        if !self.themed {
            apply_theme(ui.ctx());
            self.themed = true;
        }
        let ctx = ui.ctx().clone();

        // 统一界面缩放：抵消各机系统 DPI 差异 → 跨机一致（effective ppp = win_scale）。
        // 用户可在右上角 ± 调，记住在配置里。
        let native = ctx.native_pixels_per_point().unwrap_or(1.0);
        let target_zoom = (self.store.win_scale / native).clamp(0.4, 3.0);
        if (ctx.zoom_factor() - target_zoom).abs() > 0.005 {
            ctx.set_zoom_factor(target_zoom);
        }

        self.drain(&ctx);
        match self.mode {
            AppMode::RunningHub => self.request_thumbs(&ctx),
            AppMode::Wedding => self.w_request_thumbs(&ctx),
        }

        // 拖拽导入（按模式分流）
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped.is_empty() {
            match self.mode {
                AppMode::RunningHub => self.handle_drop(dropped),
                AppMode::Wedding => self.w_handle_drop(dropped),
            }
        }

        // 记忆窗口尺寸（用于下次启动还原）
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            self.store.win_w = r.width();
            self.store.win_h = r.height();
        }
        // 关闭前保存
        if ctx.input(|i| i.viewport().close_requested()) {
            self.save_all();
        }

        let header_shadow = egui::Shadow {
            offset: [0, 3],
            blur: 10,
            spread: 0,
            color: egui::Color32::from_black_alpha(22),
        };
        let wedding = self.mode == AppMode::Wedding;
        egui::Panel::top("header")
            .frame(panel_frame(pal::SURFACE, 18, 12, header_shadow))
            .show_inside(ui, |ui| {
                self.mode_switch(ui);
                if wedding {
                    self.w_header(ui, &ctx);
                } else {
                    self.header(ui, &ctx);
                }
            });
        let logs_resp = egui::Panel::bottom("logs")
            .resizable(true)
            .default_size(self.store.logs_h)
            .frame(panel_frame(pal::BG, 14, 8, egui::Shadow::NONE))
            .show_inside(ui, |ui| self.log_panel(ui));
        self.store.logs_h = logs_resp.response.rect.height();
        let list_resp = egui::Panel::left("list")
            .resizable(true)
            .default_size(self.store.left_w)
            .frame(panel_frame(pal::PANEL, 12, 10, egui::Shadow::NONE))
            .show_inside(ui, |ui| {
                if wedding {
                    self.w_list(ui);
                } else {
                    self.list_panel(ui);
                }
            });
        self.store.left_w = list_resp.response.rect.width();
        egui::CentralPanel::default()
            .frame(panel_frame(pal::BG, 16, 14, egui::Shadow::NONE))
            .show_inside(ui, |ui| {
                if wedding {
                    self.w_preview(ui);
                } else {
                    self.preview_panel(ui);
                }
            });

        // 拖拽悬停提示
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let p = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_overlay"),
            ));
            p.rect_filled(screen, 0.0, egui::Color32::from_rgba_unmultiplied(13, 148, 136, 36));
            p.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "松开以导入图片 / 文件夹",
                egui::FontId::proportional(26.0),
                pal::ACCENT_DK,
            );
        }

        // 跑动中持续重绘（~30fps）让进度条平滑创进；停下后吸附到真实进度，1 帧即 settle。
        if self.running || self.w_running {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // 标题行
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("◆").color(pal::ACCENT).size(20.0));
            ui.add_space(2.0);
            ui.heading(egui::RichText::new(APP_TITLE).color(pal::TEXT));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("RunningHub 批量精修 / 高清放大").color(pal::TEXT_WEAK));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (txt, col) = if self.running {
                    ("● 处理中", pal::INFO)
                } else {
                    ("● 就绪", pal::SUCCESS)
                };
                ui.label(egui::RichText::new(txt).color(col).small());
                ui.add_space(10.0);
                self.account_badge(ui);
            });
        });
        ui.add_space(10.0);

        // 控制栏
        let mut want_start = false;
        let mut want_retry_failed = false;
        let mut want_rerun_selected = false;
        ui.horizontal(|ui| {
            if !self.running {
                let btn = egui::Button::new(
                    egui::RichText::new("▶  开始批量处理")
                        .size(15.0)
                        .strong()
                        .color(pal::ON_ACCENT),
                )
                .fill(pal::ACCENT)
                .min_size(egui::vec2(168.0, 34.0))
                .corner_radius(egui::CornerRadius::same(10));
                if ui.add(btn).clicked() {
                    want_start = true;
                }
            } else {
                let btn = egui::Button::new(
                    egui::RichText::new("■  停止")
                        .size(15.0)
                        .strong()
                        .color(pal::ON_ACCENT),
                )
                .fill(pal::ERROR)
                .min_size(egui::vec2(120.0, 34.0))
                .corner_radius(egui::CornerRadius::same(10));
                if ui.add(btn).clicked() {
                    self.stop.store(true, Ordering::SeqCst);
                    self.logs.push("⏹ 已请求停止，等待当前任务结束…".into());
                }
            }

            // 重试失败 / 重跑选中
            let nfail = self.failed_indices().len();
            if ui
                .add_enabled(
                    !self.running && nfail > 0,
                    egui::Button::new(format!("↻  重试失败项 ({nfail})")),
                )
                .clicked()
            {
                want_retry_failed = true;
            }
            if ui
                .add_enabled(
                    !self.running && self.selected.is_some(),
                    egui::Button::new("⟳  重跑选中"),
                )
                .clicked()
            {
                want_rerun_selected = true;
            }

            if ui
                .add_enabled(
                    !self.cfg.output_dir.trim().is_empty(),
                    egui::Button::new("📂  打开输出文件夹"),
                )
                .clicked()
            {
                open_folder(&self.cfg.output_dir);
            }
            ui.checkbox(&mut self.follow, "跟随处理中");
            ui.toggle_value(&mut self.show_settings, "⚙  参数设置");
        });

        // 完成横幅
        if self.finished_banner && !self.running && !self.items.is_empty() {
            ui.add_space(6.0);
            let (_, ok, skip, fail) = self.counts();
            banner(
                ui,
                pal::SUCCESS,
                &format!("✅ 处理完成：成功 {ok}，跳过 {skip}，失败 {fail}"),
            );
        }

        // 进度
        let total = self.items.len();
        if total > 0 {
            ui.add_space(8.0);
            let (done, ok, skip, fail) = self.counts();
            let real = done as f32 / total as f32;
            let frac = self.anim_progress(ctx, real, self.running);
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_height(16.0)
                    .corner_radius(egui::CornerRadius::same(8))
                    .fill(pal::ACCENT)
                    .text(
                        egui::RichText::new(format!("{done} / {total}   {:.0}%", frac * 100.0))
                            .color(pal::TEXT)
                            .small(),
                    ),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                count_chip(ui, format!("✓ 成功 {ok}"), pal::SUCCESS);
                count_chip(ui, format!("⏭ 跳过 {skip}"), pal::WARN);
                count_chip(ui, format!("✗ 失败 {fail}"), pal::ERROR);
                if self.running {
                    ui.add_space(4.0);
                    ui.spinner();
                    ui.label(egui::RichText::new("处理中…").color(pal::TEXT_WEAK).small());
                }
            });
        }

        // 设置（可收起）
        if self.show_settings {
            ui.add_space(10.0);
            card_frame().show(ui, |ui| {
                ui.add_enabled_ui(!self.running, |ui| self.settings_form(ui));
            });
        }

        if want_start {
            self.start_full(ctx);
        } else if want_retry_failed {
            let idxs = self.failed_indices();
            self.start_subset(ctx, idxs);
        } else if want_rerun_selected {
            if let Some(i) = self.selected {
                self.start_subset(ctx, vec![i]);
            }
        }
    }

    // 顶部常驻账户徽章
    fn account_badge(&mut self, ui: &mut egui::Ui) {
        if ui.small_button("⟳").on_hover_text("刷新账户").clicked() {
            self.refresh_account();
        }
        if self.account_loading {
            ui.label(egui::RichText::new("账户查询中…").color(pal::TEXT_WEAK).small());
        } else if let Some(e) = &self.account_err {
            ui.label(
                egui::RichText::new(format!("账户：{e}"))
                    .color(pal::ERROR)
                    .small(),
            );
        } else if let Some(a) = &self.account {
            let coins = a.remain_coins.clone().unwrap_or_else(|| "?".into());
            let tasks = a.current_task_counts.clone().unwrap_or_else(|| "?".into());
            let low = coins_value(a).map(|c| c <= 0.0).unwrap_or(false);
            count_chip(
                ui,
                format!("🪙 余额 {coins}"),
                if low { pal::ERROR } else { pal::ACCENT },
            );
            count_chip(ui, format!("⛓ 并发 {tasks}"), pal::INFO);
        }
    }

    fn settings_form(&mut self, ui: &mut egui::Ui) {
        // —— 预设切换 ——
        self.preset_bar(ui);
        ui.add_space(6.0);

        // —— 主参数：全宽自适应行，长路径 / API Key 完整可见 ——
        ui.horizontal(|ui| {
            field_label(ui, "API Key");
            let bw = 88.0;
            let w = (ui.available_width() - bw - 8.0).max(160.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.api_key)
                    .password(true)
                    .hint_text("头像 → API 调用 页面创建并复制")
                    .desired_width(w),
            );
            if ui.add_sized([bw, 30.0], egui::Button::new("测试连接")).clicked() {
                self.refresh_account();
            }
        });

        ui.horizontal(|ui| {
            field_label(ui, "工作流 ID");
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.workflow_id)
                    .hint_text("网址 /workflow/ 后面那串数字")
                    .desired_width(ui.available_width()),
            );
        });

        ui.horizontal(|ui| {
            field_label(ui, "输入素材");
            let bw = 62.0;
            let w = (ui.available_width() - bw * 2.0 - 12.0).max(120.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.input_dir)
                    .hint_text("选文件夹批量；或「选图」单/多选；也可拖拽进窗口")
                    .desired_width(w),
            );
            if ui.add_sized([bw, 30.0], egui::Button::new("文件夹…")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.cfg.input_dir = p.display().to_string();
                }
            }
            if ui
                .add_sized([bw, 30.0], egui::Button::new("选图…"))
                .on_hover_text("单选/多选图片（不必整文件夹），与文件夹叠加")
                .clicked()
            {
                let mut n = 0;
                for p in pick_image_files() {
                    if !self.extra_files.contains(&p) {
                        self.extra_files.push(p);
                        n += 1;
                    }
                }
                if n > 0 {
                    self.logs.push(format!("➕ 选入 {n} 张图片（共 {} 张附加）", self.extra_files.len()));
                }
            }
        });

        ui.horizontal(|ui| {
            field_label(ui, "输出文件夹");
            let bw = 64.0;
            let w = (ui.available_width() - bw - 8.0).max(140.0);
            ui.add(egui::TextEdit::singleline(&mut self.cfg.output_dir).desired_width(w));
            if ui.add_sized([bw, 30.0], egui::Button::new("浏览…")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.cfg.output_dir = p.display().to_string();
                }
            }
        });

        ui.horizontal(|ui| {
            field_label(ui, "工作流 JSON");
            let bw = 64.0;
            let w = (ui.available_width() - bw - 8.0).max(140.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.workflow_json)
                    .hint_text("用于自动识别输入/输出节点")
                    .desired_width(w),
            );
            if ui.add_sized([bw, 30.0], egui::Button::new("浏览…")).clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("json", &["json"]).pick_file() {
                    self.cfg.workflow_json = p.display().to_string();
                }
            }
        });

        ui.horizontal(|ui| {
            field_label(ui, "并发数");
            ui.add(egui::Slider::new(&mut self.cfg.concurrency, 1..=8));
            ui.label(
                egui::RichText::new("基础套餐填 1；额度足够可调大成倍提速")
                    .color(pal::TEXT_WEAK)
                    .small(),
            );
        });

        // 工作流路径变了就重新解析节点/参数
        self.reparse_workflow_if_changed();

        // 拖入的附加图片提示
        if !self.extra_files.is_empty() {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("➕ 已拖入 {} 张附加图片", self.extra_files.len()))
                        .color(pal::ACCENT),
                );
                if ui.small_button("清除").clicked() {
                    self.extra_files.clear();
                }
            });
        }

        ui.checkbox(
            &mut self.cfg.skip_processed,
            "跳过已处理的图片（断点续跑）—— 取消勾选则全部重新处理",
        );
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cfg.auto_open_output, "跑完自动打开输出文件夹");
            ui.checkbox(&mut self.cfg.notify_sound, "完成提示音");
        });

        // —— 工作流参数（表单化）——
        if !self.detected_params.is_empty() {
            egui::CollapsingHeader::new(format!("工作流参数（{} 项，勾选“覆盖”才生效）", self.detected_params.len()))
                .default_open(false)
                .show(ui, |ui| self.params_form(ui));
        }

        egui::CollapsingHeader::new("高级设置（节点 / 重试 / 超时 / 预警）")
            .default_open(false)
            .show(ui, |ui| {
                egui::Grid::new("adv")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("输入节点ID（覆盖自动）");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.input_node_override)
                                .hint_text("留空=自动；如 642")
                                .desired_width(200.0),
                        );
                        ui.end_row();
                        ui.label("输出节点ID（覆盖自动）");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.output_node_override)
                                .hint_text("留空=自动")
                                .desired_width(200.0),
                        );
                        ui.end_row();
                        ui.label("注入字段名");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.input_field)
                                .desired_width(200.0),
                        );
                        ui.end_row();
                        ui.label("轮询间隔(秒)");
                        ui.add(egui::DragValue::new(&mut self.cfg.poll_interval));
                        ui.end_row();
                        ui.label("任务超时(秒)");
                        ui.add(egui::DragValue::new(&mut self.cfg.task_timeout));
                        ui.end_row();
                        ui.label("提交重试次数");
                        ui.add(egui::DragValue::new(&mut self.cfg.create_retry));
                        ui.end_row();
                        ui.label("提交重试等待(秒)");
                        ui.add(egui::DragValue::new(&mut self.cfg.create_retry_wait));
                        ui.end_row();
                        ui.label("网络重试次数");
                        ui.add(egui::DragValue::new(&mut self.cfg.net_retry));
                        ui.end_row();
                        ui.label("每张预估消耗(余额预警)");
                        ui.add(
                            egui::DragValue::new(&mut self.cfg.coins_per_image)
                                .speed(0.1)
                                .range(0.0..=1_000_000.0),
                        );
                        ui.end_row();
                    });
                ui.checkbox(&mut self.cfg.add_metadata, "写入元数据 addMetadata");
                ui.label("额外节点参数（JSON 数组，可选；例：固定种子）");
                ui.add(
                    egui::TextEdit::multiline(&mut self.cfg.extra_overrides)
                        .hint_text("[{\"nodeId\":\"采样器id\",\"fieldName\":\"seed\",\"fieldValue\":123456}]")
                        .desired_rows(2)
                        .desired_width(560.0),
                );
            });

        if !self.detected_in.is_empty() {
            let out = if self.detected_out.is_empty() {
                "全部输出".to_string()
            } else {
                self.detected_out.clone()
            };
            ui.label(
                egui::RichText::new(format!("已识别节点：输入={}  输出={}", self.detected_in, out))
                    .weak(),
            );
        }
    }

    // 预设下拉 + 新建/重命名/删除
    fn preset_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("预设").color(pal::TEXT_WEAK).small());
            let cur_name = self
                .store
                .presets
                .get(self.store.current)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "默认".into());
            let mut switch_to: Option<usize> = None;
            egui::ComboBox::from_id_salt("preset_combo")
                .selected_text(cur_name)
                .show_ui(ui, |ui| {
                    for (i, p) in self.store.presets.iter().enumerate() {
                        if ui
                            .selectable_label(i == self.store.current, &p.name)
                            .clicked()
                        {
                            switch_to = Some(i);
                        }
                    }
                });
            if let Some(i) = switch_to {
                if i != self.store.current {
                    // 先把当前编辑写回旧预设，再切换
                    let old = self.store.current;
                    if let Some(p) = self.store.presets.get_mut(old) {
                        p.cfg = self.cfg.clone();
                    }
                    self.store.current = i;
                    self.cfg = self.store.presets[i].cfg.clone();
                    self.parsed_wf.clear(); // 触发重新解析
                    save_store(&self.store);
                    self.refresh_account();
                }
            }

            if ui.small_button("➕ 新建").clicked() {
                // 当前编辑先写回，再新建一份默认
                let old = self.store.current;
                if let Some(p) = self.store.presets.get_mut(old) {
                    p.cfg = self.cfg.clone();
                }
                let name = format!("预设 {}", self.store.presets.len() + 1);
                self.store.presets.push(Preset {
                    name,
                    cfg: UiConfig::default(),
                });
                self.store.current = self.store.presets.len() - 1;
                self.cfg = self.store.presets[self.store.current].cfg.clone();
                self.parsed_wf.clear();
                save_store(&self.store);
            }
            if ui.small_button("🗑 删除").clicked() && self.store.presets.len() > 1 {
                let i = self.store.current;
                self.store.presets.remove(i);
                if self.store.current >= self.store.presets.len() {
                    self.store.current = self.store.presets.len() - 1;
                }
                self.cfg = self.store.presets[self.store.current].cfg.clone();
                self.parsed_wf.clear();
                save_store(&self.store);
            }

            // 重命名
            if let Some(p) = self.store.presets.get_mut(self.store.current) {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut p.name)
                        .desired_width(140.0)
                        .hint_text("预设名"),
                );
                if resp.lost_focus() {
                    save_store(&self.store);
                }
            }
        });
    }

    // 工作流参数表单
    fn params_form(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("params")
            .num_columns(3)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                for (i, p) in self.detected_params.iter().enumerate() {
                    let on = self.param_on.get_mut(i);
                    if let Some(on) = on {
                        ui.checkbox(on, "覆盖");
                    }
                    ui.label(
                        egui::RichText::new(format!("{} · {}#{}", p.label, p.node_type, p.node_id))
                            .color(pal::TEXT_WEAK)
                            .small(),
                    );
                    let enabled = self.param_on.get(i).copied().unwrap_or(false);
                    if let Some(val) = self.param_val.get_mut(i) {
                        ui.add_enabled(
                            enabled,
                            egui::TextEdit::singleline(val).desired_width(220.0),
                        );
                    }
                    ui.end_row();
                }
            });
        ui.label(
            egui::RichText::new("提示：种子留空/不覆盖=每次随机；勾选并填数字=固定。")
                .color(pal::TEXT_WEAK)
                .small(),
        );
    }

    fn list_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("图片列表").color(pal::TEXT).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.items.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("{} 张", self.items.len()))
                            .color(pal::TEXT_WEAK)
                            .small(),
                    );
                }
            });
        });

        // 状态筛选
        ui.horizontal_wrapped(|ui| {
            for f in [
                ListFilter::All,
                ListFilter::Active,
                ListFilter::Ok,
                ListFilter::Skipped,
                ListFilter::Failed,
            ] {
                if ui
                    .selectable_label(self.list_filter == f, f.label())
                    .clicked()
                {
                    self.list_filter = f;
                }
            }
        });
        ui.add_space(6.0);

        if self.items.is_empty() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("🗂").size(30.0).color(pal::TEXT_WEAK));
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("填好参数后点“开始批量处理”，或把图片/文件夹拖进来")
                        .color(pal::TEXT_WEAK)
                        .small(),
                );
            });
            return;
        }

        // 过滤出可见项（索引指向 items）
        let visible: Vec<usize> = (0..self.items.len())
            .filter(|&i| self.list_filter.matches(self.items[i].stage))
            .collect();

        if visible.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("该筛选下没有图片")
                        .color(pal::TEXT_WEAK)
                        .small(),
                );
            });
            return;
        }

        let row_h = 64.0;
        let mut clicked: Option<usize> = None;
        let mut to_decode: Vec<(usize, PathBuf)> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("list_scroll")
            .auto_shrink([false, false])
            .show_rows(ui, row_h, visible.len(), |ui, range| {
                for vi in range {
                    let i = visible[vi];
                    let selected = self.selected == Some(i);
                    let (name, stage, detail, has_thumb) = {
                        let it = &self.items[i];
                        (it.name.clone(), it.stage, it.detail.clone(), it.list_tex.is_some())
                    };
                    // 懒解码列表缩略图
                    if !has_thumb {
                        let it = &mut self.items[i];
                        if !it.list_req {
                            it.list_req = true;
                            to_decode.push((i, it.input.clone()));
                        }
                    }

                    // 整行可点：用 Frame 画选中底色 + 非交互内容，避免内部控件吞掉点击
                    let bg = if selected {
                        tint(pal::ACCENT, 36)
                    } else {
                        egui::Color32::TRANSPARENT
                    };
                    let inner = egui::Frame {
                        inner_margin: egui::Margin::symmetric(6, 4),
                        outer_margin: egui::Margin::same(0),
                        fill: bg,
                        stroke: egui::Stroke::NONE,
                        corner_radius: egui::CornerRadius::same(8),
                        shadow: egui::Shadow::NONE,
                    }
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            let thumb_sz = egui::vec2(42.0, 42.0);
                            if let Some(t) = &self.items[i].list_tex {
                                ui.add(
                                    egui::Image::new(t)
                                        .fit_to_exact_size(thumb_sz)
                                        .corner_radius(6.0),
                                );
                            } else {
                                let (r, _) =
                                    ui.allocate_exact_size(thumb_sz, egui::Sense::hover());
                                ui.painter()
                                    .rect_filled(r, egui::CornerRadius::same(6), pal::FIELD);
                            }
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                let nm = egui::RichText::new(&name).color(pal::TEXT);
                                ui.label(if selected { nm.strong() } else { nm });
                                ui.horizontal(|ui| {
                                    stage_chip(ui, stage);
                                    if stage.is_active() && !detail.is_empty() {
                                        ui.label(
                                            egui::RichText::new(&detail)
                                                .color(pal::TEXT_WEAK)
                                                .small(),
                                        );
                                    }
                                });
                            });
                        });
                    });
                    let resp = inner.response.interact(egui::Sense::click());
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                }
            });

        for (i, path) in to_decode {
            spawn_decode(self.tx.clone(), ui.ctx().clone(), i, Which::List, path, LIST_THUMB_MAX);
        }
        if let Some(i) = clicked {
            self.selected = Some(i);
            self.follow = false;
            self.reset_compare_view();
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        let Some(idx) = self.selected else {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("🖼").size(40.0).color(pal::TEXT_WEAK));
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("从左侧选择一张图片查看处理前/后对比")
                        .color(pal::TEXT_WEAK),
                );
            });
            return;
        };

        // 取出渲染所需数据（避免与可变借用冲突）
        let (name, stage, detail, has_out, in_tex, out_tex, in_path, out_path) = {
            let it = &self.items[idx];
            (
                it.name.clone(),
                it.stage,
                it.detail.clone(),
                !it.outputs.is_empty(),
                it.in_tex.clone(),
                it.out_tex.clone(),
                it.input.clone(),
                it.outputs.first().cloned(),
            )
        };

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("对比").color(pal::TEXT).strong());
            ui.label(egui::RichText::new(&name).color(pal::TEXT_WEAK));
            stage_chip(ui, stage);
            if stage.is_active() && !detail.is_empty() {
                ui.label(egui::RichText::new(&detail).color(pal::TEXT_WEAK).small());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 模式切换
                if ui
                    .selectable_label(self.cmp_mode == CmpMode::Wipe, "滑动对比")
                    .clicked()
                {
                    self.cmp_mode = CmpMode::Wipe;
                }
                if ui
                    .selectable_label(self.cmp_mode == CmpMode::Side, "并排")
                    .clicked()
                {
                    self.cmp_mode = CmpMode::Side;
                }
                if self.cmp_mode == CmpMode::Wipe {
                    if ui.button("1:1").clicked() {
                        self.set_one_to_one(&out_tex, &in_tex);
                    }
                    if ui.button("适应").clicked() {
                        self.reset_compare_view();
                    }
                    ui.label(
                        egui::RichText::new(format!("{:.0}%", self.cmp_zoom * 100.0))
                            .color(pal::TEXT_WEAK)
                            .small(),
                    );
                }
            });
        });
        ui.add_space(10.0);

        match self.cmp_mode {
            CmpMode::Side => {
                let avail_w = ui.available_width();
                let avail_h = ui.available_height();
                let cell =
                    egui::vec2((avail_w / 2.0 - 44.0).max(120.0), (avail_h - 92.0).max(160.0));
                ui.columns(2, |cols| {
                    preview_cell(&mut cols[0], "处理前 · 原图", &in_tex, cell, |ui| {
                        ui.label(egui::RichText::new("⏳ 加载中…").color(pal::TEXT_WEAK).small());
                    });
                    preview_cell(&mut cols[1], "处理后 · 结果", &out_tex, cell, |ui| {
                        if has_out {
                            ui.label(egui::RichText::new("⏳ 加载中…").color(pal::TEXT_WEAK).small());
                        } else {
                            empty_after(ui, stage);
                        }
                    });
                });
            }
            CmpMode::Wipe => {
                self.wipe_viewer(ui, &in_tex, &out_tex, has_out, stage);
            }
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if open_btn(ui, "🖼  打开原图") {
                open_file(&in_path);
            }
            if has_out && open_btn(ui, "🔍  打开结果") {
                if let Some(p) = &out_path {
                    open_file(p);
                }
            }
            if let Some(p) = out_path.as_ref().or(Some(&in_path)) {
                ui.label(
                    egui::RichText::new(p.display().to_string())
                        .color(pal::TEXT_WEAK)
                        .small(),
                );
            }
        });
    }

    // 1:1 像素查看：让纹理按自身像素 1 texel = 1 屏幕像素显示。
    // 预览纹理已是全分辨率（按 GPU 纹理上限夹紧），所以这是真实像素视图，
    // 放大/精修的细节可直接在此评估，无需再打开外部看图器。
    fn set_one_to_one(
        &mut self,
        out_tex: &Option<egui::TextureHandle>,
        in_tex: &Option<egui::TextureHandle>,
    ) {
        self.cmp_offset = egui::Vec2::ZERO;
        if let Some(t) = out_tex.as_ref().or(in_tex.as_ref()) {
            let tw = t.size_vec2().x;
            if self.cmp_fit_w > 1.0 {
                self.cmp_zoom = (tw / self.cmp_fit_w).clamp(0.2, 12.0);
                return;
            }
        }
        self.cmp_zoom = 1.0;
    }

    // 滑动对比 + 缩放/平移
    fn wipe_viewer(
        &mut self,
        ui: &mut egui::Ui,
        in_tex: &Option<egui::TextureHandle>,
        out_tex: &Option<egui::TextureHandle>,
        has_out: bool,
        stage: Stage,
    ) {
        let avail = egui::vec2(ui.available_width(), (ui.available_height() - 56.0).max(160.0));
        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::CornerRadius::same(8), pal::SURFACE);
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(8),
            egui::Stroke::new(1.0, pal::STROKE),
            egui::StrokeKind::Inside,
        );

        // 没有结果图就退化成只看原图 / 提示
        let (Some(itex), otex) = (in_tex.as_ref(), out_tex.as_ref()) else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "加载中…",
                egui::FontId::proportional(14.0),
                pal::TEXT_WEAK,
            );
            return;
        };
        if otex.is_none() && !has_out {
            // 原图已有，但无结果：左半画原图，右半给状态文字
            let base = fit_rect(rect, tex_aspect(itex));
            self.cmp_fit_w = base.width();
            let draw = scaled_rect(base, rect.center() + self.cmp_offset, self.cmp_zoom);
            painter.image(itex.id(), draw, uv_full(), egui::Color32::WHITE);
            empty_after_painter(&painter, rect, stage);
            self.handle_zoom_pan(ui, &resp, rect);
            return;
        }
        let Some(otex) = otex else {
            // 结果还在解码
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "结果加载中…",
                egui::FontId::proportional(14.0),
                pal::TEXT_WEAK,
            );
            return;
        };

        // 缩放/平移交互（拖动分割线优先）
        let divider_x = rect.left() + self.cmp_wipe * rect.width();
        if resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.cmp_drag_wipe = (pos.x - divider_x).abs() < 12.0;
            }
        }
        if resp.dragged() {
            if self.cmp_drag_wipe {
                if let Some(pos) = resp.interact_pointer_pos() {
                    self.cmp_wipe = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                }
            } else {
                self.cmp_offset += resp.drag_delta();
            }
        }
        if resp.drag_stopped() {
            self.cmp_drag_wipe = false;
        }
        self.handle_zoom_pan(ui, &resp, rect);

        // 用 out 的宽高比作为基准，两图叠到同一显示矩形
        let base = fit_rect(rect, tex_aspect(otex));
        self.cmp_fit_w = base.width();
        let draw = scaled_rect(base, rect.center() + self.cmp_offset, self.cmp_zoom);
        let divider_x = rect.left() + self.cmp_wipe * rect.width();

        // 左半：原图
        let left_clip = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(divider_x, rect.bottom()),
        );
        painter
            .with_clip_rect(left_clip)
            .image(itex.id(), draw, uv_full(), egui::Color32::WHITE);
        // 右半：结果
        let right_clip = egui::Rect::from_min_max(
            egui::pos2(divider_x, rect.top()),
            rect.right_bottom(),
        );
        painter
            .with_clip_rect(right_clip)
            .image(otex.id(), draw, uv_full(), egui::Color32::WHITE);

        // 分割线 + 手柄
        painter.line_segment(
            [egui::pos2(divider_x, rect.top()), egui::pos2(divider_x, rect.bottom())],
            egui::Stroke::new(2.0, pal::ACCENT),
        );
        painter.circle_filled(egui::pos2(divider_x, rect.center().y), 7.0, pal::ACCENT);
        // 角标
        painter.text(
            rect.left_top() + egui::vec2(8.0, 8.0),
            egui::Align2::LEFT_TOP,
            "原图",
            egui::FontId::proportional(12.0),
            pal::TEXT_WEAK,
        );
        painter.text(
            rect.right_top() + egui::vec2(-8.0, 8.0),
            egui::Align2::RIGHT_TOP,
            "结果",
            egui::FontId::proportional(12.0),
            pal::ACCENT_DK,
        );

        if resp.hovered() {
            ui.ctx().set_cursor_icon(if self.cmp_drag_wipe {
                egui::CursorIcon::ResizeHorizontal
            } else {
                egui::CursorIcon::Grab
            });
        }
    }

    fn handle_zoom_pan(&mut self, ui: &mut egui::Ui, resp: &egui::Response, _rect: egui::Rect) {
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                let factor = (scroll * 0.0015).exp();
                self.cmp_zoom = (self.cmp_zoom * factor).clamp(0.2, 12.0);
            }
        }
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("日志").color(pal::TEXT).strong());
            if !self.logs.is_empty() {
                ui.label(
                    egui::RichText::new(format!("{} 条", self.logs.len()))
                        .color(pal::TEXT_WEAK)
                        .small(),
                );
            }
            // 级别过滤
            egui::ComboBox::from_id_salt("log_filter")
                .selected_text(self.log_filter.label())
                .width(72.0)
                .show_ui(ui, |ui| {
                    for f in [
                        LogFilter::All,
                        LogFilter::Info,
                        LogFilter::Success,
                        LogFilter::Warn,
                        LogFilter::Error,
                    ] {
                        ui.selectable_value(&mut self.log_filter, f, f.label());
                    }
                });
            ui.add(
                egui::TextEdit::singleline(&mut self.log_search)
                    .hint_text("搜索…")
                    .desired_width(160.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("清空").clicked() {
                    self.logs.clear();
                }
                if ui.button("导出").clicked() {
                    self.export_logs();
                }
            });
        });
        ui.add_space(4.0);
        let search = self.log_search.to_lowercase();
        egui::ScrollArea::vertical()
            .id_salt("logs_scroll")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.logs {
                    if !log_matches_filter(line, self.log_filter) {
                        continue;
                    }
                    if !search.is_empty() && !line.to_lowercase().contains(&search) {
                        continue;
                    }
                    ui.label(egui::RichText::new(line).monospace().color(log_color(line)));
                }
            });
    }

    fn export_logs(&mut self) {
        let default = "vc_batch_log.txt";
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name(default)
            .add_filter("text", &["txt"])
            .save_file()
        {
            match std::fs::write(&path, self.logs.join("\r\n")) {
                Ok(()) => self.logs.push(format!("📝 日志已导出：{}", path.display())),
                Err(e) => self.logs.push(format!("⚠ 日志导出失败：{e}")),
            }
        }
    }
}

// ============ 人物入景（C 模式）UI ============
fn shot_label(s: &str) -> &'static str {
    match s {
        "full" => "全身",
        "medium" => "中景",
        "close" => "近景",
        "closeup" => "特写",
        _ => "景别",
    }
}
fn tone_label(s: &str) -> &'static str {
    match s {
        "warm" => "暖",
        "overcast" => "冷",
        _ => "自然",
    }
}
fn w_status_chip(ui: &mut egui::Ui, st: cstate::CStatus) {
    let color = match st {
        cstate::CStatus::Pending => pal::TEXT_WEAK,
        cstate::CStatus::Prompted => pal::TEXT_WEAK,
        cstate::CStatus::Generating => pal::INFO,
        cstate::CStatus::AwaitingQc => pal::SUCCESS,
        cstate::CStatus::Selected => pal::ACCENT,
        cstate::CStatus::Done => pal::SUCCESS,
        cstate::CStatus::QcFail => pal::ERROR,
        cstate::CStatus::Skipped => pal::WARN,
        cstate::CStatus::Failed => pal::ERROR,
    };
    egui::Frame {
        inner_margin: egui::Margin::symmetric(8, 2),
        outer_margin: egui::Margin::same(0),
        fill: tint(color, 38),
        stroke: egui::Stroke::new(1.0, tint(color, 90)),
        corner_radius: egui::CornerRadius::same(7),
        shadow: egui::Shadow::NONE,
    }
    .show(ui, |ui| {
        ui.label(egui::RichText::new(st.label()).color(color).small());
    });
}

// 人物入景预览解码（后台线程 → Msg::WThumb）。
fn w_spawn_decode(
    tx: Sender<Msg>,
    ctx: egui::Context,
    job: usize,
    is_out: bool,
    path: PathBuf,
    max: u32,
) {
    std::thread::spawn(move || {
        match decode_thumb(&path, max) {
            Ok((w, h, rgba)) => {
                let _ = tx.send(Msg::WThumb { job, is_out, w, h, rgba });
            }
            Err(e) => {
                let _ = tx.send(Msg::Log(format!("⚠ 预览加载失败 {}：{e}", path.display())));
            }
        }
        ctx.request_repaint();
    });
}

// 人物入景一键流水线（后台线程）：判景别 → 自动选景排重 → 装配 → 双图出图（断点续跑）。
// 简单并发执行器：n 个 worker 从队列取索引执行 f（f 必须 Send + Clone + 'static）。
fn run_pool<F>(n: usize, items: Vec<usize>, stop: &Arc<AtomicBool>, f: F)
where
    F: Fn(usize) + Send + Clone + 'static,
{
    if items.is_empty() {
        return;
    }
    let (jtx, jrx) = crossbeam_channel::unbounded::<usize>();
    for it in items {
        let _ = jtx.send(it);
    }
    drop(jtx);
    let mut handles = Vec::new();
    for _ in 0..n.max(1) {
        let jrx = jrx.clone();
        let stop = stop.clone();
        let f = f.clone();
        handles.push(std::thread::spawn(move || {
            while let Ok(i) = jrx.recv() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                f(i);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

// 输出文件名按人物原片名（断点续跑对增删图鲁棒，对齐 RunningHub）。
fn w_out_name(out_dir: &str, input: &str) -> PathBuf {
    let stem = Path::new(input).file_stem().and_then(|s| s.to_str()).unwrap_or("img");
    Path::new(out_dir).join(format!("{stem}_c.png"))
}

#[allow(clippy::too_many_arguments)]
fn w_pipeline(
    m: cstate::CManifest,
    assets: cmode::Assets,
    key: String,
    out_dir: String,
    scene_base: PathBuf,
    _client: String,
    tx: Sender<Msg>,
    ctx: egui::Context,
    stop: Arc<AtomicBool>,
    concurrency: usize,
    skip_done: bool,
) {
    use std::sync::Mutex;
    let log = |s: String| {
        let _ = tx.send(Msg::Log(s));
        ctx.request_repaint();
    };
    let cli = match foursapi::FsClient::new(key) {
        Ok(c) => c,
        Err(e) => {
            log(format!("❌ 4sapi 初始化失败：{e}"));
            let _ = tx.send(Msg::WFinished);
            ctx.request_repaint();
            return;
        }
    };
    let _ = std::fs::create_dir_all(&out_dir);
    let n = concurrency.max(1);
    let m = Arc::new(Mutex::new(m));

    // 预跳过（断点续跑）：输出已存在、且仍是「待装配」的单直接标「已跳过」，不判景别、不出图（省 token/钱）。
    // 复用账本里已判/已出/已选/手改的单不动（手改重出的单已被 w_edit_job 删了旧图、回 Pending）。
    if skip_done {
        let len = m.lock().unwrap().jobs.len();
        for i in 0..len {
            let (no, input, st) = {
                let g = m.lock().unwrap();
                (g.jobs[i].no.clone(), g.jobs[i].input.clone(), g.jobs[i].status)
            };
            if st != cstate::CStatus::Pending {
                continue;
            }
            let outp = w_out_name(&out_dir, &input);
            if outp.exists() {
                {
                    let mut g = m.lock().unwrap();
                    g.jobs[i].status = cstate::CStatus::Skipped;
                    g.jobs[i].outputs = vec![outp.display().to_string()];
                }
                let _ = tx.send(Msg::WJob {
                    no,
                    shot: None,
                    subjects: None,
                    scene: None,
                    scene_file: None,
                    output: Some(outp.display().to_string()),
                    status: cstate::CStatus::Skipped,
                });
            }
        }
        ctx.request_repaint();
    }

    // 阶段 A：判景别（并发；跳过未跳过且缺 shot/subjects 的单）。
    let judge_idx: Vec<usize> = {
        let g = m.lock().unwrap();
        (0..g.jobs.len())
            .filter(|&i| {
                let j = &g.jobs[i];
                j.status != cstate::CStatus::Skipped && (j.shot.is_none() || j.subjects.is_none())
            })
            .collect()
    };
    {
        let cli = cli.clone();
        let m = m.clone();
        let tx = tx.clone();
        let ctx = ctx.clone();
        run_pool(n, judge_idx, &stop, move |i| {
            let (no, input) = {
                let g = m.lock().unwrap();
                (g.jobs[i].no.clone(), g.jobs[i].input.clone())
            };
            let _ = tx.send(Msg::Log(format!("  判景别 {no}…")));
            ctx.request_repaint();
            let (shot, subj) = match cli.judge_shot(Path::new(&input)) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(Msg::Log(format!(
                        "  ⚠ {no} 判景别失败，默认 全身/单人（请左侧核对人数，必要时改后重出）：{e}"
                    )));
                    ("full".to_string(), 1)
                }
            };
            {
                let mut g = m.lock().unwrap();
                g.jobs[i].shot = Some(shot.clone());
                g.jobs[i].subjects = Some(subj);
            }
            let _ = tx.send(Msg::WJob {
                no: no.clone(),
                shot: Some(shot.clone()),
                subjects: Some(subj),
                scene: None,
                scene_file: None,
                output: None,
                status: cstate::CStatus::Pending,
            });
            let _ = tx.send(Msg::Log(format!(
                "  {no} → 景别 {} · {}",
                shot,
                if subj == 1 { "单人" } else { "双人" }
            )));
            ctx.request_repaint();
        });
    }
    if stop.load(Ordering::SeqCst) {
        log("⏹ 已停止。".into());
        let _ = tx.send(Msg::WFinished);
        ctx.request_repaint();
        return;
    }

    // 阶段 B：自动选景排重 + 装配（顺序，含跨单排重依赖）。
    {
        let mut g = m.lock().unwrap();
        g.auto_plan(&assets.scenes);
        let _ = g.assemble(&assets);
        for j in &g.jobs {
            let _ = tx.send(Msg::WJob {
                no: j.no.clone(),
                shot: None,
                subjects: None,
                scene: j.scene.clone(),
                scene_file: j.scene_file.clone(),
                output: None,
                status: j.status,
            });
        }
    }
    ctx.request_repaint();

    // 阶段 C：双图出图（并发）。
    let gen_idx: Vec<usize> = {
        let g = m.lock().unwrap();
        (0..g.jobs.len())
            .filter(|&i| g.jobs[i].status == cstate::CStatus::Prompted)
            .collect()
    };
    {
        let cli = cli.clone();
        let m = m.clone();
        let tx = tx.clone();
        let ctx = ctx.clone();
        let out_dir = out_dir.clone();
        let scene_base = scene_base.clone();
        run_pool(n, gen_idx, &stop, move |i| {
            let (no, input, prompt, scene_file) = {
                let g = m.lock().unwrap();
                let j = &g.jobs[i];
                (
                    j.no.clone(),
                    j.input.clone(),
                    j.prompt.clone().unwrap_or_default(),
                    j.scene_file.clone().unwrap_or_default(),
                )
            };
            let fail = |st: cstate::CStatus| Msg::WJob {
                no: no.clone(),
                shot: None,
                subjects: None,
                scene: None,
                scene_file: None,
                output: None,
                status: st,
            };
            if prompt.is_empty() || scene_file.is_empty() {
                let _ = tx.send(Msg::Log(format!("  跳过 {no}：未成功装配（catalog 可能缺该景别场景）")));
                let _ = tx.send(fail(cstate::CStatus::Failed));
                return;
            }
            let _ = tx.send(Msg::WJob {
                no: no.clone(),
                shot: None,
                subjects: None,
                scene: None,
                scene_file: None,
                output: None,
                status: cstate::CStatus::Generating,
            });
            let person = PathBuf::from(&input);
            let plate = scene_base.join(&scene_file);
            let (w, h) = image::image_dimensions(&person).unwrap_or((1024, 1536));
            let size = cmode::size_for_aspect(w, h);
            let _ = tx.send(Msg::Log(format!("  ▶ 出图 {no}（{size}）…")));
            ctx.request_repaint();
            let outp = w_out_name(&out_dir, &input);
            match cli.edit_dual(&person, &plate, &prompt, size, "high") {
                Ok(bytes) => match std::fs::write(&outp, &bytes) {
                    Ok(()) => {
                        let _ = tx.send(Msg::Log(format!("  ✓ {no} 完成")));
                        let _ = tx.send(Msg::WJob {
                            no: no.clone(),
                            shot: None,
                            subjects: None,
                            scene: None,
                            scene_file: None,
                            output: Some(outp.display().to_string()),
                            status: cstate::CStatus::AwaitingQc,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Log(format!("  ✗ {no} 写文件失败：{e}")));
                        let _ = tx.send(fail(cstate::CStatus::Failed));
                    }
                },
                Err(e) => {
                    let _ = tx.send(Msg::Log(format!("  ✗ {no} 出图失败：{e}")));
                    let _ = tx.send(fail(cstate::CStatus::Failed));
                }
            }
            ctx.request_repaint();
        });
    }

    log(format!(
        "{}本轮结束（结果在输出夹）。",
        if stop.load(Ordering::SeqCst) { "⏹ 已停止，" } else { "" }
    ));
    let _ = tx.send(Msg::WFinished);
    ctx.request_repaint();
}

impl App {
    fn mode_switch(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("◆").color(pal::ACCENT).size(18.0));
            ui.add_space(4.0);
            if ui
                .selectable_label(self.mode == AppMode::RunningHub, "  RunningHub 精修/放大  ")
                .clicked()
            {
                self.mode = AppMode::RunningHub;
            }
            if ui
                .selectable_label(self.mode == AppMode::Wedding, "  人物入景  ")
                .clicked()
            {
                self.mode = AppMode::Wedding;
                if self.w_assets.is_none() {
                    self.w_load_assets();
                }
            }
            // 右上角：界面缩放（跨机一致，可调）+ 版本号
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(format!("v{APP_VERSION}")).color(pal::TEXT_WEAK).small());
                ui.add_space(12.0);
                if ui.small_button("＋").on_hover_text("界面放大").clicked() {
                    self.store.win_scale = (self.store.win_scale + 0.1).min(2.0);
                    self.save_all();
                }
                ui.label(
                    egui::RichText::new(format!("{:.0}%", self.store.win_scale * 100.0))
                        .color(pal::TEXT)
                        .small(),
                );
                if ui.small_button("－").on_hover_text("界面缩小").clicked() {
                    self.store.win_scale = (self.store.win_scale - 0.1).max(0.7);
                    self.save_all();
                }
                ui.label(egui::RichText::new("缩放").color(pal::TEXT_WEAK).small());
            });
        });
        ui.separator();
    }

    fn w_load_assets(&mut self) {
        let dir = PathBuf::from(self.w_scene_dir.trim());
        match cmode::load_from_dir(&dir) {
            Ok(a) => {
                self.w_assets = Some(a);
                self.w_assets_err = None;
            }
            Err(e) => {
                self.w_assets = None;
                self.w_assets_err = Some(e.to_string());
            }
        }
        self.save_all(); // 记住场景库路径（含局域网共享路径）等配置
    }

    // 人物入景拖拽：文件夹→设原片文件夹；图片→追加到单独选的原片。
    fn w_handle_drop(&mut self, dropped: Vec<egui::DroppedFile>) {
        let mut added = 0;
        for f in dropped {
            let Some(p) = f.path else { continue };
            if p.is_dir() {
                self.w_input_dir = p.display().to_string();
                self.logs.push(format!("📂 已设原片文件夹：{}", p.display()));
            } else if is_image_file(&p) && !self.w_input_files.contains(&p) {
                self.w_input_files.push(p);
                added += 1;
            }
        }
        if added > 0 {
            self.logs.push(format!("➕ 拖入 {added} 张原片（共 {} 张单独选）", self.w_input_files.len()));
        }
    }

    // 后台查 4sapi 额度（测试连接）。优先用访问令牌，留空退回 sk- 密钥。
    fn w_refresh_account(&mut self, ctx: &egui::Context) {
        let token = self.w_access_token.trim().to_string();
        let key = self.w_4sapi_key.trim().to_string();
        if token.is_empty() && key.is_empty() {
            self.w_account_err = Some("先填访问令牌或 sk- 密钥".into());
            return;
        }
        if self.w_account_loading {
            return;
        }
        self.w_account_loading = true;
        self.w_account_err = None;
        let uid = self.w_user_id.trim().to_string();
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            match foursapi::FsClient::new(key) {
                Ok(c) => match c.account(&token, &uid) {
                    Ok(a) => {
                        let _ = tx.send(Msg::WAccount(a));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::WAccountErr(e.to_string()));
                    }
                },
                Err(e) => {
                    let _ = tx.send(Msg::WAccountErr(e.to_string()));
                }
            }
            ctx2.request_repaint();
        });
    }

    fn w_manifest_path(&self) -> Option<PathBuf> {
        let dir = self.w_output_dir.trim();
        let m = self.w_manifest.as_ref()?;
        if dir.is_empty() {
            return None;
        }
        Some(Path::new(dir).join(format!("{}_manifest.json", m.client)))
    }
    fn w_save(&self) {
        if let (Some(m), Some(p)) = (&self.w_manifest, self.w_manifest_path()) {
            let _ = m.save(&p);
        }
    }

    fn w_header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("人物入景").color(pal::TEXT));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("真人原片 → 场景库 → 自动选景装配 → 双图出图（gpt-image-2）").color(pal::TEXT_WEAK).small());
        });
        ui.add_space(8.0);

        // 配置行 1：4sapi Key / 调色 / 场景池
        ui.horizontal(|ui| {
            field_label(ui, "4sapi Key");
            ui.add(
                egui::TextEdit::singleline(&mut self.w_4sapi_key)
                    .password(true)
                    .desired_width(240.0)
                    .hint_text("key.txt 自动读取"),
            );
            ui.add_space(8.0);
            ui.label("调色");
            egui::ComboBox::from_id_salt("w_tone")
                .selected_text(tone_label(&self.w_tone))
                .width(64.0)
                .show_ui(ui, |ui| {
                    for v in ["natural", "warm", "overcast"] {
                        if ui.selectable_label(self.w_tone == v, tone_label(v)).clicked() {
                            self.w_tone = v.into();
                        }
                    }
                });
            ui.add_space(8.0);
            ui.label("场景池");
            let serieses: Vec<String> = {
                let mut v: Vec<String> = Vec::new();
                if let Some(a) = &self.w_assets {
                    for s in &a.scenes {
                        if !v.contains(&s.series) {
                            v.push(s.series.clone());
                        }
                    }
                }
                v
            };
            let cur = if self.w_series.is_empty() { "全部".to_string() } else { self.w_series.clone() };
            egui::ComboBox::from_id_salt("w_series")
                .selected_text(cur)
                .width(96.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.w_series.is_empty(), "全部").clicked() {
                        self.w_series.clear();
                    }
                    for s in &serieses {
                        if ui.selectable_label(&self.w_series == s, s).clicked() {
                            self.w_series = s.clone();
                        }
                    }
                });
        });

        // 额度 / 测试连接（访问令牌：4sapi 个人设置→系统访问令牌；非 sk- 出图密钥）
        ui.horizontal(|ui| {
            field_label(ui, "访问令牌");
            ui.add(
                egui::TextEdit::singleline(&mut self.w_access_token)
                    .password(true)
                    .desired_width(200.0)
                    .hint_text("个人设置→系统访问令牌（查额度用，可留空）"),
            );
            ui.add_space(6.0);
            ui.label("用户ID");
            ui.add(
                egui::TextEdit::singleline(&mut self.w_user_id)
                    .desired_width(72.0)
                    .hint_text("可留空"),
            );
            if ui.add_sized([84.0, 28.0], egui::Button::new("测试 / 刷新")).clicked() {
                self.w_refresh_account(ctx);
            }
            if self.w_account_loading {
                ui.label(egui::RichText::new("查询中…").color(pal::TEXT_WEAK).small());
            } else if let Some(a) = &self.w_account {
                let low = a.remain_usd() <= 1.0;
                count_chip(
                    ui,
                    format!("🪙 余额 ≈ ${:.2}", a.remain_usd()),
                    if low { pal::ERROR } else { pal::ACCENT },
                );
                if !a.group.is_empty() {
                    count_chip(ui, format!("组 {}", a.group), pal::INFO);
                }
                ui.label(
                    egui::RichText::new(format!("已用 ${:.2}", a.used_usd()))
                        .color(pal::TEXT_WEAK)
                        .small(),
                );
            } else if let Some(e) = &self.w_account_err {
                ui.label(egui::RichText::new(format!("⚠ {e}")).color(pal::ERROR).small());
            }
        });

        // 配置行 2/3：原片 / 输出文件夹
        ui.horizontal(|ui| {
            field_label(ui, "原片素材");
            let bw = 62.0;
            let w = (ui.available_width() - bw * 2.0 - 12.0).max(120.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.w_input_dir)
                    .desired_width(w)
                    .hint_text("选文件夹批量（夹名=客户名）；或「选图」单/多选；也可拖拽"),
            );
            if ui.add_sized([bw, 30.0], egui::Button::new("文件夹…")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.w_input_dir = p.display().to_string();
                }
            }
            if ui
                .add_sized([bw, 30.0], egui::Button::new("选图…"))
                .on_hover_text("单选/多选原片（不必整文件夹），与文件夹叠加")
                .clicked()
            {
                let mut n = 0;
                for p in pick_image_files() {
                    if !self.w_input_files.contains(&p) {
                        self.w_input_files.push(p);
                        n += 1;
                    }
                }
                if n > 0 {
                    self.logs.push(format!("➕ 选入 {n} 张原片（共 {} 张单独选）", self.w_input_files.len()));
                }
            }
        });
        if !self.w_input_files.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(96.0);
                ui.label(
                    egui::RichText::new(format!("➕ 已单独选 {} 张原片", self.w_input_files.len()))
                        .color(pal::ACCENT)
                        .small(),
                );
                if ui.small_button("清除").clicked() {
                    self.w_input_files.clear();
                }
            });
        }
        ui.horizontal(|ui| {
            field_label(ui, "输出文件夹");
            let bw = 64.0;
            let w = (ui.available_width() - bw - 8.0).max(140.0);
            ui.add(egui::TextEdit::singleline(&mut self.w_output_dir).desired_width(w));
            if ui.add_sized([bw, 30.0], egui::Button::new("浏览…")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.w_output_dir = p.display().to_string();
                }
            }
        });

        // 场景库目录（可编辑 + 浏览 + 加载）
        ui.horizontal(|ui| {
            field_label(ui, "场景库");
            let bw = 60.0;
            let w = (ui.available_width() - bw * 2.0 - 16.0).max(140.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.w_scene_dir)
                    .desired_width(w)
                    .hint_text("含 templates/ 与 scenes/ 的目录"),
            );
            if ui.add_sized([bw, 30.0], egui::Button::new("浏览…")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.w_scene_dir = p.display().to_string();
                    self.w_load_assets();
                }
            }
            if ui.add_sized([bw, 30.0], egui::Button::new("加载")).clicked() {
                self.w_load_assets();
            }
        });
        ui.horizontal(|ui| {
            ui.add_space(96.0);
            if let Some(a) = &self.w_assets {
                ui.label(
                    egui::RichText::new(format!("✓ 已加载 {} 张 plate", a.scenes.len()))
                        .color(pal::SUCCESS)
                        .small(),
                );
            } else {
                let msg = self.w_assets_err.clone().unwrap_or_else(|| "未加载（填好目录点「加载」）".into());
                ui.label(egui::RichText::new(format!("⚠ {msg}")).color(pal::WARN).small());
            }
        });

        // 并发 / 断点续跑（对齐 RunningHub）
        ui.horizontal(|ui| {
            field_label(ui, "并发数");
            ui.add(egui::Slider::new(&mut self.w_concurrency, 1..=8));
            ui.add_space(12.0);
            ui.checkbox(&mut self.w_skip_done, "跳过已处理（断点续跑）");
        });

        ui.add_space(10.0);
        // —— 一键自动流水线 ——
        let has_sel = self
            .w_manifest
            .as_ref()
            .map_or(false, |m| m.jobs.iter().any(|j| j.status == cstate::CStatus::Selected));
        ui.horizontal(|ui| {
            if !self.w_running {
                let b = egui::Button::new(
                    egui::RichText::new("▶  开始").size(15.0).strong().color(pal::ON_ACCENT),
                )
                .fill(pal::ACCENT)
                .min_size(egui::vec2(150.0, 34.0))
                .corner_radius(egui::CornerRadius::same(10));
                if ui
                    .add(b)
                    .on_hover_text("自动：判景别 → 选景装配 → 双图出图 → 落到输出夹（已存在结果自动跳过）")
                    .clicked()
                {
                    self.w_start(ctx);
                }
            } else {
                let b = egui::Button::new(
                    egui::RichText::new("■  停止").size(15.0).strong().color(pal::ON_ACCENT),
                )
                .fill(pal::ERROR)
                .min_size(egui::vec2(110.0, 34.0))
                .corner_radius(egui::CornerRadius::same(10));
                if ui.add(b).clicked() {
                    self.w_stop.store(true, Ordering::SeqCst);
                    self.logs.push("⏹ 已请求停止，等当前一张结束…".into());
                }
                ui.label(egui::RichText::new("● 处理中").color(pal::INFO).small());
            }
            if ui
                .add_enabled(!self.w_running && has_sel, egui::Button::new("归集已选"))
                .on_hover_text("把 QC 选用的片归档到 输出/final")
                .clicked()
            {
                self.w_collect();
            }
            if ui
                .add_enabled(!self.w_output_dir.trim().is_empty(), egui::Button::new("📂 输出夹"))
                .clicked()
            {
                open_folder(&self.w_output_dir);
            }
        });

        // 进度 + 统计（对齐 RunningHub）
        let counts = self.w_manifest.as_ref().map(|m| m.counts());
        if let Some((tot, active, ok, skip, fail)) = counts {
            let done = ok + skip; // 已出结果（成功 + 跳过）
            if tot > 0 {
                ui.add_space(8.0);
                let real = (done + fail) as f32 / tot as f32;
                let frac = self.anim_progress(ctx, real, self.w_running);
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_height(16.0)
                        .corner_radius(egui::CornerRadius::same(8))
                        .fill(pal::ACCENT)
                        .text(
                            egui::RichText::new(format!("{} / {tot}   {:.0}%", done + fail, frac * 100.0))
                                .color(pal::TEXT)
                                .small(),
                        ),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    count_chip(ui, format!("✓ 成功 {ok}"), pal::SUCCESS);
                    count_chip(ui, format!("⏭ 跳过 {skip}"), pal::WARN);
                    count_chip(ui, format!("✗ 失败 {fail}"), pal::ERROR);
                    if self.w_running {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(format!("进行中 {active}…")).color(pal::INFO).small());
                    }
                });
            }
        }
    }

    fn w_init(&mut self) {
        let dir = self.w_input_dir.trim().to_string();
        if dir.is_empty() {
            self.logs.push("❌ 先选原片文件夹".into());
            return;
        }
        let path = PathBuf::from(&dir);
        let client = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("client")
            .to_string();
        match cstate::CManifest::init(&client, &path) {
            Ok(mut m) => {
                m.key = self.w_tone.clone();
                m.series = if self.w_series.is_empty() { None } else { Some(self.w_series.clone()) };
                self.logs.push(format!("✅ 建账「{client}」：{} 单。逐单选景别/人数 → 排片 → 装配 → 出图。", m.jobs.len()));
                self.w_manifest = Some(m);
                self.w_sel = Some(0);
                self.w_save();
            }
            Err(e) => self.logs.push(format!("❌ 建账失败：{e}")),
        }
    }

    // 一键自动流水线：建账 → 后台(判景别 → 选景装配 → 双图出图)。
    fn w_start(&mut self, ctx: &egui::Context) {
        let key = self.w_4sapi_key.trim().to_string();
        if key.is_empty() {
            self.logs.push("❌ 4sapi Key 为空（放 key.txt 或手填）".into());
            return;
        }
        let in_dir = self.w_input_dir.trim().to_string();
        let out_dir = self.w_output_dir.trim().to_string();
        if in_dir.is_empty() && self.w_input_files.is_empty() {
            self.logs.push("❌ 先选「文件夹」或点「选图」选几张原片".into());
            return;
        }
        if out_dir.is_empty() {
            self.logs.push("❌ 先选输出文件夹".into());
            return;
        }
        let assets = match &self.w_assets {
            Some(a) => a.clone(),
            None => {
                self.logs.push("❌ 场景库未加载（填好场景库目录点「加载」）".into());
                return;
            }
        };
        // 收集输入：文件夹里的图片 + 单独选/拖入的图片（去重）
        let mut files: Vec<PathBuf> = Vec::new();
        if !in_dir.is_empty() {
            if let Ok(rd) = std::fs::read_dir(&in_dir) {
                let mut df: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| is_image_file(p)).collect();
                df.sort();
                files.extend(df);
            }
        }
        for p in &self.w_input_files {
            if !files.contains(p) {
                files.push(p.clone());
            }
        }
        if files.is_empty() {
            self.logs.push("❌ 没有可处理的图片（文件夹为空且没选图）".into());
            return;
        }
        // 客户名：文件夹名优先；否则取第一张图的父目录名
        let client = if !in_dir.is_empty() {
            PathBuf::from(&in_dir).file_name().and_then(|s| s.to_str()).unwrap_or("client").to_string()
        } else {
            files
                .first()
                .and_then(|p| p.parent())
                .and_then(|d| d.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("素材")
                .to_string()
        };
        let skip = self.w_skip_done;
        // 复用账本：仅「纯文件夹、未单独选图」且同客户时复用（保留手改）；选了图=新选一批，每次新建。
        let reuse = skip
            && self.w_input_files.is_empty()
            && !in_dir.is_empty()
            && self
                .w_manifest
                .as_ref()
                .map_or(false, |m| m.client == client && !m.jobs.is_empty());
        let mut m = if reuse {
            self.w_manifest.clone().unwrap()
        } else {
            cstate::CManifest::from_files(&client, &files)
        };
        m.key = self.w_tone.clone();
        m.series = if self.w_series.is_empty() { None } else { Some(self.w_series.clone()) };
        if reuse {
            self.logs.push(format!("（复用「{client}」账本：保留景别/人数，已出的跳过、改过的重出）"));
        }
        self.w_manifest = Some(m.clone());
        if self.w_sel.is_none() {
            self.w_sel = Some(0);
        }
        self.w_running = true;
        self.prog_anim = 0.0;
        self.w_stop = Arc::new(AtomicBool::new(false));
        self.w_save();
        let conc = self.w_concurrency.max(1);
        self.logs.push(format!(
            "▶ 开始「{}」：{} 张，并发 {} → 自动判景别({}) → 选景装配 → 双图出图{}",
            client,
            m.jobs.len(),
            conc,
            foursapi::VISION_MODEL,
            if skip { "（断点续跑：已有结果跳过）" } else { "（全部重出）" }
        ));
        let scene_base = PathBuf::from(self.w_scene_dir.trim());
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        let stop = self.w_stop.clone();
        std::thread::spawn(move || {
            w_pipeline(m, assets, key, out_dir, scene_base, client, tx, ctx2, stop, conc, skip)
        });
    }

    fn w_collect(&mut self) {
        let out_dir = self.w_output_dir.trim().to_string();
        if out_dir.is_empty() {
            self.logs.push("❌ 先选输出文件夹".into());
            return;
        }
        let final_dir = Path::new(&out_dir).join("final");
        let res = self.w_manifest.as_mut().map(|m| m.collect(&final_dir));
        match res {
            Some(Ok((done, ups))) => {
                self.logs.push(format!("✅ 归集 {} 张 → {}", done.len(), final_dir.display()));
                if !ups.is_empty() {
                    self.logs.push(format!(
                        "⚠ 需放大（长边<{}px，可切到 RunningHub 高清放大）：{}",
                        cstate::UPSCALE_EDGE,
                        ups.join(", ")
                    ));
                }
                self.w_save();
            }
            Some(Err(e)) => self.logs.push(format!("❌ 归集失败：{e}")),
            None => {}
        }
    }

    fn w_select(&mut self, no: &str, pick: &str) {
        let le = image::image_dimensions(Path::new(pick)).map(|(w, h)| w.max(h)).unwrap_or(0);
        if let Some(m) = self.w_manifest.as_mut() {
            let _ = m.select(no, pick, le);
        }
        self.logs.push(format!("✓ {no} 选用（长边 {le}px）"));
        self.w_save();
    }
    fn w_fail(&mut self, no: &str) {
        if let Some(m) = self.w_manifest.as_mut() {
            let _ = m.fail(no, "QC 不过");
        }
        self.logs.push(format!("✗ {no} 判废（可重装配/重出）"));
        self.w_save();
    }

    // 手改某单的景别/人数：删旧图、清记录、回「待装配」，下次「▶ 开始」只重出这一张（其它已出的仍跳过）。
    fn w_edit_job(&mut self, no: &str, shot: Option<String>, subjects: Option<u8>) {
        let mut note = String::new();
        if let Some(m) = self.w_manifest.as_mut() {
            if let Some(j) = m.jobs.iter_mut().find(|j| j.no == no) {
                if let Some(s) = &shot {
                    j.shot = Some(s.clone());
                }
                if let Some(s) = subjects {
                    j.subjects = Some(s);
                }
                // 删旧结果文件 + 清记录 + 回 Pending（断点续跑下会重出这一张）
                for o in &j.outputs {
                    let _ = std::fs::remove_file(o);
                }
                j.outputs.clear();
                j.selected = None;
                j.needs_upscale = None;
                j.prompt = None;
                j.tpl_md5 = None;
                if shot.is_some() {
                    // 改了景别 → 清场景，下次自动重选合适的
                    j.scene = None;
                    j.scene_file = None;
                }
                j.status = cstate::CStatus::Pending;
                note = match (&shot, subjects) {
                    (Some(s), _) => format!("✎ {no} 景别改为「{}」，已清旧图、待重出（点「▶ 开始」）", shot_label(s)),
                    (None, Some(1)) => format!("✎ {no} 改为「单人」，已清旧图、待重出（点「▶ 开始」）"),
                    (None, Some(2)) => format!("✎ {no} 改为「双人」，已清旧图、待重出（点「▶ 开始」）"),
                    _ => format!("✎ {no} 已改、待重出"),
                };
            }
        }
        if !note.is_empty() {
            self.logs.push(note);
            self.w_save();
        }
    }

    fn w_list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("单子列表").color(pal::TEXT).strong());
            if let Some(m) = &self.w_manifest {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(format!("{} 单", m.jobs.len())).color(pal::TEXT_WEAK).small());
                });
            }
        });
        // 状态筛选（全部/进行中/成功/跳过/失败）
        if self.w_manifest.is_some() {
            ui.horizontal_wrapped(|ui| {
                for f in [
                    WListFilter::All,
                    WListFilter::Active,
                    WListFilter::Ok,
                    WListFilter::Skipped,
                    WListFilter::Failed,
                ] {
                    if ui.selectable_label(self.w_list_filter == f, f.label()).clicked() {
                        self.w_list_filter = f;
                    }
                }
            });
        }
        ui.add_space(6.0);
        if self.w_manifest.is_none() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("填好配置后点「▶ 开始」").color(pal::TEXT_WEAK).small());
            });
            return;
        }
        let filter = self.w_list_filter;
        let snapshot: Vec<(usize, String, Option<String>, Option<u8>, Option<String>, cstate::CStatus)> = self
            .w_manifest
            .as_ref()
            .unwrap()
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| filter.matches(j.status))
            .map(|(i, j)| (i, j.no.clone(), j.shot.clone(), j.subjects, j.scene.clone(), j.status))
            .collect();
        let mut click: Option<usize> = None;
        let mut edits: Vec<(String, Option<String>, Option<u8>)> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("w_list_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (idx, no, shot, subj, scene, status) in &snapshot {
                    let selected = self.w_sel == Some(*idx);
                    let bg = if selected { tint(pal::ACCENT, 36) } else { egui::Color32::TRANSPARENT };
                    egui::Frame {
                        inner_margin: egui::Margin::symmetric(6, 5),
                        outer_margin: egui::Margin::same(0),
                        fill: bg,
                        stroke: egui::Stroke::NONE,
                        corner_radius: egui::CornerRadius::same(8),
                        shadow: egui::Shadow::NONE,
                    }
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(selected, egui::RichText::new(format!("#{no}")).strong())
                                .clicked()
                            {
                                click = Some(*idx);
                            }
                            let cur = shot.clone().unwrap_or_default();
                            egui::ComboBox::from_id_salt(format!("w_shot_{no}"))
                                .selected_text(if cur.is_empty() { "景别".into() } else { shot_label(&cur).to_string() })
                                .width(64.0)
                                .show_ui(ui, |ui| {
                                    for s in ["full", "medium", "close", "closeup"] {
                                        if ui.selectable_label(cur == s, shot_label(s)).clicked() {
                                            edits.push((no.clone(), Some(s.to_string()), None));
                                        }
                                    }
                                });
                            if ui.selectable_label(*subj == Some(1), "单").clicked() {
                                edits.push((no.clone(), None, Some(1)));
                            }
                            if ui.selectable_label(*subj == Some(2), "双").clicked() {
                                edits.push((no.clone(), None, Some(2)));
                            }
                        });
                        ui.horizontal(|ui| {
                            w_status_chip(ui, *status);
                            if let Some(sc) = scene {
                                ui.label(egui::RichText::new(sc).color(pal::TEXT_WEAK).small());
                            }
                        });
                    });
                }
            });
        if let Some(i) = click {
            self.w_sel = Some(i);
        }
        for (no, shot, subj) in edits {
            self.w_edit_job(&no, shot, subj);
        }
    }

    fn w_preview(&mut self, ui: &mut egui::Ui) {
        let Some(i) = self.w_sel else {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("🖼").size(40.0).color(pal::TEXT_WEAK));
                ui.add_space(10.0);
                ui.label(egui::RichText::new("从左侧选择单子查看 原片 / 结果，并 QC 选片").color(pal::TEXT_WEAK));
            });
            return;
        };
        let (no, status, shot, subj, scene, input, outputs, needs_up) = {
            let Some(m) = &self.w_manifest else { return };
            let Some(j) = m.jobs.get(i) else {
                self.w_sel = None;
                return;
            };
            (
                j.no.clone(),
                j.status,
                j.shot.clone(),
                j.subjects,
                j.scene.clone(),
                j.input.clone(),
                j.outputs.clone(),
                j.needs_upscale,
            )
        };

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("#{no}")).color(pal::TEXT).strong());
            w_status_chip(ui, status);
            if let Some(s) = &shot {
                ui.label(egui::RichText::new(shot_label(s)).color(pal::TEXT_WEAK).small());
            }
            ui.label(
                egui::RichText::new(match subj {
                    Some(1) => "单人",
                    Some(2) => "双人",
                    _ => "人数未定",
                })
                .color(pal::TEXT_WEAK)
                .small(),
            );
            if let Some(sc) = &scene {
                ui.label(egui::RichText::new(format!("场景 {sc}")).color(pal::TEXT_WEAK).small());
            }
        });
        ui.add_space(8.0);

        let avail_w = ui.available_width();
        let avail_h = ui.available_height();
        let cell = egui::vec2((avail_w / 2.0 - 44.0).max(120.0), (avail_h - 110.0).max(160.0));
        let in_tex = self.w_in_tex.clone();
        let out_tex = self.w_out_tex.clone();
        let has_out = !outputs.is_empty();
        ui.columns(2, |cols| {
            preview_cell(&mut cols[0], "原片", &in_tex, cell, |ui| {
                ui.label(egui::RichText::new("⏳ 加载中…").color(pal::TEXT_WEAK).small());
            });
            preview_cell(&mut cols[1], "结果候选", &out_tex, cell, |ui| {
                if has_out {
                    ui.label(egui::RichText::new("⏳ 加载中…").color(pal::TEXT_WEAK).small());
                } else {
                    let (txt, col) = match status {
                        cstate::CStatus::QcFail => ("已判废", pal::WARN),
                        cstate::CStatus::Failed => ("出图失败（见日志）", pal::ERROR),
                        cstate::CStatus::Generating => ("生成中…", pal::INFO),
                        cstate::CStatus::Prompted => ("待出图", pal::INFO),
                        _ => ("尚未出图", pal::TEXT_WEAK),
                    };
                    ui.label(egui::RichText::new(txt).color(col));
                }
            });
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if open_btn(ui, "🖼  打开原片") {
                open_file(Path::new(&input));
            }
            if has_out && open_btn(ui, "🔍  打开结果") {
                open_file(Path::new(&outputs[0]));
            }
            if has_out
                && matches!(
                    status,
                    cstate::CStatus::AwaitingQc | cstate::CStatus::Selected | cstate::CStatus::Skipped
                )
            {
                let pick = outputs[0].clone();
                if ui.button("✓ 选用").clicked() {
                    self.w_select(&no, &pick);
                }
                if ui.button("✗ 判废").clicked() {
                    self.w_fail(&no);
                }
            }
            if status == cstate::CStatus::Selected {
                let tag = if needs_up == Some(true) { "已选片 · 需放大" } else { "已选片" };
                ui.label(egui::RichText::new(tag).color(pal::SUCCESS).small());
            }
        });
    }

    // 选中单的原片/结果按需解码（全分辨率，切换时释放重解）。
    fn w_request_thumbs(&mut self, ctx: &egui::Context) {
        let Some(i) = self.w_sel else { return };
        if self.w_tex_job != Some(i) {
            self.w_tex_job = Some(i);
            self.w_in_tex = None;
            self.w_out_tex = None;
            self.w_in_req = false;
            self.w_out_req = false;
        }
        let (input, out0) = {
            let Some(m) = &self.w_manifest else { return };
            let Some(j) = m.jobs.get(i) else { return };
            (j.input.clone(), j.outputs.first().cloned())
        };
        let cap = (ctx.input(|i| i.max_texture_side) as u32)
            .min(PREVIEW_MAX)
            .max(LIST_THUMB_MAX);
        if self.w_in_tex.is_none() && !self.w_in_req {
            self.w_in_req = true;
            w_spawn_decode(self.tx.clone(), ctx.clone(), i, false, PathBuf::from(input), cap);
        }
        if let Some(o) = out0 {
            if self.w_out_tex.is_none() && !self.w_out_req {
                self.w_out_req = true;
                w_spawn_decode(self.tx.clone(), ctx.clone(), i, true, PathBuf::from(o), cap);
            }
        }
    }
}

// ============ 缩略图解码（后台线程）============
fn spawn_decode(
    tx: Sender<Msg>,
    ctx: egui::Context,
    idx: usize,
    which: Which,
    path: PathBuf,
    max: u32,
) {
    std::thread::spawn(move || {
        match decode_thumb(&path, max) {
            Ok((w, h, rgba)) => {
                let _ = tx.send(Msg::Thumb { idx, which, w, h, rgba });
            }
            Err(e) => {
                let _ = tx.send(Msg::Log(format!("⚠ 预览加载失败 {}：{e}", path.display())));
            }
        }
        ctx.request_repaint();
    });
}

fn decode_thumb(path: &Path, max: u32) -> anyhow::Result<(usize, usize, Vec<u8>)> {
    let img = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?;
    let img = img.thumbnail(max, max); // 等比快速缩放
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Ok((w, h, rgba.into_raw()))
}

fn fmt_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        "无".into()
    } else {
        ids.join(",")
    }
}

fn parse_extra_overrides(text: &str) -> Result<Vec<serde_json::Value>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    let v: serde_json::Value = serde_json::from_str(t).map_err(|e| e.to_string())?;
    match v {
        serde_json::Value::Array(a) => Ok(a),
        _ => Err("必须是 JSON 数组，例如 [{...}]".into()),
    }
}

// 把工作流里的当前值转成编辑框初值字符串。
fn json_value_to_edit(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

// 把编辑框字符串按原值类型转回 JSON 值。
fn edit_to_json_value(s: &str, original: &serde_json::Value) -> serde_json::Value {
    match original {
        serde_json::Value::Number(_) => {
            if let Ok(i) = s.trim().parse::<i64>() {
                serde_json::json!(i)
            } else if let Ok(f) = s.trim().parse::<f64>() {
                serde_json::json!(f)
            } else {
                serde_json::Value::String(s.to_string())
            }
        }
        serde_json::Value::Bool(_) => match s.trim().to_lowercase().as_str() {
            "true" | "1" => serde_json::json!(true),
            "false" | "0" => serde_json::json!(false),
            _ => serde_json::Value::String(s.to_string()),
        },
        _ => serde_json::Value::String(s.to_string()),
    }
}

fn coins_value(a: &AccountInfo) -> Option<f64> {
    a.remain_coins.as_ref().and_then(|s| s.trim().parse::<f64>().ok())
}

fn resolve_workflow_path(name: &str) -> PathBuf {
    let p = PathBuf::from(name);
    if p.is_absolute() || p.exists() {
        return p;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join(name);
            if c.exists() {
                return c;
            }
        }
    }
    p
}

fn open_folder(path: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
}

fn open_file(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &path.display().to_string()])
            .spawn();
    }
}

// 完成提示音（Windows MessageBeep，无新依赖）。
#[cfg(windows)]
fn notify_beep() {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBeep(u_type: u32) -> i32;
    }
    unsafe {
        let _ = MessageBeep(0x00000040); // MB_ICONASTERISK
    }
}
#[cfg(not(windows))]
fn notify_beep() {}

// ============ 后台批处理（线程池）============
fn run_batch(b: BatchConfig, tx: Sender<Msg>, stop: Arc<AtomicBool>, ctx: egui::Context) {
    let log = |s: String| {
        let _ = tx.send(Msg::Log(s));
        ctx.request_repaint();
    };

    let settings = RhSettings {
        net_retry: b.cfg.net_retry.max(1),
    };
    let client = match RhClient::new(b.cfg.api_key.clone(), settings) {
        Ok(c) => c,
        Err(e) => {
            log(format!("❌ 初始化失败：{e}"));
            let _ = tx.send(Msg::Finished);
            ctx.request_repaint();
            return;
        }
    };

    log(format!(
        "使用：输入节点={}  输出节点={}",
        b.input_node,
        b.output_node.clone().unwrap_or_else(|| "全部".into())
    ));

    let acc = client.account_status();
    if acc.current_task_counts.is_some() || acc.remain_coins.is_some() {
        log(format!(
            "账户状态：currentTaskCounts={}  remainCoins={}",
            acc.current_task_counts.clone().unwrap_or_else(|| "?".into()),
            acc.remain_coins.clone().unwrap_or_else(|| "?".into())
        ));
    }

    // 构造任务队列（带 UI 索引）
    let jobs: Vec<(usize, PathBuf)> = match &b.mode {
        RunMode::Subset(subset) => {
            // 重置这些项的状态
            for (idx, _) in subset {
                let _ = tx.send(Msg::Stage {
                    idx: *idx,
                    stage: Stage::Queued,
                    detail: String::new(),
                });
            }
            log(format!("↻ 重跑 {} 项…", subset.len()));
            subset.clone()
        }
        RunMode::Full { extra } => {
            // 收集输入目录图片 + 拖入的附加图片
            let mut files: Vec<PathBuf> = Vec::new();
            if !b.cfg.input_dir.trim().is_empty() {
                match std::fs::read_dir(&b.cfg.input_dir) {
                    Ok(rd) => {
                        for e in rd.flatten() {
                            let p = e.path();
                            if p.is_file() {
                                if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                                    if IMAGE_EXTS.contains(&ext.to_lowercase().as_str()) {
                                        files.push(p);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log(format!("⚠ 无法读取输入文件夹：{e}（仅处理拖入的图片）"));
                    }
                }
            }
            for p in extra {
                if p.is_file() && !files.contains(p) {
                    files.push(p.clone());
                }
            }
            files.sort();
            files.dedup();
            let total = files.len();
            if total == 0 {
                log("❌ 没有可处理的图片（支持 png/jpg/jpeg/webp/bmp）".into());
                let _ = tx.send(Msg::Finished);
                ctx.request_repaint();
                return;
            }
            let _ = std::fs::create_dir_all(&b.cfg.output_dir);

            // 余额预警（按每张预估消耗）
            if b.cfg.coins_per_image > 0.0 {
                if let Some(bal) = acc.remain_coins.as_ref().and_then(|s| s.trim().parse::<f64>().ok())
                {
                    let pending = if b.cfg.skip_processed {
                        files
                            .iter()
                            .filter(|p| {
                                let name = p
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("img");
                                existing_outputs(&b.cfg.output_dir, name).is_empty()
                            })
                            .count()
                    } else {
                        total
                    };
                    let need = pending as f64 * b.cfg.coins_per_image;
                    if need > bal {
                        log(format!(
                            "⚠ 余额预警：待处理约 {pending} 张 × {:.2} = {:.2}，超过当前余额 {:.2}，可能中途用尽。",
                            b.cfg.coins_per_image, need, bal
                        ));
                    }
                }
            }

            // 先把完整列表发给 UI
            let list: Vec<(String, PathBuf)> = files
                .iter()
                .map(|p| {
                    let name = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("img")
                        .to_string();
                    (name, p.clone())
                })
                .collect();
            let _ = tx.send(Msg::Files(list));
            ctx.request_repaint();
            log(format!(
                "共 {} 张待处理，并发 {}，输出到：{}",
                total, b.cfg.concurrency, b.cfg.output_dir
            ));
            log(format!(
                "断点续跑：{}",
                if b.cfg.skip_processed {
                    "开启（已有结果的图片将自动跳过）"
                } else {
                    "关闭（全部重新处理）"
                }
            ));
            files.into_iter().enumerate().collect()
        }
    };

    // 子集重跑强制处理（忽略断点续跑）
    let force = matches!(b.mode, RunMode::Subset(_));

    let (jtx, jrx) = crossbeam_channel::unbounded::<(usize, PathBuf)>();
    for j in jobs {
        let _ = jtx.send(j);
    }
    drop(jtx);

    let n = b.cfg.concurrency.max(1);
    let mut handles = Vec::new();
    for _ in 0..n {
        let jrx = jrx.clone();
        let tx = tx.clone();
        let ctx = ctx.clone();
        let stop = stop.clone();
        let client = client.clone();
        let b = b.clone();
        handles.push(std::thread::spawn(move || {
            while let Ok((idx, path)) = jrx.recv() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("img")
                    .to_string();

                // 断点续跑：已有结果则跳过（子集强制重跑时不跳过）
                if b.cfg.skip_processed && !force {
                    let existing = existing_outputs(&b.cfg.output_dir, &name);
                    if !existing.is_empty() {
                        let _ = tx.send(Msg::Outputs {
                            idx,
                            paths: existing.clone(),
                            task_id: String::new(),
                        });
                        let _ = tx.send(Msg::Stage {
                            idx,
                            stage: Stage::Skipped,
                            detail: "已存在结果".into(),
                        });
                        let _ = tx.send(Msg::Log(format!("⏭ 跳过（已存在）：{name}")));
                        ctx.request_repaint();
                        continue;
                    }
                }

                match process_one(&client, &b, idx, &path, &name, &stop, &tx, &ctx) {
                    Ok((paths, task_id)) => {
                        let _ = tx.send(Msg::Outputs {
                            idx,
                            paths: paths.clone(),
                            task_id: task_id.clone(),
                        });
                        let _ = tx.send(Msg::Stage {
                            idx,
                            stage: Stage::Done,
                            detail: format!("{} 张结果", paths.len()),
                        });
                        let _ = tx.send(Msg::Log(format!("✓ 完成：{name} → {} 张", paths.len())));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Stage {
                            idx,
                            stage: Stage::Failed,
                            detail: e.to_string(),
                        });
                        let _ = tx.send(Msg::Log(format!("✗ 失败：{name}：{e}")));
                    }
                }
                ctx.request_repaint();
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }

    let stopped = stop.load(Ordering::SeqCst);
    log(format!(
        "{}本轮结束。",
        if stopped { "⏹ 已停止，" } else { "" }
    ));

    let _ = tx.send(Msg::Finished);
    ctx.request_repaint();
}

/// 返回输出目录里属于该图的已存在结果文件（命名 `原名_rh*`）。
fn existing_outputs(out_dir: &str, name: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(out_dir) {
        let prefix = format!("{name}_rh");
        for e in rd.flatten() {
            let p = e.path();
            if let Some(f) = p.file_name().and_then(|s| s.to_str()) {
                if f.starts_with(&prefix) {
                    v.push(p);
                }
            }
        }
    }
    v.sort();
    v
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// 从 UI items 汇总清单：全量 + 子集重跑后都正确。
fn write_manifest(out_dir: &str, items: &[Item]) -> std::io::Result<String> {
    let path = Path::new(out_dir).join("_manifest.csv");
    let mut s = String::from("\u{feff}");
    s.push_str("input,status,task_id,outputs\n");
    for it in items {
        let status = match it.stage {
            Stage::Done => {
                if it.outputs.is_empty() {
                    "no_output".to_string()
                } else {
                    "ok".to_string()
                }
            }
            Stage::Skipped => "skipped".to_string(),
            Stage::Failed => format!("error: {}", it.detail),
            _ => "pending".to_string(),
        };
        let outputs = it
            .outputs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(";");
        s.push_str(&csv_field(&it.input.display().to_string()));
        s.push(',');
        s.push_str(&csv_field(&status));
        s.push(',');
        s.push_str(&csv_field(&it.task_id));
        s.push(',');
        s.push_str(&csv_field(&outputs));
        s.push('\n');
    }
    std::fs::write(&path, s)?;
    Ok(path.display().to_string())
}

/// 处理单张：上传 → 提交（队列占满重试）→ 轮询（容忍不确定）→ 下载；过程中上报阶段。
fn process_one(
    client: &RhClient,
    b: &BatchConfig,
    idx: usize,
    path: &Path,
    name: &str,
    stop: &Arc<AtomicBool>,
    tx: &Sender<Msg>,
    ctx: &egui::Context,
) -> anyhow::Result<(Vec<PathBuf>, String)> {
    let log = |s: String| {
        let _ = tx.send(Msg::Log(s));
        ctx.request_repaint();
    };
    let stage = |st: Stage, detail: String| {
        let _ = tx.send(Msg::Stage { idx, stage: st, detail });
        ctx.request_repaint();
    };

    stage(Stage::Uploading, String::new());
    log(format!("▶ 上传：{name}"));
    let file_name = client.upload_image(path)?;

    let mut node_arr: Vec<serde_json::Value> = vec![serde_json::json!({
        "nodeId": b.input_node,
        "fieldName": b.cfg.input_field,
        "fieldValue": file_name,
    })];
    node_arr.extend(b.extra_overrides.iter().cloned());
    let nodes = serde_json::Value::Array(node_arr);

    stage(Stage::Submitting, String::new());
    log(format!("  提交任务（注入到节点 {}）…", b.input_node));
    let mut task_id = String::new();
    let retry = b.cfg.create_retry.max(1);
    for attempt in 0..retry {
        if stop.load(Ordering::SeqCst) {
            anyhow::bail!("已停止");
        }
        match client.create_task(&b.cfg.workflow_id, &nodes, b.cfg.add_metadata)? {
            CreateOutcome::Task {
                task_id: id,
                node_warnings,
            } => {
                if let Some(w) = node_warnings {
                    log(format!("⚠ 工作流节点校验提示: {w}"));
                }
                task_id = id;
                break;
            }
            CreateOutcome::Busy(m) => {
                stage(Stage::Submitting, format!("队列占满，重试 {}/{}", attempt + 1, retry));
                log(format!(
                    "  队列/并发占满（{m}），{}s 后重试 {}/{}",
                    b.cfg.create_retry_wait,
                    attempt + 1,
                    retry
                ));
                std::thread::sleep(Duration::from_secs(b.cfg.create_retry_wait));
            }
        }
    }
    if task_id.is_empty() {
        anyhow::bail!("提交失败：重试次数用尽（可能并发额度不足，建议降低并发数）");
    }
    stage(Stage::Waiting, format!("taskId={task_id}"));
    log(format!("  taskId={task_id}，等待生成…"));

    let poll = Duration::from_secs(b.cfg.poll_interval.max(1));
    let deadline = Instant::now() + Duration::from_secs(b.cfg.task_timeout.max(1));
    let mut unknown_strikes = 0u32;
    let outputs = loop {
        if stop.load(Ordering::SeqCst) {
            anyhow::bail!("已停止");
        }
        if Instant::now() > deadline {
            anyhow::bail!("任务超时（>{}s）", b.cfg.task_timeout);
        }
        match client.poll_once(&task_id)? {
            PollState::Done(items) => break items,
            PollState::Pending => {
                unknown_strikes = 0;
                std::thread::sleep(poll);
            }
            PollState::Unknown(m) => {
                unknown_strikes += 1;
                if unknown_strikes >= 4 {
                    anyhow::bail!("任务失败/异常：{m}");
                }
                std::thread::sleep(poll);
            }
        }
    };

    stage(Stage::Downloading, String::new());
    let targets: Vec<_> = match &b.output_node {
        Some(nid) => {
            let f: Vec<_> = outputs.iter().filter(|o| &o.nodeId == nid).cloned().collect();
            if f.is_empty() {
                outputs
            } else {
                f
            }
        }
        None => outputs,
    };

    let multi = targets.len() > 1;
    let mut saved: Vec<PathBuf> = Vec::new();
    for (i, o) in targets.iter().enumerate() {
        if o.fileUrl.is_empty() {
            continue;
        }
        let ext = o
            .fileUrl
            .split('?')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .filter(|e| !e.is_empty() && e.len() <= 5)
            .map(|e| format!(".{e}"))
            .unwrap_or_else(|| ".png".into());
        let suffix = if multi {
            format!("_rh_{}{}", i + 1, ext)
        } else {
            format!("_rh{}", ext)
        };
        let out = Path::new(&b.cfg.output_dir).join(format!("{name}{suffix}"));
        match client.download(&o.fileUrl, &out) {
            Ok(()) => {
                log(format!("  ✓ 已保存：{}", out.display()));
                saved.push(out);
            }
            Err(e) => log(format!("  ✗ 下载失败：{}：{e}", o.fileUrl)),
        }
    }
    Ok((saved, task_id))
}

// ============ main ============
fn main() -> Result<(), eframe::Error> {
    let store = load_store();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{APP_TITLE}  v{APP_VERSION}"))
            .with_inner_size([store.win_w, store.win_h])
            .with_min_inner_size([880.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

// 现代浅色主题：字体（含中文）、字号、间距、配色、圆角、阴影。
fn apply_theme(ctx: &egui::Context) {
    setup_cjk_font(ctx);

    let mut style = (*ctx.global_style()).clone();

    use egui::{FontFamily::Proportional, FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Heading, FontId::new(23.0, Proportional)),
        (TextStyle::Body, FontId::new(14.5, Proportional)),
        (TextStyle::Button, FontId::new(14.5, Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, egui::FontFamily::Monospace)),
        (TextStyle::Small, FontId::new(12.0, Proportional)),
    ]
    .into();

    let sp = &mut style.spacing;
    sp.item_spacing = egui::vec2(10.0, 9.0);
    sp.button_padding = egui::vec2(14.0, 8.0);
    sp.interact_size.y = 32.0; // 更舒适的输入框/按钮高度
    sp.slider_width = 180.0;
    sp.menu_margin = egui::Margin::same(8);
    sp.indent = 18.0;
    sp.text_edit_width = 320.0;

    let r = egui::CornerRadius::same(9);
    let mk = |bg: egui::Color32, weak: egui::Color32, stroke: egui::Color32, sw: f32, fg: egui::Color32, exp: f32| {
        egui::style::WidgetVisuals {
            bg_fill: bg,
            weak_bg_fill: weak,
            bg_stroke: egui::Stroke::new(sw, stroke),
            corner_radius: r,
            fg_stroke: egui::Stroke::new(1.0, fg),
            expansion: exp,
        }
    };

    let mut v = egui::Visuals::light();
    v.dark_mode = false;
    v.weak_text_color = Some(pal::TEXT_WEAK);
    v.window_fill = pal::SURFACE;
    v.panel_fill = pal::PANEL;
    v.extreme_bg_color = pal::FIELD; // 文本框背景
    v.faint_bg_color = pal::FIELD; // 斑马纹
    v.code_bg_color = pal::PANEL;
    v.warn_fg_color = pal::WARN;
    v.error_fg_color = pal::ERROR;
    v.hyperlink_color = pal::ACCENT;
    v.window_corner_radius = egui::CornerRadius::same(12);
    v.window_stroke = egui::Stroke::new(1.0, pal::STROKE);
    v.window_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: egui::Color32::from_black_alpha(30),
    };
    v.popup_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: egui::Color32::from_black_alpha(28),
    };
    v.menu_corner_radius = egui::CornerRadius::same(10);
    v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(13, 148, 136, 48);
    v.selection.stroke = egui::Stroke::new(1.6, pal::ACCENT); // 聚焦时清晰的青绿描边
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.image_loading_spinners = true;

    // 输入框背景：白卡上用浅灰底，配合清晰边框，明显是“可输入”
    v.text_edit_bg_color = Some(pal::FIELD);
    // 静态：1.2px 中灰边框，输入框/按钮都有清晰边界；悬停/聚焦更明显
    v.widgets.noninteractive = mk(pal::SURFACE, pal::SURFACE, pal::STROKE, 1.0, pal::TEXT, 0.0);
    v.widgets.inactive = mk(pal::SURFACE, pal::BTN, pal::STROKE_HI, 1.2, pal::TEXT, 0.0);
    v.widgets.hovered = mk(pal::SURFACE_HI, pal::BTN_HI, pal::STROKE_STRONG, 1.4, pal::TEXT, 1.0);
    v.widgets.active = mk(pal::ACCENT_DK, pal::ACCENT_DK, pal::ACCENT_DK, 1.4, pal::ON_ACCENT, 1.0);
    v.widgets.open = mk(pal::SURFACE_HI, pal::BTN_HI, pal::STROKE_STRONG, 1.2, pal::TEXT, 0.0);

    style.visuals = v;
    ctx.set_global_style(style);
}

// 卡片外观：用于把内容包成带圆角/描边/留白的“卡片”。
fn card_frame() -> egui::Frame {
    egui::Frame {
        inner_margin: egui::Margin::same(14),
        outer_margin: egui::Margin::same(0),
        fill: pal::SURFACE,
        stroke: egui::Stroke::new(1.0, pal::STROKE),
        corner_radius: egui::CornerRadius::same(12),
        shadow: egui::Shadow {
            offset: [0, 1],
            blur: 8,
            spread: 0,
            color: egui::Color32::from_black_alpha(18),
        },
    }
}

// 面板外观：纯色填充 + 内边距，无描边/阴影（除非传入）。
fn panel_frame(fill: egui::Color32, mx: i8, my: i8, shadow: egui::Shadow) -> egui::Frame {
    egui::Frame {
        inner_margin: egui::Margin::symmetric(mx, my),
        outer_margin: egui::Margin::same(0),
        fill,
        stroke: egui::Stroke::NONE,
        corner_radius: egui::CornerRadius::ZERO,
        shadow,
    }
}

// 设置表单左侧固定宽度、左对齐标签，保证各行输入框左边缘对齐。
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(92.0, 28.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.label(egui::RichText::new(text).color(pal::TEXT));
        },
    );
}

// 顶部完成横幅
fn banner(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
    egui::Frame {
        inner_margin: egui::Margin::symmetric(12, 6),
        outer_margin: egui::Margin::same(0),
        fill: tint(color, 28),
        stroke: egui::Stroke::new(1.0, tint(color, 80)),
        corner_radius: egui::CornerRadius::same(8),
        shadow: egui::Shadow::NONE,
    }
    .show(ui, |ui| {
        ui.label(egui::RichText::new(text).color(color).strong());
    });
}

// 状态徽章（圆角彩色小标签）。
fn stage_chip(ui: &mut egui::Ui, stage: Stage) {
    let color = stage.color();
    egui::Frame {
        inner_margin: egui::Margin::symmetric(8, 2),
        outer_margin: egui::Margin::same(0),
        fill: tint(color, 38),
        stroke: egui::Stroke::new(1.0, tint(color, 90)),
        corner_radius: egui::CornerRadius::same(7),
        shadow: egui::Shadow::NONE,
    }
    .show(ui, |ui| {
        ui.label(
            egui::RichText::new(format!("{} {}", stage.icon(), stage.label()))
                .color(color)
                .small(),
        );
    });
}

// 把强调色按 alpha 调淡，做徽章/选中底色。
fn tint(c: egui::Color32, alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

// 计数徽章（顶部成功/跳过/失败）。
fn count_chip(ui: &mut egui::Ui, text: String, color: egui::Color32) {
    egui::Frame {
        inner_margin: egui::Margin::symmetric(10, 3),
        outer_margin: egui::Margin::same(0),
        fill: tint(color, 30),
        stroke: egui::Stroke::new(1.0, tint(color, 70)),
        corner_radius: egui::CornerRadius::same(8),
        shadow: egui::Shadow::NONE,
    }
    .show(ui, |ui| {
        ui.label(egui::RichText::new(text).color(color).small());
    });
}

// 青绿描边的次按钮（打开原图 / 打开结果）。
fn open_btn(ui: &mut egui::Ui, label: &str) -> bool {
    let btn = egui::Button::new(egui::RichText::new(label).color(pal::ACCENT).strong())
        .fill(tint(pal::ACCENT, 30))
        .stroke(egui::Stroke::new(1.0, tint(pal::ACCENT, 120)));
    ui.add(btn).clicked()
}

// “处理后”单元在无结果时的占位文字（并排模式）。
fn empty_after(ui: &mut egui::Ui, stage: Stage) {
    match stage {
        Stage::Failed => {
            ui.label(egui::RichText::new("✗ 处理失败").color(pal::ERROR));
        }
        Stage::Skipped => {
            ui.label(egui::RichText::new("已跳过").color(pal::WARN));
        }
        Stage::Done => {
            ui.label(egui::RichText::new("无输出").color(pal::TEXT_WEAK));
        }
        _ => {
            ui.spinner();
            ui.label(egui::RichText::new("尚未生成…").color(pal::TEXT_WEAK).small());
        }
    }
}

// 滑动模式无结果时的占位（直接画在 painter 上）。
fn empty_after_painter(painter: &egui::Painter, rect: egui::Rect, stage: Stage) {
    let (txt, col) = match stage {
        Stage::Failed => ("✗ 处理失败", pal::ERROR),
        Stage::Skipped => ("已跳过", pal::WARN),
        Stage::Done => ("无输出", pal::TEXT_WEAK),
        _ => ("尚未生成…", pal::TEXT_WEAK),
    };
    painter.text(
        egui::pos2(rect.center().x + rect.width() * 0.25, rect.center().y),
        egui::Align2::CENTER_CENTER,
        txt,
        egui::FontId::proportional(14.0),
        col,
    );
}

// 对比预览中的单个图片卡片（处理前 / 处理后），用于并排模式。
fn preview_cell(
    ui: &mut egui::Ui,
    title: &str,
    tex: &Option<egui::TextureHandle>,
    cell: egui::Vec2,
    empty: impl FnOnce(&mut egui::Ui),
) {
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new(title).color(pal::TEXT).strong());
            ui.add_space(8.0);
            match tex {
                Some(t) => {
                    ui.add(
                        egui::Image::new(t)
                            .max_size(cell)
                            .maintain_aspect_ratio(true)
                            .corner_radius(8.0),
                    );
                }
                None => {
                    ui.add_space((cell.y / 2.0 - 16.0).max(0.0));
                    empty(ui);
                }
            }
        });
    });
}

// 纹理宽高比
fn tex_aspect(t: &egui::TextureHandle) -> f32 {
    let s = t.size_vec2();
    if s.y > 0.0 {
        s.x / s.y
    } else {
        1.0
    }
}

// 在外框内按宽高比求“适应”矩形
fn fit_rect(outer: egui::Rect, aspect: f32) -> egui::Rect {
    let (ow, oh) = (outer.width(), outer.height());
    let mut w = ow;
    let mut h = ow / aspect;
    if h > oh {
        h = oh;
        w = oh * aspect;
    }
    egui::Rect::from_center_size(outer.center(), egui::vec2(w, h))
}

// 把基础矩形按 zoom 缩放并平移到目标中心
fn scaled_rect(base: egui::Rect, center: egui::Pos2, zoom: f32) -> egui::Rect {
    egui::Rect::from_center_size(center, base.size() * zoom)
}

fn uv_full() -> egui::Rect {
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
}

// 日志按前缀着色。
fn log_color(line: &str) -> egui::Color32 {
    let t = line.trim_start();
    if t.starts_with('✓') {
        pal::SUCCESS
    } else if t.starts_with('✗') || t.starts_with('❌') {
        pal::ERROR
    } else if t.starts_with('⏭') || t.starts_with('⚠') || t.starts_with('⏹') {
        pal::WARN
    } else if t.starts_with('ℹ') || t.starts_with('▶') {
        pal::INFO
    } else {
        pal::TEXT_WEAK
    }
}

// 日志是否匹配级别过滤。
fn log_matches_filter(line: &str, f: LogFilter) -> bool {
    if f == LogFilter::All {
        return true;
    }
    let c = log_color(line);
    match f {
        LogFilter::All => true,
        LogFilter::Success => c == pal::SUCCESS,
        LogFilter::Error => c == pal::ERROR,
        LogFilter::Warn => c == pal::WARN,
        LogFilter::Info => c == pal::INFO || c == pal::TEXT_WEAK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_item(name: &str, input: &str, stage: Stage, outputs: Vec<&str>, task_id: &str, detail: &str) -> Item {
        Item {
            name: name.into(),
            input: PathBuf::from(input),
            stage,
            detail: detail.into(),
            outputs: outputs.into_iter().map(PathBuf::from).collect(),
            task_id: task_id.into(),
            in_tex: None,
            out_tex: None,
            list_tex: None,
            in_req: false,
            out_req: false,
            list_req: false,
        }
    }

    #[test]
    fn existing_outputs_skip_logic() {
        // 断点续跑核心：输出目录里有 `原名_rh*` 的应被识别为“已处理”。
        let dir = std::env::temp_dir().join("vc_skip_test");
        let _ = std::fs::create_dir_all(&dir);
        let d = dir.to_str().unwrap();
        std::fs::write(dir.join("1V3A3146_rh.png"), b"x").unwrap();
        std::fs::write(dir.join("photo_rh_1.png"), b"x").unwrap();
        std::fs::write(dir.join("_manifest.csv"), b"x").unwrap();

        assert!(!existing_outputs(d, "1V3A3146").is_empty(), "有结果 → 应跳过");
        assert!(!existing_outputs(d, "photo").is_empty(), "多输出命名也应匹配");
        assert!(existing_outputs(d, "1V3A3145").is_empty(), "无结果 → 应处理");
        assert!(existing_outputs(d, "1V3A314").is_empty(), "前缀部分匹配不应误判");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_value_typing() {
        // 数字原值 → 解析为数字；非法 → 退化为字符串。
        assert_eq!(edit_to_json_value("123456", &serde_json::json!(0)), serde_json::json!(123456i64));
        assert_eq!(edit_to_json_value("1.5", &serde_json::json!(0.0)), serde_json::json!(1.5));
        assert_eq!(
            edit_to_json_value("8K高清", &serde_json::json!("")),
            serde_json::json!("8K高清")
        );
    }

    #[test]
    fn store_migration_and_roundtrip() {
        // 旧格式（裸 UiConfig）→ 自动包成 1 个预设
        let old = r#"{"api_key":"k","workflow_id":"w","concurrency":3}"#;
        let s = parse_store(old);
        assert_eq!(s.presets.len(), 1);
        assert_eq!(s.presets[0].cfg.api_key, "k");
        assert_eq!(s.presets[0].cfg.workflow_id, "w");
        assert_eq!(s.presets[0].cfg.concurrency, 3);

        // 新格式 round-trip
        let mut store = Store::default();
        store.presets.push(Preset { name: "4K放大".into(), cfg: UiConfig::default() });
        store.current = 1;
        store.left_w = 300.0;
        let txt = serde_json::to_string(&store).unwrap();
        let back = parse_store(&txt);
        assert_eq!(back.presets.len(), 2);
        assert_eq!(back.presets[1].name, "4K放大");
        assert_eq!(back.current, 1);
        assert_eq!(back.left_w, 300.0);

        // 垃圾 → 默认
        let def = parse_store("not json");
        assert_eq!(def.presets.len(), 1);
    }

    #[test]
    fn store_normalize_clamps() {
        let s = Store { presets: vec![], current: 9, win_w: 1.0, win_h: -3.0, left_w: 5.0, logs_h: 0.0, ..Default::default() }.normalized();
        assert_eq!(s.presets.len(), 1, "空预设应补一个默认");
        assert_eq!(s.current, 0, "current 越界应夹紧");
        assert!(s.win_w >= 640.0 && s.win_h >= 480.0 && s.left_w >= 160.0 && s.logs_h >= 60.0);
    }

    #[test]
    fn manifest_from_items_statuses() {
        let dir = std::env::temp_dir().join("vc_manifest_test");
        let _ = std::fs::create_dir_all(&dir);
        let items = vec![
            mk_item("a", r"C:\in\a.png", Stage::Done, vec![r"C:\out\a_rh.png"], "t1", ""),
            mk_item("b", r"C:\in\b.png", Stage::Skipped, vec![r"C:\out\b_rh.png"], "", ""),
            mk_item("c", r"C:\in\c.png", Stage::Failed, vec![], "", "任务超时"),
            mk_item("d", r"C:\in\d.png", Stage::Done, vec![], "t2", ""),
        ];
        let p = write_manifest(dir.to_str().unwrap(), &items).unwrap();
        let csv = std::fs::read_to_string(&p).unwrap();
        assert!(csv.starts_with('\u{feff}'), "应有 UTF-8 BOM");
        assert!(csv.contains("a_rh.png") && csv.contains(",ok,"), "成功行");
        assert!(csv.contains(",skipped,"), "跳过行");
        assert!(csv.contains("error: 任务超时"), "失败行带原因");
        assert!(csv.contains(",no_output,"), "Done 但无输出 → no_output");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_filter_matches() {
        assert!(ListFilter::All.matches(Stage::Failed));
        assert!(ListFilter::Failed.matches(Stage::Failed));
        assert!(!ListFilter::Failed.matches(Stage::Done));
        assert!(ListFilter::Ok.matches(Stage::Done));
        assert!(ListFilter::Skipped.matches(Stage::Skipped));
        assert!(ListFilter::Active.matches(Stage::Uploading));
        assert!(ListFilter::Active.matches(Stage::Queued));
        assert!(!ListFilter::Active.matches(Stage::Done));
    }

    #[test]
    fn log_filter_classification() {
        assert!(log_matches_filter("✓ 完成：x", LogFilter::Success));
        assert!(!log_matches_filter("✓ 完成：x", LogFilter::Error));
        assert!(log_matches_filter("✗ 失败：x", LogFilter::Error));
        assert!(log_matches_filter("⚠ 注意", LogFilter::Warn));
        assert!(log_matches_filter("任何东西", LogFilter::All));
    }

    #[test]
    fn coins_value_parse() {
        let mut a = AccountInfo::default();
        a.remain_coins = Some("12.5".into());
        assert_eq!(coins_value(&a), Some(12.5));
        a.remain_coins = Some("?".into());
        assert_eq!(coins_value(&a), None);
    }

    #[test]
    fn failed_indices_and_counts() {
        let mut app = App::new_headless();
        app.items = vec![
            mk_item("a", "a", Stage::Done, vec![], "", ""),
            mk_item("b", "b", Stage::Failed, vec![], "", "e"),
            mk_item("c", "c", Stage::Skipped, vec![], "", ""),
            mk_item("d", "d", Stage::Failed, vec![], "", "e"),
        ];
        assert_eq!(app.failed_indices(), vec![1, 3]);
        let (done, ok, skip, fail) = app.counts();
        assert_eq!((done, ok, skip, fail), (4, 1, 1, 2));
    }

    #[test]
    fn start_run_validation_blocks_empty() {
        // 空配置点开始 → 记录“还有参数没填”，不启动后台线程
        let mut app = App::new_headless();
        let ctx = egui::Context::default();
        app.start_full(&ctx);
        assert!(!app.running, "校验失败不应进入运行态");
        assert!(
            app.logs.iter().any(|l| l.contains("还有参数没填")),
            "应提示缺参数，实得：{:?}",
            app.logs
        );
    }

    #[test]
    fn handle_drop_folder_and_images() {
        let base = std::env::temp_dir().join("vc_drop_test");
        let _ = std::fs::create_dir_all(&base);
        let img = base.join("p.png");
        std::fs::write(&img, b"x").unwrap();
        let txt = base.join("note.txt");
        std::fs::write(&txt, b"x").unwrap();

        let mut app = App::new_headless();
        // 拖文件夹 → 设输入目录
        app.handle_drop(vec![egui::DroppedFile { path: Some(base.clone()), ..Default::default() }]);
        assert_eq!(app.cfg.input_dir, base.display().to_string());
        // 拖图片 → 追加；拖 txt → 忽略
        app.handle_drop(vec![
            egui::DroppedFile { path: Some(img.clone()), ..Default::default() },
            egui::DroppedFile { path: Some(txt.clone()), ..Default::default() },
        ]);
        assert_eq!(app.extra_files, vec![img.clone()], "只追加图片，忽略 txt");
        // 重复拖同一张 → 不重复
        app.handle_drop(vec![egui::DroppedFile { path: Some(img.clone()), ..Default::default() }]);
        assert_eq!(app.extra_files.len(), 1, "去重");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn param_overrides_build_correct_json() {
        let mut app = App::new_headless();
        app.detected_params = vec![
            workflow::DetectedParam {
                node_id: "605".into(),
                node_type: "KSampler".into(),
                field: "seed".into(),
                label: "种子".into(),
                value: serde_json::json!(0),
            },
            workflow::DetectedParam {
                node_id: "586".into(),
                node_type: "CLIPTextEncode".into(),
                field: "text".into(),
                label: "提示词".into(),
                value: serde_json::json!(""),
            },
        ];
        // 只覆盖第一个（种子），第二个不勾
        app.param_on = vec![true, false];
        app.param_val = vec!["123456".into(), "改了但没勾".into()];
        let ov = app.param_override_values();
        assert_eq!(ov.len(), 1, "只产出勾选的项");
        assert_eq!(ov[0]["nodeId"], "605");
        assert_eq!(ov[0]["fieldName"], "seed");
        assert_eq!(ov[0]["fieldValue"], serde_json::json!(123456i64), "数字按数字注入");
    }

    #[test]
    fn wedding_init_builds_manifest() {
        // C 模式建账：只收图片、写回调色，建出 manifest。
        let base = std::env::temp_dir().join("vc_w_init_test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.jpg"), b"x").unwrap();
        std::fs::write(base.join("b.png"), b"x").unwrap();
        std::fs::write(base.join("note.txt"), b"x").unwrap();
        let mut app = App::new_headless();
        app.w_input_dir = base.display().to_string();
        app.w_tone = "warm".into();
        app.w_init();
        let m = app.w_manifest.as_ref().expect("应建账成功");
        assert_eq!(m.jobs.len(), 2, "只收图片、忽略 txt");
        assert_eq!(m.key, "warm");
        assert_eq!(app.w_sel, Some(0));
        let _ = std::fs::remove_dir_all(&base);
    }
}

// ============ UI 层测试（egui_kittest 模拟点击/渲染真实控件树）============
#[cfg(test)]
mod ui_tests {
    use super::*;
    use egui_kittest::{kittest::Queryable, Harness};

    fn harness<'a>() -> Harness<'a, App> {
        let mut h = Harness::builder()
            .with_size(egui::vec2(1280.0, 920.0))
            .build_ui_state(|ui, app: &mut App| app.render(ui), App::new_headless());
        h.run();
        h
    }

    #[test]
    fn renders_core_widgets() {
        let h = harness();
        // 主操作按钮 + 表单关键标签都应出现在控件树里
        // 用精确按钮文案，避免与空列表占位语里同样含“开始批量处理”的文字冲突
        assert!(
            h.query_by_label("▶  开始批量处理").is_some(),
            "应渲染“开始批量处理”按钮"
        );
        assert!(h.query_by_label("API Key").is_some(), "应渲染 API Key 标签");
        assert!(h.query_by_label("工作流 ID").is_some(), "应渲染 工作流 ID 标签");
    }

    #[test]
    fn click_start_with_empty_config_logs_validation() {
        let mut h = harness();
        h.get_by_label("▶  开始批量处理").click();
        h.run();
        assert!(
            h.state().logs.iter().any(|l| l.contains("还有参数没填")),
            "点开始后应有缺参数提示，实得：{:?}",
            h.state().logs
        );
        assert!(!h.state().running);
    }

    #[test]
    fn toggle_settings_hides_form() {
        let mut h = harness();
        assert!(h.query_by_label("API Key").is_some(), "默认展开设置");
        h.get_by_label_contains("参数设置").click();
        h.run();
        assert!(!h.state().show_settings, "点击后应收起设置");
        assert!(
            h.query_by_label("API Key").is_none(),
            "收起后表单标签应消失"
        );
    }

    #[test]
    fn new_and_delete_preset_via_buttons() {
        let mut h = harness();
        assert_eq!(h.state().store.presets.len(), 1);
        h.get_by_label_contains("新建").click();
        h.run();
        assert_eq!(h.state().store.presets.len(), 2, "新建后应有 2 个预设");
        assert_eq!(h.state().store.current, 1, "应切到新预设");
        h.get_by_label_contains("删除").click();
        h.run();
        assert_eq!(h.state().store.presets.len(), 1, "删除后回到 1 个");
        assert_eq!(h.state().store.current, 0);
    }

    fn item(name: &str, stage: Stage) -> Item {
        Item {
            name: name.into(),
            input: PathBuf::from(format!("{name}.png")),
            stage,
            detail: String::new(),
            outputs: Vec::new(),
            task_id: String::new(),
            in_tex: None,
            out_tex: None,
            list_tex: None,
            in_req: false,
            out_req: false,
            list_req: false,
        }
    }

    #[test]
    fn list_filter_click_failed() {
        let mut h = harness();
        // 收起设置（与真实“看列表”场景一致），让左侧列表有完整高度
        h.state_mut().show_settings = false;
        h.state_mut().items = vec![item("a", Stage::Done), item("b", Stage::Failed)];
        h.run();
        // 列表筛选条里的“失败”按钮（精确文案，不与状态徽章“✗ 失败”冲突）
        h.get_by_label("失败").click();
        h.run();
        assert!(matches!(h.state().list_filter, ListFilter::Failed));
    }

    #[test]
    fn compare_mode_toggle_to_wipe() {
        let mut h = harness();
        h.state_mut().show_settings = false; // 收起设置，给中央对比区完整高度（同 list_filter 测试）
        h.state_mut().items = vec![item("a", Stage::Done)];
        h.state_mut().selected = Some(0);
        h.run();
        assert!(matches!(h.state().cmp_mode, CmpMode::Side), "默认并排");
        h.get_by_label("滑动对比").click();
        h.run();
        assert!(matches!(h.state().cmp_mode, CmpMode::Wipe), "点击后切到滑动对比");
    }

    #[test]
    fn retry_failed_button_shows_count() {
        let mut h = harness();
        h.state_mut().items = vec![item("b", Stage::Failed), item("c", Stage::Failed)];
        h.run();
        assert!(
            h.query_by_label_contains("重试失败项 (2)").is_some(),
            "有 2 个失败项时应出现“重试失败项 (2)”按钮"
        );
    }

    #[test]
    fn wedding_mode_switch_renders() {
        let mut h = harness();
        h.state_mut().show_settings = false;
        h.state_mut().mode = AppMode::Wedding;
        h.run();
        // 用唯一的 field_label 断言（C 模式配置表单已渲染）
        assert!(h.query_by_label("原片素材").is_some(), "应渲染 C 模式原片素材");
        assert!(h.query_by_label("输出文件夹").is_some(), "应渲染 C 模式输出文件夹");
        assert!(h.query_by_label("场景库").is_some(), "应渲染场景库状态行");
    }
}

fn setup_cjk_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
        "C:/Windows/Fonts/msyh.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "cjk".to_owned());
            break;
        }
    }
    ctx.set_fonts(fonts);
}
