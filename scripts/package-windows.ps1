# 打包 Windows 便携版：解压后直接运行，配置/数据都在 exe 所在目录。
#
# 用法（在仓库根目录执行）：
#   pwsh -File scripts/package-windows.ps1
#   pwsh -File scripts/package-windows.ps1 -SkipBuild
#
# 产物：
#   dist/SubForge.rs-<version>-windows-x86_64.zip
#   dist/SHA256SUMS.txt

[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if (-not $Version) {
    $cargo = Get-Content (Join-Path $root "Cargo.toml") -Raw
    $m = [regex]::Match($cargo, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $m.Success) { throw "无法从 Cargo.toml 读取 version" }
    $Version = $m.Groups[1].Value
}

$name = "SubForge.rs-$Version-windows-x86_64"
$dist = Join-Path $root "dist"
$stage = Join-Path $dist $name
$zip = Join-Path $dist "$name.zip"

if (-not $SkipBuild) {
    Push-Location $root
    try {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }
    } finally {
        Pop-Location
    }
}

if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Path $stage -Force | Out-Null

$exe = Join-Path $root "target/release/subforge-rs.exe"
if (-not (Test-Path $exe)) { throw "找不到 $exe，请先构建 release" }
Copy-Item $exe (Join-Path $stage "subforge-rs.exe")
Copy-Item (Join-Path $root "static") (Join-Path $stage "static") -Recurse
Copy-Item (Join-Path $root "config.example.toml") $stage
Copy-Item (Join-Path $root "README.md") $stage
if (Test-Path (Join-Path $root "LICENSE")) {
    Copy-Item (Join-Path $root "LICENSE") $stage
}

# 确保不把密码/数据带进发布包
foreach ($forbidden in @("config.toml", "data", "data-smoke")) {
    $p = Join-Path $stage $forbidden
    if (Test-Path $p) { Remove-Item -Recurse -Force $p }
}

if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip

$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
$sums = Join-Path $dist "SHA256SUMS.txt"
"$hash  $name.zip" | Set-Content -Encoding ASCII $sums

Write-Host "已生成："
Write-Host "  $zip"
Write-Host "  $sums"
