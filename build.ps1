# 构建脚本：本机用 GNU 工具链（rust-toolchain.toml 已固定）。
# GNU 目标编译 windows-sys 等 crate 需要 dlltool.exe，它随 rustup 的 GNU 工具链
# 一起分发在 self-contained 目录里，但默认不在 PATH 上，这里临时加进去。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File build.ps1            # 开发版
#   powershell -ExecutionPolicy Bypass -File build.ps1 -Release   # 发布版（单文件 exe）
param([switch]$Release)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$cargoBin = "$env:USERPROFILE\.cargo\bin"
$sc = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
# 便携 MinGW-w64（含 as/dlltool/gcc/ld）—— self-contained 的 dlltool 需要 as.exe 才能生成导入库。
$mingw = Join-Path $root "toolchain\mingw64\bin"
$env:Path = "$cargoBin;$mingw;$sc;" + $env:Path

if (-not (Test-Path "$mingw\as.exe")) {
    Write-Warning "未找到 MinGW 汇编器：$mingw\as.exe（请先解压 toolchain\winlibs.zip 到 toolchain\）"
}

if ($Release) {
    Write-Host "==> cargo build --release"
    cargo build --release
    $exe = "target\release\visual_coast_batch.exe"
    $named = "target\release\视觉海岸批量处理工作台.exe"
} else {
    Write-Host "==> cargo build"
    cargo build
    $exe = "target\debug\visual_coast_batch.exe"
    $named = "target\debug\视觉海岸批量处理工作台.exe"
}

if (Test-Path $exe) {
    Copy-Item $exe $named -Force
    Write-Host "构建完成：$named"
} else {
    Write-Error "构建失败：未生成 $exe"
}
