# 视觉海岸批量处理工作台

RunningHub 工作流批量自动化的 Windows 桌面程序（Rust + egui，单文件 exe）。
把本地“输入文件夹”里的图片批量上传到 RunningHub 跑工作流（Flux2-Klein 人像精修 / 高清放大），
再把结果图自动下载到“输出文件夹”。功能对齐命令行脚本 `doc/rh_batch.py`。

## 功能

- 现代浅色界面（高级浅色）：浅灰画布 + 白色卡片 + 海岸青绿（teal）强调色，圆角卡片、细描边、柔和阴影、彩色状态徽章；面板式布局（顶部控制区 / 左侧图片列表 / 中间“处理前/处理后”对比 / 底部日志）；中文字体、统一字号间距
- **分图进度**：每张图实时显示阶段（上传中 / 提交中 / 生成中 / 下载中 / 已完成 / 已跳过 / 失败）+ 彩色状态图标；总进度条带百分比
- **处理前/后对比预览**：选中任意图片即并排显示原图与结果图（缩略图后台解码，不卡界面）；“跟随处理中的图片”自动切到当前正在处理的那张；可一键用系统默认程序打开结果。断点续跑被跳过的图也会回显已存在的结果，便于直接对比
- 表单填参 + 原生文件夹/文件选择，参数自动持久化（下次打开记得上次填写）
- 工作流 JSON 自动识别 `LoadImage`（输入）/ `SaveImage`（输出）节点；多节点时给出告警，可手动指定
- 上传 → 提交 → 轮询 → 下载 全链路 + 实时日志
- 多线程并发；提交遇“队列/并发占满”自动等待重试；网络失败自动重试
- 断点续跑（默认开启）：输出命名 `原名_rh.png`，勾选“跳过已处理的图片（断点续跑）”时，输出目录里已存在 `原名_rh*` 结果的图片会自动跳过；取消勾选则全部重新处理
- 轮询对“不确定”响应容忍若干次再判失败（对齐 Python `unknown_strikes`）
- 解析提交返回的 `promptTips.node_errors` 并在日志告警
- 开跑前显示账户状态（`currentTaskCounts` / `remainCoins`）
- 处理完写出 `_manifest.csv`（UTF-8 BOM，Excel 友好）+ 成功/跳过/失败汇总
- 高级设置：手动节点 ID、注入字段名、额外节点参数（如固定 seed）、轮询间隔、任务超时、各类重试次数、`addMetadata`

## 构建运行

本机使用 **GNU 工具链**（`rust-toolchain.toml` 已固定）。编译 `windows-sys` 需要 `dlltool`+`as`，
随附在工程内的便携 MinGW（`toolchain\mingw64\bin`）里，`build.ps1` 会自动把它和工具链 self-contained
目录加进 PATH，无需手动配置：

```powershell
# 开发版
powershell -ExecutionPolicy Bypass -File build.ps1

# 发布版（隐藏控制台、单文件 exe）
powershell -ExecutionPolicy Bypass -File build.ps1 -Release
```

产物：`target\release\视觉海岸批量处理工作台.exe`，**仅依赖 Windows 系统 DLL**（TLS 走系统 Schannel，
无 OpenSSL / WebView2 / MinGW 运行时依赖），可直接拷给别人双击运行。

> 也可手动 `cargo build --release`，但需先把 `toolchain\mingw64\bin` 与工具链的
> `...\x86_64-pc-windows-gnu\bin\self-contained` 加入 PATH（即 `build.ps1` 所做）。

可选：放置 `assets/app.ico` 后重新构建即可给 exe 加图标。

## 使用

1. 填写 API Key、工作流 ID、输入/输出文件夹；选择工作流 JSON（用于自动识别节点）。
2. 按并发额度设置“并发数”（基础套餐填 1）。
3. 点“▶ 开始批量处理”，看进度条与日志；中途可“■ 停止”。

配置保存在 exe 同目录的 `vc_batch_config.json`。
