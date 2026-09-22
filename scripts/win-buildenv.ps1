# 准备 Windows 上的构建环境，然后在这个环境里跑命令。
#
# **多数情况下你不需要这个脚本。** 直接 `cargo build` 就行。
#
# 它只剩一件事：给 libopus 找 cmake。`audiopus_sys` 从源码编 libopus，
# 而 cmake 不一定在 PATH 上 —— Visual Studio 自带一份，但装在一个
# 没人会加进 PATH 的地方。找不到 cmake 的话 cargo 会在 audiopus_sys
# 那一步失败，报错还不会告诉你缺的是 cmake。
#
# 用法：
#   .\scripts\win-buildenv.ps1                      # 只检查环境，报告缺什么
#   .\scripts\win-buildenv.ps1 -Run "cargo test --workspace"
#
# ---------------------------------------------------------------------------
# 2026-09 之前这个脚本大得多：APM 用的是 C++ 的 libwebrtc，要 meson、ninja、
# 伪造的 nm.exe、libclang，还要把 CARGO_TARGET_DIR 挪到 C:\t 去绕 MAX_PATH。
# 换成纯 Rust 的 sonora 之后那些全没了。经过见 docs/apm-backend.md。
# ---------------------------------------------------------------------------

[CmdletBinding()]
param(
    [string]$Run = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$missing = @()

# ---- cmake（libopus 要用）----
# cmake crate 认 CMAKE 这个环境变量，所以找到了直接设上就行，不用改 PATH。
if (-not (Get-Command cmake -ErrorAction SilentlyContinue) -and -not $env:CMAKE) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $found = $null
    if (Test-Path $vswhere) {
        $found = & $vswhere -latest -products * -property installationPath |
            ForEach-Object { Join-Path $_ "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe" } |
            Where-Object { Test-Path $_ } |
            Select-Object -First 1
    }
    if ($found) {
        $env:CMAKE = $found
        Write-Host "cmake: $found"
    } else {
        $missing += "cmake  ->  装一个（https://cmake.org），或者设 `$env:CMAKE 指到已有的"
    }
} else {
    Write-Host "cmake: 已经在 PATH 上（或者 `$env:CMAKE 已设）"
}

# VS 自带的 cmake 是 4.x，而 libopus 的 CMakeLists 声明的最低版本太老，
# 4.x 会直接拒绝。这个变量让它放行。
if (-not $env:CMAKE_POLICY_VERSION_MINIMUM) { $env:CMAKE_POLICY_VERSION_MINIMUM = "3.5" }

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
