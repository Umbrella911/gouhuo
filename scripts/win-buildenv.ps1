# 准备 Windows 上的构建环境，然后在这个环境里跑命令。
#
# 为什么需要这个脚本：APM（webrtc-audio-processing）要从源码编 libwebrtc，
# 而那套构建是 Linux 习惯的 —— meson + ninja + nm，还要 MSVC 环境和 libclang。
# third_party/ 下的补丁解决了构建脚本里的 Windows 缺口，但外部工具还得自己备。
#
# 用法：
#   .\scripts\win-buildenv.ps1                      # 只检查环境，报告缺什么
#   .\scripts\win-buildenv.ps1 -Run "cargo test --workspace"
#   .\scripts\win-buildenv.ps1 -Run "cargo run --release -p device-probe"
#
# 一次性的准备（脚本会告诉你缺哪个）：
#   pip install meson ninja
#   rustup component add llvm-tools
#
# 背景和踩过的坑见 docs/m2-apm-windows.md。

[CmdletBinding()]
param(
    [string]$Run = "",
    # MAX_PATH。abseil 的相对路径 + cargo 的 out 目录能顶到 265 字符
    # （Windows 拿"当前目录 + 相对路径"在规范化**之前**比 260 上限）。
    # 构建根不够短的话，cl 会报"源文件不存在"—— 而文件其实在。
    [string]$TargetDir = "C:\t"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$missing = @()

# ---- MSVC ----
# meson 要在 PATH 上看得见 cl.exe 才会选 MSVC 后端。
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "找不到 vswhere，Visual Studio 没装？" }
$vs = & $vswhere -latest -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath |
    Select-Object -First 1
if (-not $vs) { throw "找不到带 C++ 工具链的 Visual Studio" }

$vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
Write-Host "MSVC: $vs"
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^(INCLUDE|LIB|LIBPATH|PATH)=(.*)$') {
        Set-Item -Path "env:$($matches[1])" -Value $matches[2]
    }
}
if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) { throw "注入 MSVC 环境之后还是找不到 cl.exe" }

# ---- meson / ninja ----
foreach ($tool in @("meson", "ninja")) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        $missing += "$tool  ->  pip install meson ninja"
    }
}

# ---- nm ----
# 上游的构建脚本本来是裸调 PATH 上的 nm；third_party/ 的补丁改成了去 rustc
# sysroot 里找 llvm-nm，所以这里只要确认那个组件装了。
$sysrootBin = Join-Path (rustc --print sysroot) "lib\rustlib\x86_64-pc-windows-msvc\bin"
if (-not (Test-Path (Join-Path $sysrootBin "llvm-nm.exe"))) {
    $missing += "llvm-nm  ->  rustup component add llvm-tools"
}

# ---- libclang（bindgen 要用）----
if (-not $env:LIBCLANG_PATH) {
    $candidates = @(
        (Join-Path $vs "VC\Tools\Llvm\x64\bin"),
        "C:\Program Files\LLVM\bin"
    )
    # vswhere 只给了一个 VS；别的 VS 实例里也可能有 libclang，一并找。
    $candidates += (& $vswhere -products * -property installationPath |
        ForEach-Object { Join-Path $_ "VC\Tools\Llvm\x64\bin" })
    $found = $candidates | Where-Object { Test-Path (Join-Path $_ "libclang.dll") } | Select-Object -First 1
    if ($found) {
        $env:LIBCLANG_PATH = $found
        Write-Host "libclang: $found"
    } else {
        $missing += "libclang.dll  ->  装 VS 的 'C++ Clang tools for Windows' 组件，或者单独装 LLVM"
    }
} else {
    Write-Host "libclang: $env:LIBCLANG_PATH（沿用已有的 LIBCLANG_PATH）"
}

# ---- cmake（libopus 要用）----
# VS 自带一份，通常不在 PATH 上。cmake crate 认 CMAKE 这个环境变量。
if (-not (Get-Command cmake -ErrorAction SilentlyContinue) -and -not $env:CMAKE) {
    $c = Join-Path $vs "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
    if (Test-Path $c) {
        $env:CMAKE = $c
        Write-Host "cmake: $c"
    } else {
        $missing += "cmake  ->  装一个，或者设 `$env:CMAKE"
    }
}
# VS 自带的 cmake 是 4.x，libopus 的 CMakeLists 声明的最低版本太老会被直接拒。
if (-not $env:CMAKE_POLICY_VERSION_MINIMUM) { $env:CMAKE_POLICY_VERSION_MINIMUM = "3.5" }

# ---- 构建根要短 ----
$env:CARGO_TARGET_DIR = $TargetDir
Write-Host "CARGO_TARGET_DIR: $TargetDir （MAX_PATH，见脚本顶部注释）"

if ($missing.Count -gt 0) {
    Write-Host ""
    Write-Host "还缺这些：" -ForegroundColor Yellow
    $missing | ForEach-Object { Write-Host "  $_" }
    throw "环境没准备齐"
}

Write-Host "环境就绪。"

if ($Run) {
    Write-Host ""
    Write-Host "== $Run =="
    Invoke-Expression $Run
    exit $LASTEXITCODE
}
