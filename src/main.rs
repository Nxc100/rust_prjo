// 视觉海岸批量处理工作台
// —— RunningHub 工作流批量自动化（上传 → 提交 → 轮询 → 下载）的 Windows 桌面程序。
// 功能对齐命令行脚本 rh_batch.py，并提供：分图进度阶段、处理前/后对比预览、实时日志、
// 断点续跑、并发与重试、账户状态、处理清单 _manifest.csv、汇总统计。
//
// 发布版隐藏控制台黑窗；debug 版保留控制台便于看输出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(non_snake_case)]

mod api;
mod workflow;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{CreateOutcome, PollState, RhClient, RhSettings};

const APP_TITLE: &str = "视觉海岸批量处理工作台";
const CONFIG_FILE: &str = "vc_batch_config.json";
const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "bmp"];
const THUMB_MAX: u32 = 1100; // 预览缩略图最长边像素

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
    pub const STROKE: Color32 = Color32::from_rgb(224, 229, 236); // 细边框
    pub const STROKE_HI: Color32 = Color32::from_rgb(199, 207, 217);
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
        }
    }
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(CONFIG_FILE)))
        .unwrap_or_else(|| PathBuf::from(CONFIG_FILE))
}
fn load_config() -> UiConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn save_config(c: &UiConfig) {
    if let Ok(s) = serde_json::to_string_pretty(c) {
        let _ = std::fs::write(config_path(), s);
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

#[derive(Clone, Copy, PartialEq)]
enum Which {
    Input,
    Output,
}

// ============ 后台→UI 的消息 ============
enum Msg {
    Files(Vec<(String, PathBuf)>),
    Stage { idx: usize, stage: Stage, detail: String },
    Outputs { idx: usize, paths: Vec<PathBuf> },
    Log(String),
    Thumb { idx: usize, which: Which, w: usize, h: usize, rgba: Vec<u8> },
    Finished,
}

// ============ 单张图片状态（UI 侧）============
struct Item {
    name: String,
    input: PathBuf,
    stage: Stage,
    detail: String,
    outputs: Vec<PathBuf>,
    in_tex: Option<egui::TextureHandle>,
    out_tex: Option<egui::TextureHandle>,
    in_req: bool,
    out_req: bool,
}

// ============ 跑批配置快照 ============
#[derive(Clone)]
struct BatchConfig {
    cfg: UiConfig,
    input_node: String,
    output_node: Option<String>,
    extra_overrides: Vec<serde_json::Value>,
}

// ============ 应用状态 ============
struct App {
    cfg: UiConfig,
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            cfg: load_config(),
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
        }
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

    fn start(&mut self, ctx: &egui::Context) {
        self.logs.clear();
        // 清空旧通道里的残留消息
        while self.rx.try_recv().is_ok() {}

        // 1) 必填校验
        let missing: Vec<&str> = [
            ("apiKey", self.cfg.api_key.trim().is_empty()),
            ("workflowId", self.cfg.workflow_id.trim().is_empty()),
            ("输入文件夹", self.cfg.input_dir.trim().is_empty()),
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

        let extra_overrides = match parse_extra_overrides(&self.cfg.extra_overrides) {
            Ok(v) => v,
            Err(e) => {
                self.logs.push(format!("❌ 额外节点参数 JSON 无效：{e}"));
                return;
            }
        };

        save_config(&self.cfg);

        // 3) 重置运行状态并启动后台
        self.stop = Arc::new(AtomicBool::new(false));
        self.running = true;
        self.items.clear();
        self.selected = None;
        self.follow = true;
        self.show_settings = false; // 跑起来后自动收起设置，腾出空间看进度/对比

        let batch = BatchConfig {
            cfg: self.cfg.clone(),
            input_node,
            output_node,
            extra_overrides,
        };
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || run_batch(batch, tx, stop, ctx2));
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
                            in_tex: None,
                            out_tex: None,
                            in_req: false,
                            out_req: false,
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
                    }
                }
                Msg::Outputs { idx, paths } => {
                    if let Some(it) = self.items.get_mut(idx) {
                        it.outputs = paths;
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
                        format!("thumb-{idx}-{}", which == Which::Input),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    if let Some(it) = self.items.get_mut(idx) {
                        match which {
                            Which::Input => it.in_tex = Some(tex),
                            Which::Output => it.out_tex = Some(tex),
                        }
                    }
                }
                Msg::Finished => self.running = false,
            }
        }
    }

    // 为选中项按需请求缩略图解码（后台线程解码，避免卡 UI）
    fn request_thumbs(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.selected else { return };
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
            spawn_decode(self.tx.clone(), ctx.clone(), idx, Which::Input, in_path);
        }
        if need_out {
            if let Some(p) = out_path {
                self.items[idx].out_req = true;
                spawn_decode(self.tx.clone(), ctx.clone(), idx, Which::Output, p);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain(&ctx);
        self.request_thumbs(&ctx);

        let header_shadow = egui::Shadow {
            offset: [0, 3],
            blur: 10,
            spread: 0,
            color: egui::Color32::from_black_alpha(22),
        };
        egui::Panel::top("header")
            .frame(panel_frame(pal::SURFACE, 18, 12, header_shadow))
            .show_inside(ui, |ui| self.header(ui, &ctx));
        egui::Panel::bottom("logs")
            .resizable(true)
            .default_size(140.0)
            .frame(panel_frame(pal::BG, 14, 8, egui::Shadow::NONE))
            .show_inside(ui, |ui| self.log_panel(ui));
        egui::Panel::left("list")
            .resizable(true)
            .default_size(340.0)
            .frame(panel_frame(pal::PANEL, 12, 10, egui::Shadow::NONE))
            .show_inside(ui, |ui| self.list_panel(ui));
        egui::CentralPanel::default()
            .frame(panel_frame(pal::BG, 16, 14, egui::Shadow::NONE))
            .show_inside(ui, |ui| self.preview_panel(ui));

        if self.running {
            ctx.request_repaint_after(Duration::from_millis(150));
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
            });
        });
        ui.add_space(10.0);

        // 控制栏
        let mut want_start = false;
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

        // 进度
        let total = self.items.len();
        if total > 0 {
            ui.add_space(8.0);
            let (done, ok, skip, fail) = self.counts();
            let frac = done as f32 / total as f32;
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
            self.start(ctx);
        }
    }

    fn settings_form(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("form")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("API Key");
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.api_key)
                        .password(true)
                        .hint_text("头像 → API 调用 页面创建并复制")
                        .desired_width(460.0),
                );
                ui.end_row();

                ui.label("工作流 ID");
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.workflow_id)
                        .hint_text("网址 /workflow/ 后面那串数字")
                        .desired_width(460.0),
                );
                ui.end_row();

                ui.label("输入文件夹");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.input_dir).desired_width(380.0));
                    if ui.button("浏览…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.cfg.input_dir = p.display().to_string();
                        }
                    }
                });
                ui.end_row();

                ui.label("输出文件夹");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.output_dir).desired_width(380.0));
                    if ui.button("浏览…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.cfg.output_dir = p.display().to_string();
                        }
                    }
                });
                ui.end_row();

                ui.label("工作流 JSON");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.workflow_json)
                            .hint_text("用于自动识别输入/输出节点")
                            .desired_width(380.0),
                    );
                    if ui.button("浏览…").clicked() {
                        if let Some(p) =
                            rfd::FileDialog::new().add_filter("json", &["json"]).pick_file()
                        {
                            self.cfg.workflow_json = p.display().to_string();
                        }
                    }
                });
                ui.end_row();

                ui.label("并发数");
                ui.add(egui::Slider::new(&mut self.cfg.concurrency, 1..=8));
                ui.end_row();
            });

        ui.checkbox(
            &mut self.cfg.skip_processed,
            "跳过已处理的图片（断点续跑）—— 取消勾选则全部重新处理",
        );

        egui::CollapsingHeader::new("高级设置（节点 / 重试 / 超时）")
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
        ui.add_space(6.0);

        if self.items.is_empty() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("🗂").size(30.0).color(pal::TEXT_WEAK));
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("填好参数后点“开始批量处理”")
                        .color(pal::TEXT_WEAK)
                        .small(),
                );
            });
            return;
        }

        let mut clicked: Option<usize> = None;
        egui::ScrollArea::vertical()
            .id_salt("list_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for i in 0..self.items.len() {
                    let (name, stage, detail) = {
                        let it = &self.items[i];
                        (it.name.clone(), it.stage, it.detail.clone())
                    };
                    let selected = self.selected == Some(i);
                    let w = ui.available_width();
                    let resp = ui.add_sized(
                        [w, 26.0],
                        egui::Button::selectable(
                            selected,
                            egui::RichText::new(&name).color(pal::TEXT),
                        ),
                    );
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        stage_chip(ui, stage);
                        if stage.is_active() && !detail.is_empty() {
                            ui.label(
                                egui::RichText::new(&detail).color(pal::TEXT_WEAK).small(),
                            );
                        }
                    });
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some(i) = clicked {
            self.selected = Some(i);
            self.follow = false;
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
        });
        ui.add_space(10.0);

        let avail_w = ui.available_width();
        let avail_h = ui.available_height();
        let cell = egui::vec2((avail_w / 2.0 - 44.0).max(120.0), (avail_h - 92.0).max(160.0));

        ui.columns(2, |cols| {
            preview_cell(&mut cols[0], "处理前 · 原图", &in_tex, cell, |ui| {
                ui.spinner();
                ui.label(egui::RichText::new("加载中…").color(pal::TEXT_WEAK).small());
            });
            preview_cell(&mut cols[1], "处理后 · 结果", &out_tex, cell, |ui| {
                if has_out {
                    ui.spinner();
                    ui.label(egui::RichText::new("加载中…").color(pal::TEXT_WEAK).small());
                } else {
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
                            ui.label(
                                egui::RichText::new("尚未生成…").color(pal::TEXT_WEAK).small(),
                            );
                        }
                    }
                }
            });
        });

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
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("清空").clicked() {
                    self.logs.clear();
                }
            });
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .id_salt("logs_scroll")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.logs {
                    ui.label(egui::RichText::new(line).monospace().color(log_color(line)));
                }
            });
    }
}

// ============ 缩略图解码（后台线程）============
fn spawn_decode(tx: Sender<Msg>, ctx: egui::Context, idx: usize, which: Which, path: PathBuf) {
    std::thread::spawn(move || {
        match decode_thumb(&path) {
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

fn decode_thumb(path: &Path) -> anyhow::Result<(usize, usize, Vec<u8>)> {
    let img = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?;
    let img = img.thumbnail(THUMB_MAX, THUMB_MAX); // 等比快速缩放
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
            acc.current_task_counts.unwrap_or_else(|| "?".into()),
            acc.remain_coins.unwrap_or_else(|| "?".into())
        ));
    }

    // 收集图片
    let mut files: Vec<PathBuf> = Vec::new();
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
            log(format!("❌ 无法读取输入文件夹：{e}"));
            let _ = tx.send(Msg::Finished);
            ctx.request_repaint();
            return;
        }
    }
    files.sort();
    let total = files.len();
    if total == 0 {
        log("❌ 输入文件夹里没有图片（支持 png/jpg/jpeg/webp/bmp）".into());
        let _ = tx.send(Msg::Finished);
        ctx.request_repaint();
        return;
    }
    let _ = std::fs::create_dir_all(&b.cfg.output_dir);

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

    // 任务队列（带索引）
    let (jtx, jrx) = crossbeam_channel::unbounded::<(usize, PathBuf)>();
    for (i, f) in files.into_iter().enumerate() {
        let _ = jtx.send((i, f));
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
            let mut results: Vec<ProcessResult> = Vec::new();
            while let Ok((idx, path)) = jrx.recv() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("img")
                    .to_string();
                let input_str = path.display().to_string();

                // 断点续跑：已有结果则跳过，并把已存在的结果回显到“处理后”，方便直接对比
                if b.cfg.skip_processed {
                    let existing = existing_outputs(&b.cfg.output_dir, &name);
                    if !existing.is_empty() {
                        let _ = tx.send(Msg::Outputs {
                            idx,
                            paths: existing.clone(),
                        });
                        let _ = tx.send(Msg::Stage {
                            idx,
                            stage: Stage::Skipped,
                            detail: "已存在结果".into(),
                        });
                        let _ = tx.send(Msg::Log(format!("⏭ 跳过（已存在）：{name}")));
                        results.push(ProcessResult {
                            input: input_str,
                            status: "skipped".into(),
                            task_id: String::new(),
                            outputs: existing
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(";"),
                        });
                        ctx.request_repaint();
                        continue;
                    }
                }

                match process_one(&client, &b, idx, &path, &name, &stop, &tx, &ctx) {
                    Ok((paths, task_id)) => {
                        let _ = tx.send(Msg::Outputs {
                            idx,
                            paths: paths.clone(),
                        });
                        let _ = tx.send(Msg::Stage {
                            idx,
                            stage: Stage::Done,
                            detail: format!("{} 张结果", paths.len()),
                        });
                        let _ = tx.send(Msg::Log(format!("✓ 完成：{name} → {} 张", paths.len())));
                        results.push(ProcessResult {
                            input: input_str,
                            status: if paths.is_empty() { "no_output" } else { "ok" }.into(),
                            task_id,
                            outputs: paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(";"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Stage {
                            idx,
                            stage: Stage::Failed,
                            detail: e.to_string(),
                        });
                        let _ = tx.send(Msg::Log(format!("✗ 失败：{name}：{e}")));
                        results.push(ProcessResult {
                            input: input_str,
                            status: format!("error: {e}"),
                            task_id: String::new(),
                            outputs: String::new(),
                        });
                    }
                }
                ctx.request_repaint();
            }
            results
        }));
    }
    let mut all_results: Vec<ProcessResult> = Vec::new();
    for h in handles {
        if let Ok(mut r) = h.join() {
            all_results.append(&mut r);
        }
    }

    match write_manifest(&b.cfg.output_dir, &all_results) {
        Ok(path) => log(format!("清单已写入：{path}")),
        Err(e) => log(format!("⚠ 写清单失败：{e}")),
    }

    let ok = all_results.iter().filter(|r| r.status == "ok").count();
    let sk = all_results.iter().filter(|r| r.status == "skipped").count();
    let bad = all_results.len() - ok - sk;
    let stopped = stop.load(Ordering::SeqCst);
    log(format!(
        "{}全部完成：成功 {ok}，跳过 {sk}，失败/无输出 {bad}。",
        if stopped { "⏹ 已停止，" } else { "" }
    ));

    let _ = tx.send(Msg::Finished);
    ctx.request_repaint();
}

#[derive(Clone)]
struct ProcessResult {
    input: String,
    status: String,
    task_id: String,
    outputs: String,
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

fn write_manifest(out_dir: &str, results: &[ProcessResult]) -> std::io::Result<String> {
    let path = Path::new(out_dir).join("_manifest.csv");
    let mut s = String::from("\u{feff}");
    s.push_str("input,status,task_id,outputs\n");
    for r in results {
        s.push_str(&csv_field(&r.input));
        s.push(',');
        s.push_str(&csv_field(&r.status));
        s.push(',');
        s.push_str(&csv_field(&r.task_id));
        s.push(',');
        s.push_str(&csv_field(&r.outputs));
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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            .with_inner_size([1180.0, 800.0])
            .with_min_inner_size([880.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

// 现代深色主题：字体（含中文）、字号、间距、配色、圆角、阴影。
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
    sp.item_spacing = egui::vec2(10.0, 10.0);
    sp.button_padding = egui::vec2(14.0, 7.0);
    sp.interact_size.y = 30.0;
    sp.slider_width = 180.0;
    sp.menu_margin = egui::Margin::same(8);
    sp.indent = 18.0;

    let r = egui::CornerRadius::same(9);
    let mk = |bg: egui::Color32, weak: egui::Color32, stroke: egui::Color32, fg: egui::Color32, exp: f32| {
        egui::style::WidgetVisuals {
            bg_fill: bg,
            weak_bg_fill: weak,
            bg_stroke: egui::Stroke::new(1.0, stroke),
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
    v.selection.stroke = egui::Stroke::new(1.0, pal::ACCENT);
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.image_loading_spinners = true;

    v.widgets.noninteractive = mk(pal::SURFACE, pal::SURFACE, pal::STROKE, pal::TEXT, 0.0);
    v.widgets.inactive = mk(pal::SURFACE, pal::BTN, pal::STROKE, pal::TEXT, 0.0);
    v.widgets.hovered = mk(pal::SURFACE_HI, pal::BTN_HI, pal::STROKE_HI, pal::TEXT, 1.0);
    v.widgets.active = mk(pal::ACCENT_DK, pal::ACCENT_DK, pal::ACCENT_DK, pal::ON_ACCENT, 1.0);
    v.widgets.open = mk(pal::SURFACE_HI, pal::BTN_HI, pal::STROKE_HI, pal::TEXT, 0.0);

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

// 对比预览中的单个图片卡片（处理前 / 处理后）。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_real_input_thumbnail() {
        // 验证缩略图解码链路（真实大图 → 等比缩放 → RGBA）。
        let p = std::path::Path::new(r"D:\ai_work\runninghub_api\input\1V3A2861.jpg");
        if !p.exists() {
            return; // 无样张时跳过
        }
        let (w, h, rgba) = decode_thumb(p).expect("应能解码 JPEG 缩略图");
        assert!(w > 0 && h > 0, "尺寸应为正");
        assert_eq!(rgba.len(), w * h * 4, "应为 RGBA 像素");
        assert!(w as u32 <= THUMB_MAX && h as u32 <= THUMB_MAX, "应已缩放到上限内");
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
