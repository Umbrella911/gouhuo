# 跑完整的 M1 报告并存档。
#
# 用法：
#   .\scripts\m1.ps1                 # 默认每配置 30 秒，约 13 分钟
#   .\scripts\m1.ps1 -Seconds 6      # 快速过一遍，约 3 分钟
#
# 出两个文件到 artifacts\：给人看的文本报告，和给 CI / 做趋势对比的 JSON。

[CmdletBinding()]
param(
    [int]$Seconds = 30,
    [string]$OutDir = "artifacts"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# audiopus_sys 要用 cmake 从源码编译 libopus。VS BuildTools 自带一份，
# 但通常不在 PATH 上。
if (-not (Get-Command cmake -ErrorAction SilentlyContinue) -and -not $env:CMAKE) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        foreach ($vs in & $vswhere -products * -property installationPath) {
            $candidate = Join-Path $vs "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
            if (Test-Path $candidate) {
                $env:CMAKE = $candidate
                Write-Host "用 VS 自带的 cmake: $candidate"
                break
            }
        }
    }
    if (-not $env:CMAKE) {
        throw "找不到 cmake。装一个，或者设 `$env:CMAKE 指向 cmake.exe。"
    }
}
# VS 自带的 cmake 是 4.x，libopus 的 CMakeLists 声明的最低版本太老会被直接拒。
if (-not $env:CMAKE_POLICY_VERSION_MINIMUM) {
    $env:CMAKE_POLICY_VERSION_MINIMUM = "3.5"
}

Write-Host "== 构建 =="
cargo build --release -p latency-probe
if ($LASTEXITCODE -ne 0) { throw "构建失败" }

Write-Host "== 测试 =="
cargo test --workspace
if ($LASTEXITCODE -ne 0) { throw "测试失败" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$txt = Join-Path $OutDir "m1-$stamp.txt"
$json = Join-Path $OutDir "m1-$stamp.json"

# 别在跑测量的时候干别的：这测的是实时调度，后台一个编译就能把 p99 毁掉。
Write-Host "== 测量（每配置 $Seconds 秒）=="
Write-Host "   跑的时候别动这台机器 —— 后台一个编译就能把 p99 毁掉。"

cargo run --release -q -p latency-probe -- --seconds $Seconds |
    Tee-Object -FilePath $txt
if ($LASTEXITCODE -ne 0) { throw "测量失败" }

cargo run --release -q -p latency-probe -- --seconds $Seconds --json |
    Out-File -FilePath $json -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "JSON 输出失败" }

Write-Host ""
Write-Host "报告: $txt"
Write-Host "JSON: $json"
