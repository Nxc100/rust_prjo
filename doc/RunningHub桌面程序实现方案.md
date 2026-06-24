# RunningHub 批量处理 · Windows 桌面程序实现方案（Rust + egui）

> 目标：把现有的 Python 批量脚本（读本地文件夹 → 上传 RunningHub → 跑 Flux2‑Klein 人像精修/高清放大 → 下载结果）做成一个 **Windows 桌面 GUI 程序**：填几个参数、点按钮、看进度条，全程不用碰命令行，编译出来是**一个自包含的 .exe**。

---

## 目录

1. [技术选型（为什么用 egui）](#1-技术选型为什么用-egui)
2. [整体架构与数据流](#2-整体架构与数据流)
3. [第一步：创建工程与依赖](#3-第一步创建工程与依赖)
4. [关键坑：中文显示](#4-关键坑中文显示)
5. [模块代码（可直接拼装）](#5-模块代码可直接拼装)
   - 5.1 `workflow.rs` 解析工作流、自动识别节点
   - 5.2 `api.rs` RunningHub 接口客户端
   - 5.3 `main.rs` 界面 + 后台线程 + 进度回传 + 并发 + 停止
6. [关键技术点说明](#6-关键技术点说明)
7. [打包成 exe / 加图标 / 做安装包](#7-打包成-exe--加图标--做安装包)
8. [分阶段实施里程碑（Checklist）](#8-分阶段实施里程碑checklist)
9. [测试与常见问题排查](#9-测试与常见问题排查)
10. [备选 GUI 方案对比](#10-备选-gui-方案对比)

---

## 1. 技术选型（为什么用 egui）

| 方案 | 语言/前端 | 产物 | 运行时依赖 | 上手难度 | 适合度 |
|---|---|---|---|---|---|
| **egui / eframe** ✅ | 纯 Rust | 单个 .exe | 无（GPU 即可） | ★ 低 | **工具类小软件最佳** |
| Tauri | Rust + HTML/CSS/JS | .exe + 资源 | 需 WebView2、Node 工具链 | ★★★ 高 | UI 要很漂亮时 |
| Slint | Rust + .slint DSL | 单个 .exe | 无 | ★★ 中 | 界面好看且纯 Rust |
| iced | 纯 Rust（Elm 风） | 单个 .exe | 无 | ★★ 中 | 偏好响应式架构 |

**结论：选 egui/eframe（当前 0.34）**。理由：

- 编译即**单文件 exe**，配合 `rustls` 不依赖 OpenSSL DLL，拷给别人双击就能用；
- 自带按钮、文本框、进度条、可滚动日志区、原生文件夹选择框（`rfd`），正好覆盖本工具所有 UI；
- 立即模式（immediate mode）写法直白：每帧重绘界面，后台用线程跑网络任务，通过 channel 把进度发回 UI 即可。

---

## 2. 整体架构与数据流

```
┌───────────────────────── UI 线程（egui，每帧重绘）─────────────────────────┐
│  表单：API Key / 工作流ID / 输入文件夹 / 输出文件夹 / 工作流JSON / 并发 / 覆盖  │
│  [▶ 开始]  →  校验 → 解析工作流识别节点(642/758) → 启动后台线程             │
│  进度条 + 日志区  ←──────────── crossbeam channel（Msg）────────────┐       │
└──────────────────────────────────────────────────────────────────│───────┘
                                                                    │
┌──────────────────────── 后台线程池（N 个 worker）─────────────────│───────┐
│  任务队列(crossbeam) ← 输入文件夹所有图片                           │       │
│  每个 worker 循环取一张：                                           │       │
│    upload_image() ─POST /task/openapi/upload─► 得到 fileName        │       │
│    create_task()  ─POST /task/openapi/create─► 注入到 LoadImage(642)│       │
│    poll_once()×N  ─POST /task/openapi/outputs─► 轮询直到出结果       │       │
│    download()     ─GET fileUrl─► 存到 输出文件夹/原名_rh.png         │       │
│  每完成一张 → 发 Msg::Progress / Msg::Log 回 UICELL ────────────────┘       │
└────────────────────────────────────────────────────────────────────────────┘
```

**RunningHub 接口**（base = `https://www.runninghub.cn`）：

| 用途 | 方法/路径 | 关键参数 | 返回 |
|---|---|---|---|
| 上传图片 | `POST /task/openapi/upload` | multipart: `apiKey` + `fileType=image` + `file` | `data.fileName`（形如 `api/xxx.png`） |
| 发起任务 | `POST /task/openapi/create` | JSON: `apiKey`,`workflowId`,`nodeInfoList[]` | `data.taskId` |
| 查状态/取结果 | `POST /task/openapi/outputs` | JSON: `apiKey`,`taskId` | 运行中→`APIKEY_TASK_IS_QUEUED/RUNNING`；完成→`data:[{fileUrl,nodeId,...}]` |
| 账户并发 | `POST /uc/openapi/accountStatus` | JSON: `apiKey` | `data.currentTaskCounts` |

> 节点说明：你的工作流里图片入口是 `LoadImage`（id **642**），结果出口是 `SaveImage`（id **758**），程序会从工作流 JSON 自动识别，无需手填。`nodeInfoList` 里 `fieldName` 用 `image`，把上传得到的 `fileName` 作为 `fieldValue` 注入即可。

---

## 3. 第一步：创建工程与依赖

```bat
cargo new rh_batch_gui
cd rh_batch_gui
```

`Cargo.toml`：

```toml
[package]
name = "rh_batch_gui"
version = "0.1.0"
edition = "2021"

[dependencies]
eframe  = "0.34"            # egui + 窗口/渲染（默认 wgpu 后端）
egui    = "0.34"
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "multipart", "rustls-tls"] }
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
rfd     = "0.15"           # 原生“选择文件夹/文件”对话框
anyhow  = "1"
crossbeam-channel = "0.5"  # 后台→UI 的消息通道、worker 任务队列

[profile.release]
opt-level = "z"            # 体积优先（可选）
lto = true
strip = true
```

> - 用 `rustls-tls` + `default-features = false`：纯 Rust 的 TLS，**不依赖 OpenSSL**，Windows 上免去环境配置，产物也干净。
> - 不确定最新版本时，直接 `cargo add eframe egui rfd anyhow serde serde_json crossbeam-channel` 会拉取当前版本（egui 在快速迭代，少数 API 可能与下文略有出入，按 `docs.rs/egui` 当前版调整即可，下面会标出 2~3 个版本敏感点）。

工程结构：

```
rh_batch_gui/
├── Cargo.toml
├── build.rs            # （第 7 节）给 exe 加图标，可后加
├── assets/app.ico      # （可选）图标
└── src/
    ├── main.rs         # 界面 + 后台编排
    ├── api.rs          # RunningHub 客户端
    └── workflow.rs     # 解析工作流、识别节点
```

---

## 4. 关键坑：中文显示

egui 默认字体**不含中文**，不处理的话所有中文会变成 □□□ 豆腐块。解决：启动时加载系统中文字体（微软雅黑等）。这段代码在 `main.rs` 里，**务必加**（见 5.3 的 `setup_cjk_font`）。

> 版本敏感点①：egui 0.28+ 的 `font_data` 需要用 `Arc::new(...)` 包裹 `FontData`。若你的版本编译报“类型不匹配”，把 `Arc::new(...)` 去掉即可。

---

## 5. 模块代码（可直接拼装）

> 下面三段是接近完整的实现，复制到对应文件、`cargo run` 即可起步。少量版本敏感行已用注释标出。

### 5.1 `src/workflow.rs`

```rust
// 解析 ComfyUI 工作流(UI 格式 JSON)，自动识别 LoadImage(输入) / SaveImage(输出) 节点
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::Path;

#[derive(Default, Clone)]
pub struct IoNodes {
    pub input_node: Option<String>,   // 唯一 LoadImage 时给出
    pub output_node: Option<String>,  // 唯一 SaveImage 时给出
    pub all_loads: Vec<String>,
    pub all_saves: Vec<String>,
}

pub fn detect_io_nodes(json_path: &Path) -> Result<IoNodes> {
    let text = std::fs::read_to_string(json_path)?;
    let wf: Value = serde_json::from_str(&text)?;
    let nodes = wf
        .get("nodes")
        .and_then(|n| n.as_array())
        .ok_or_else(|| anyhow!("不是有效的工作流 JSON（缺少 nodes 字段）"))?;

    let mut io = IoNodes::default();
    for n in nodes {
        let ty = n.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let id = match n.get("id") {
            Some(Value::Number(num)) => num.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        match ty {
            "LoadImage" => io.all_loads.push(id),
            "SaveImage" => io.all_saves.push(id),
            _ => {}
        }
    }
    io.input_node = (io.all_loads.len() == 1).then(|| io.all_loads[0].clone());
    io.output_node = (io.all_saves.len() == 1).then(|| io.all_saves[0].clone());
    Ok(io)
}
```

### 5.2 `src/api.rs`

```rust
// RunningHub OpenAPI 客户端（阻塞式，跑在后台线程；UI 线程绝不调用它）
#![allow(non_snake_case)]

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

pub const BASE: &str = "https://www.runninghub.cn";

#[derive(Clone)]
pub struct RhClient {
    api_key: String,
    http: reqwest::blocking::Client,
}

/// 提交任务结果：拿到 taskId，或被并发/队列限制挡住需重试
pub enum CreateOutcome {
    Task(String),
    Busy(String),
}

#[derive(Deserialize)]
struct Resp<T> {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    data: Option<T>,
}

#[derive(Deserialize)]
struct UploadData {
    fileName: String,
}

#[derive(Deserialize)]
struct CreateData {
    taskId: Option<String>,
    promptTips: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct OutputItem {
    pub fileUrl: String,
    #[serde(default)]
    pub fileType: String,
    #[serde(default)]
    pub nodeId: String,
}

impl RhClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self { api_key: api_key.into(), http })
    }

    /// 上传图片，返回 RunningHub 内部 fileName（形如 api/xxxx.png）
    pub fn upload_image(&self, path: &Path) -> Result<String> {
        let url = format!("{BASE}/task/openapi/upload");
        let fname = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image.png")
            .to_string();
        let mime = match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            _ => "image/png",
        };
        let bytes = std::fs::read(path)?;
        let part = reqwest::blocking::multipart::Part::bytes(bytes)
            .file_name(fname)
            .mime_str(mime)?;
        let form = reqwest::blocking::multipart::Form::new()
            .text("apiKey", self.api_key.clone())
            .text("fileType", "image")
            .part("file", part);

        let r: Resp<UploadData> = self.http.post(url).multipart(form).send()?.json()?;
        if r.code == 0 {
            if let Some(d) = r.data {
                return Ok(d.fileName);
            }
        }
        Err(anyhow!("上传失败 code={} msg={:?}", r.code, r.msg))
    }

    /// 提交任务（nodes 为 nodeInfoList 数组）
    pub fn create_task(
        &self,
        workflow_id: &str,
        nodes: &serde_json::Value,
        add_metadata: bool,
    ) -> Result<CreateOutcome> {
        let url = format!("{BASE}/task/openapi/create");
        let body = serde_json::json!({
            "apiKey": self.api_key,
            "workflowId": workflow_id,
            "addMetadata": add_metadata,
            "nodeInfoList": nodes,
        });
        let r: Resp<CreateData> = self.http.post(url).json(&body).send()?.json()?;
        if r.code == 0 {
            if let Some(d) = r.data {
                if let Some(id) = d.taskId {
                    let _ = d.promptTips; // 需要时可解析其中的 node_errors
                    return Ok(CreateOutcome::Task(id));
                }
            }
            return Err(anyhow!("提交成功但无 taskId"));
        }
        let msg = r.msg.unwrap_or_default();
        let up = msg.to_uppercase();
        let busy = ["QUEUE", "MAXED", "RUNNING", "CONCURRENT", "LIMIT", "BUSY"]
            .iter()
            .any(|k| up.contains(k));
        if busy {
            Ok(CreateOutcome::Busy(msg))
        } else {
            Err(anyhow!("提交失败 code={} msg={}", r.code, msg))
        }
    }

    /// 查询一次：完成→Some(结果列表)，仍在排队/运行→None
    pub fn poll_once(&self, task_id: &str) -> Result<Option<Vec<OutputItem>>> {
        let url = format!("{BASE}/task/openapi/outputs");
        let body = serde_json::json!({ "apiKey": self.api_key, "taskId": task_id });
        let v: serde_json::Value = self.http.post(url).json(&body).send()?.json()?;

        let code = v.get("code").and_then(|x| x.as_i64()).unwrap_or(-1);
        let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("").to_uppercase();
        let data = v.get("data");

        if code == 0 {
            if let Some(arr) = data.and_then(|d| d.as_array()) {
                if !arr.is_empty() {
                    let items: Vec<OutputItem> =
                        serde_json::from_value(serde_json::Value::Array(arr.clone()))?;
                    return Ok(Some(items));
                }
            }
            return Ok(None); // code=0 但还没产出，继续等
        }
        if msg.contains("QUEUED") || msg.contains("RUNNING") {
            return Ok(None);
        }
        Err(anyhow!("任务异常 code={} msg={}", code, msg))
    }

    pub fn download(&self, url: &str, out: &Path) -> Result<()> {
        let bytes = self.http.get(url).send()?.error_for_status()?.bytes()?;
        std::fs::write(out, &bytes)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn account_status(&self) -> Result<serde_json::Value> {
        let url = format!("{BASE}/uc/openapi/accountStatus");
        let body = serde_json::json!({ "apiKey": self.api_key });
        let v: serde_json::Value = self.http.post(url).json(&body).send()?.json()?;
        Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
    }
}
```

### 5.3 `src/main.rs`

```rust
// 发布版隐藏控制台黑窗；debug 版保留控制台便于看输出
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(non_snake_case)]

mod api;
mod workflow;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use api::{CreateOutcome, RhClient};

// ============ 可持久化的界面设置（保存上次填写）============
#[derive(Serialize, Deserialize, Clone)]
struct UiConfig {
    api_key: String,
    workflow_id: String,
    input_dir: String,
    output_dir: String,
    workflow_json: String,
    concurrency: usize,
    overwrite: bool,
}
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            workflow_id: String::new(),
            input_dir: String::new(),
            output_dir: String::new(),
            workflow_json: "Flux2-Klein人像精修_高清放大__2_.json".into(),
            concurrency: 1,
            overwrite: false,
        }
    }
}
fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rh_gui_config.json")))
        .unwrap_or_else(|| PathBuf::from("rh_gui_config.json"))
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

// ============ 后台→UI 的消息 ============
enum Msg {
    Log(String),
    Progress(usize, usize), // done, total
    Finished,
}

// ============ 跑批配置快照 ============
#[derive(Clone)]
struct BatchConfig {
    cfg: UiConfig,
    input_node: String,
    output_node: Option<String>,
}

// ============ 应用状态 ============
struct App {
    cfg: UiConfig,
    detected_in: String,
    detected_out: String,
    running: bool,
    done: usize,
    total: usize,
    logs: Vec<String>,
    rx: Option<Receiver<Msg>>,
    stop: Arc<AtomicBool>,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            cfg: load_config(),
            detected_in: String::new(),
            detected_out: String::new(),
            running: false,
            done: 0,
            total: 0,
            logs: Vec::new(),
            rx: None,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
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
            self.logs.push(format!("❌ 还有参数没填：{}", missing.join("、")));
            return;
        }

        // 2) 识别工作流节点
        let wf_path = resolve_workflow_path(&self.cfg.workflow_json);
        let (in_node, out_node) = match workflow::detect_io_nodes(&wf_path) {
            Ok(io) => {
                self.detected_in = io.input_node.clone().unwrap_or_default();
                self.detected_out = io.output_node.clone().unwrap_or_default();
                (io.input_node, io.output_node)
            }
            Err(e) => {
                self.logs.push(format!("❌ 读取工作流失败：{e}"));
                return;
            }
        };
        let input_node = match in_node {
            Some(n) => n,
            None => {
                self.logs
                    .push("❌ 未能确定 LoadImage 输入节点，请把工作流 .json 放到程序同目录".into());
                return;
            }
        };

        save_config(&self.cfg);

        // 3) 启动后台线程
        let (tx, rx) = crossbeam_channel::unbounded();
        self.stop = Arc::new(AtomicBool::new(false));
        self.rx = Some(rx);
        self.running = true;
        self.done = 0;
        self.total = 0;
        self.logs.clear();

        let batch = BatchConfig {
            cfg: self.cfg.clone(),
            input_node,
            output_node: out_node,
        };
        let stop = self.stop.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || run_batch(batch, tx, stop, ctx2));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 收后台消息
        if let Some(rx) = &self.rx {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Log(s) => self.logs.push(s),
                    Msg::Progress(d, t) => {
                        self.done = d;
                        self.total = t;
                    }
                    Msg::Finished => self.running = false,
                }
            }
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.heading("RunningHub 批量精修 / 高清放大");
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let mut want_start = false;

            egui::Grid::new("form")
                .num_columns(2)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.label("API Key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.api_key)
                            .password(true)
                            .desired_width(440.0),
                    );
                    ui.end_row();

                    ui.label("工作流 ID");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.workflow_id).desired_width(440.0));
                    ui.end_row();

                    ui.label("输入文件夹");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.input_dir).desired_width(360.0));
                        if ui.button("浏览…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                self.cfg.input_dir = p.display().to_string();
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("输出文件夹");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.output_dir).desired_width(360.0));
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
                            egui::TextEdit::singleline(&mut self.cfg.workflow_json).desired_width(360.0),
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
                &mut self.cfg.overwrite,
                "覆盖已处理（取消勾选 = 断点续跑，自动跳过已完成）",
            );

            if !self.detected_in.is_empty() {
                let out = if self.detected_out.is_empty() {
                    "全部输出".to_string()
                } else {
                    self.detected_out.clone()
                };
                ui.label(format!("识别到节点：输入={}  输出={}", self.detected_in, out));
            }

            ui.separator();
            ui.horizontal(|ui| {
                if !self.running {
                    if ui.button("▶ 开始批量处理").clicked() {
                        want_start = true;
                    }
                } else if ui.button("■ 停止").clicked() {
                    self.stop.store(true, Ordering::SeqCst);
                    self.logs.push("⏹ 已请求停止，等待当前任务结束…".into());
                }
            });

            if self.total > 0 {
                let frac = self.done as f32 / self.total as f32;
                ui.add(
                    egui::ProgressBar::new(frac).text(format!("{}/{}", self.done, self.total)),
                );
            }

            ui.separator();
            ui.label("日志");
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(220.0)
                .show(ui, |ui| {
                    for line in &self.logs {
                        ui.monospace(line);
                    }
                });

            if want_start {
                self.start(ctx);
            }
        });

        if self.running {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
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

// ============ 后台批处理（线程池）============
fn run_batch(b: BatchConfig, tx: Sender<Msg>, stop: Arc<AtomicBool>, ctx: egui::Context) {
    let send_log = |tx: &Sender<Msg>, ctx: &egui::Context, s: String| {
        let _ = tx.send(Msg::Log(s));
        ctx.request_repaint();
    };

    let client = match RhClient::new(b.cfg.api_key.clone()) {
        Ok(c) => c,
        Err(e) => {
            send_log(&tx, &ctx, format!("❌ 初始化失败：{e}"));
            let _ = tx.send(Msg::Finished);
            return;
        }
    };

    // 收集图片
    let exts = ["png", "jpg", "jpeg", "webp", "bmp"];
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&b.cfg.input_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                    if exts.contains(&ext.to_lowercase().as_str()) {
                        files.push(p);
                    }
                }
            }
        }
    }
    files.sort();
    let total = files.len();
    if total == 0 {
        send_log(&tx, &ctx, "❌ 输入文件夹里没有图片".into());
        let _ = tx.send(Msg::Finished);
        return;
    }
    let _ = std::fs::create_dir_all(&b.cfg.output_dir);
    send_log(&tx, &ctx, format!("共 {} 张待处理，并发 {}", total, b.cfg.concurrency));

    // 任务队列 + 计数
    let done = Arc::new(AtomicUsize::new(0));
    let (jtx, jrx) = crossbeam_channel::unbounded::<PathBuf>();
    for f in files {
        let _ = jtx.send(f);
    }
    drop(jtx);

    let n = b.cfg.concurrency.max(1);
    let mut handles = Vec::new();
    for _ in 0..n {
        let jrx = jrx.clone();
        let tx = tx.clone();
        let ctx = ctx.clone();
        let stop = stop.clone();
        let done = done.clone();
        let client = client.clone();
        let b = b.clone();
        handles.push(std::thread::spawn(move || {
            while let Ok(path) = jrx.recv() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("img")
                    .to_string();

                // 断点续跑
                if !b.cfg.overwrite && output_exists(&b.cfg.output_dir, &name) {
                    let _ = tx.send(Msg::Log(format!("⏭ 跳过（已存在）：{name}")));
                    let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                    let _ = tx.send(Msg::Progress(d, total));
                    ctx.request_repaint();
                    continue;
                }

                let r = process_one(&client, &b, &path, &name, &stop, &tx, &ctx);
                match r {
                    Ok(saved) => {
                        let _ = tx.send(Msg::Log(format!("✓ 完成：{name} → {saved} 张")));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Log(format!("✗ 失败：{name}：{e}")));
                    }
                }
                let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = tx.send(Msg::Progress(d, total));
                ctx.request_repaint();
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    send_log(&tx, &ctx, "全部结束".into());
    let _ = tx.send(Msg::Finished);
    ctx.request_repaint();
}

fn output_exists(out_dir: &str, name: &str) -> bool {
    if let Ok(rd) = std::fs::read_dir(out_dir) {
        let prefix = format!("{name}_rh");
        for e in rd.flatten() {
            if let Some(f) = e.file_name().to_str() {
                if f.starts_with(&prefix) {
                    return true;
                }
            }
        }
    }
    false
}

fn process_one(
    client: &RhClient,
    b: &BatchConfig,
    path: &Path,
    name: &str,
    stop: &Arc<AtomicBool>,
    tx: &Sender<Msg>,
    ctx: &egui::Context,
) -> anyhow::Result<usize> {
    let log = |s: String| {
        let _ = tx.send(Msg::Log(s));
        ctx.request_repaint();
    };

    log(format!("▶ 上传：{name}"));
    let file_name = client.upload_image(path)?;

    // 把上传得到的 fileName 注入到 LoadImage(642) 的 image 字段
    let nodes = serde_json::json!([
        { "nodeId": b.input_node, "fieldName": "image", "fieldValue": file_name }
    ]);

    // 提交（并发占满则等待重试）
    let mut task_id = String::new();
    for attempt in 0..20 {
        if stop.load(Ordering::SeqCst) {
            anyhow::bail!("已停止");
        }
        match client.create_task(&b.cfg.workflow_id, &nodes, true)? {
            CreateOutcome::Task(id) => {
                task_id = id;
                break;
            }
            CreateOutcome::Busy(m) => {
                log(format!("  队列占满（{m}），15s 后重试 {}/20", attempt + 1));
                std::thread::sleep(Duration::from_secs(15));
            }
        }
    }
    if task_id.is_empty() {
        anyhow::bail!("提交失败：队列长期占满，建议降低并发数");
    }
    log(format!("  taskId={task_id}，等待生成…"));

    // 轮询（放大较慢，给足超时）
    let deadline = std::time::Instant::now() + Duration::from_secs(1800);
    let outputs = loop {
        if stop.load(Ordering::SeqCst) {
            anyhow::bail!("已停止");
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("任务超时");
        }
        match client.poll_once(&task_id)? {
            Some(items) => break items,
            None => std::thread::sleep(Duration::from_secs(5)),
        }
    };

    // 只下载目标 SaveImage(758) 的输出；找不到则下载全部
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
    let mut saved = 0usize;
    for (i, o) in targets.iter().enumerate() {
        let ext = o
            .fileUrl
            .split('?')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .map(|e| format!(".{e}"))
            .unwrap_or_else(|| ".png".into());
        let suffix = if multi {
            format!("_rh_{}{}", i + 1, ext)
        } else {
            format!("_rh{}", ext)
        };
        let out = Path::new(&b.cfg.output_dir).join(format!("{name}{suffix}"));
        client.download(&o.fileUrl, &out)?;
        saved += 1;
    }
    Ok(saved)
}

// ============ main ============
fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([720.0, 680.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RunningHub 批量处理",
        options,
        Box::new(|cc| {
            setup_cjk_font(&cc.egui_ctx);
            Ok(Box::new(App::new(cc)))
        }),
    )
}

// 加载系统中文字体，避免中文显示为豆腐块
fn setup_cjk_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:/Windows/Fonts/msyh.ttc",   // 微软雅黑
        "C:/Windows/Fonts/simhei.ttf", // 黑体
        "C:/Windows/Fonts/simsun.ttc", // 宋体
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            // 版本敏感点①：egui 0.28+ 需要 Arc::new(...)；旧版去掉即可
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
```

---

## 6. 关键技术点说明

**① 立即模式 GUI 不能阻塞 UI 线程。** egui 每帧重绘界面，网络/IO 必须放到后台线程，否则界面会卡死。本方案：点“开始”后 `std::thread::spawn` 后台线程，UI 线程只负责画界面 + 每帧从 channel 取进度。

**② 后台→UI 用 channel + 主动重绘。** 用 `crossbeam-channel` 把 `Msg::Log / Progress / Finished` 发回 UI；后台每发一条就 `ctx.request_repaint()` 唤醒 UI 立即刷新；UI 在 `update()` 里 `try_recv()` 排空消息。

**③ 并发与限流。** 用一个任务队列（crossbeam）+ N 个 worker 线程的线程池；并发数 = 你 apiKey 的并发额度（基础套餐通常 1）。提交任务时若返回“队列/并发占满”，`create_task` 会返回 `Busy`，worker 自动等待 15s 重试，最多 20 次。`RhClient` 内含 `reqwest::Client`（内部 Arc，`clone` 很廉价），每个 worker 各持一份。

**④ 断点续跑。** 输出命名为 `原名_rh.png`；未勾选“覆盖”时，发现已存在 `原名_rh*` 就跳过，中断后重跑不浪费算力。

**⑤ TLS / 依赖。** 用 `rustls-tls` 纯 Rust TLS，Windows 上无需 OpenSSL，产物是单 exe。`reqwest::blocking` 内部自带运行时，注意**不要**在 tokio 异步上下文里调用阻塞客户端（本方案全程同步线程，无此问题）。

**⑥ 种子重置。** 与 Python 版一致：RunningHub 调用默认会重置随机种子，人像精修每次会有细微差异。若要可复现，在 `process_one` 的 `nodes` 数组里追加 `{ "nodeId": "采样器节点id", "fieldName": "seed", "fieldValue": 123456 }`。

---

## 7. 打包成 exe / 加图标 / 做安装包

**编译发布版：**

```bat
cargo build --release
```

产物：`target\release\rh_batch_gui.exe`（单文件，可直接拷给别人双击运行）。配合 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`，发布版不会弹出黑色控制台。

**加程序图标（可选）：** 准备 `assets/app.ico`，加 `build.rs`：

```rust
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        let _ = res.compile();
    }
}
```

`Cargo.toml` 增加：

```toml
[build-dependencies]
winresource = "0.1"
```

**减小体积（可选）：** `Cargo.toml` 里已设 `opt-level="z" + lto + strip`；还想更小可用 `upx --best target\release\rh_batch_gui.exe`。

**做安装包（可选）：**
- 简单：直接发 exe；
- MSI：`cargo install cargo-wix` → 工程里 `cargo wix`；
- 或用 Inno Setup / NSIS（图形化）把 exe 打成安装向导。

**分发注意：** eframe 0.34 默认 **wgpu** 渲染（DX12/Vulkan），现代 Windows 一般开箱即用；若个别机器花屏/打不开窗口，改用 glow 渲染——`Cargo.toml` 给 eframe 开 `features=["glow"]`，并在 `NativeOptions` 里设 `renderer: eframe::Renderer::Glow`。**不需要** WebView2（那是 Tauri 才需要的）。

---

## 8. 分阶段实施里程碑（Checklist）

- [ ] **M1 起步**：`cargo new` → 加依赖 → 跑通一个空窗口，加 `setup_cjk_font` 确认**中文不乱码**。
- [ ] **M2 界面**：搭好表单（5 个输入框 + 浏览按钮 + 并发滑条 + 覆盖勾选）+ 配置持久化（关掉再开还记得上次填的）。
- [ ] **M3 解析**：写 `workflow.rs`，点“开始”能在日志里看到 `识别到节点：输入=642 输出=758`。
- [ ] **M4 接口跑通**：写 `api.rs`，先用一个临时 `main`/单元测试，对**单张图**跑通“上传→提交→轮询→下载”（这一步建议先脱离 UI 单独验证，最容易定位问题）。
- [ ] **M5 接后台**：线程 + channel + 进度条 + 日志区联动。
- [ ] **M6 并发与健壮**：线程池并发、停止按钮、断点续跑、队列占满重试。
- [ ] **M7 打包**：隐藏控制台、加图标、`--release` 出单 exe。

---

## 9. 测试与常见问题排查

| 现象 | 原因 | 处理 |
|---|---|---|
| 中文显示成 □□□ | 没加载中文字体 | 检查 `setup_cjk_font`，确认 `C:/Windows/Fonts/msyh.ttc` 存在 |
| 窗口打不开 / 花屏 | wgpu 后端与显卡/驱动不合 | 改用 glow 渲染（见第 7 节） |
| 上传/提交报鉴权失败 | apiKey 填错 | 控制台“API 调用”页重新复制 |
| 一直 RUNNING 不结束 | 放大本身慢；或工作流报错 | 确认超时足够；看日志里 `promptTips` 的 `node_errors` 是否有报错节点 |
| 频繁“队列占满” | 并发数超过套餐额度 | 并发降到 1；或升级套餐 |
| 编译报 `FontData` 类型不匹配 | egui 版本差异 | 去掉/加上 `Arc::new(...)`（版本敏感点①） |
| `run_native` 闭包签名报错 | egui 版本差异 | 旧版用 `Box::new(\|cc\| Box::new(App::new(cc)))`（不带 `Ok`） |
| 公司网络连不上 | 443 被代理拦截 | 检查代理/防火墙，确认能访问 `www.runninghub.cn` |

> 建议把网络层（`api.rs`）**先单独测通**再接 UI——GUI 调试比命令行麻烦，先用一个最小 `fn main` 验证“单图全链路”，能极大缩短排错时间。

---

## 10. 备选 GUI 方案对比

- **Tauri**：用 HTML/CSS/JS 写界面，最终样式最漂亮；但要装 Node 前端工具链、依赖系统 WebView2、工程更复杂、产物更大。**追求精美 UI 时选它。**
- **Slint**：声明式 UI（`.slint` 文件），纯 Rust、界面也好看、可出单 exe；学习曲线比 egui 略高。
- **结论**：本工具是“表单 + 进度条 + 日志”的实用型小软件，**egui 最省事**——代码量小、单文件、双击即用，正是本方案采用它的原因。
