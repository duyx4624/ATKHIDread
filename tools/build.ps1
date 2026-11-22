#Requires -Version 5.1
<#
  一键构建 + 打包
  用法：pwsh tools/build.ps1     或双击根目录 build.cmd
  产物：dist\hidread-<版本>\ 与 dist\hidread-<版本>-win64.zip
#>
$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'
Set-Location (Join-Path $PSScriptRoot '..')

Write-Host '==> cargo build --release' -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw '构建失败，已中止' }

$ver = (Select-String -Path 'Cargo.toml' -Pattern '^version = "([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
if (-not $ver) { throw '读不到 Cargo.toml 里的版本号' }

$name  = "hidread-$ver"
$stage = Join-Path 'dist' $name
$zip   = Join-Path 'dist' "$name-win64.zip"

Write-Host "==> 准备发布目录 $stage" -ForegroundColor Cyan
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force -Confirm:$false }
New-Item -ItemType Directory -Path $stage -Force | Out-Null

Copy-Item 'target/release/hidread.exe' $stage -Force
Copy-Item 'config.example.json'        $stage -Force
if (Test-Path 'plugins') { Copy-Item 'plugins' $stage -Recurse -Force }

# 说明文件按当前版本号生成，避免手工维护两份
if (Test-Path '使用说明.txt') {
    $readme = Get-Content '使用说明.txt' -Raw -Encoding UTF8
    $readme = [regex]::Replace($readme, 'hidread \d+\.\d+\.\d+', "hidread $ver")
    Set-Content (Join-Path $stage '使用说明.txt') $readme -Encoding UTF8 -NoNewline
}

$src = (Get-Item 'target/release/hidread.exe').Length
$dst = (Get-Item (Join-Path $stage 'hidread.exe')).Length
if ($src -ne $dst) { throw "exe 复制不完整（$src → $dst 字节）" }

Write-Host '==> 压缩' -ForegroundColor Cyan
if (Test-Path $zip) { Remove-Item $zip -Force -Confirm:$false }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal

$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
$size = [math]::Round((Get-Item $zip).Length / 1KB, 1)

# 清理旧包，只留当前版本
Get-ChildItem 'dist' -Directory -Filter 'hidread-*' -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -ne $name } |
    Remove-Item -Recurse -Force -Confirm:$false
Get-ChildItem 'dist' -File -Filter 'hidread-*-win64.zip' -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -ne "$name-win64.zip" } |
    Remove-Item -Force -Confirm:$false

Write-Host ''
Write-Host "构建完成 $ver" -ForegroundColor Green
Write-Host "包     dist\$name-win64.zip  ($size KB)"
Write-Host "SHA256 $hash"
Write-Host "exe    $src 字节"
Write-Host ''
Write-Host '覆盖安装：先从托盘退出，再把 exe 复制到安装目录（保留 data 目录）' -ForegroundColor DarkGray
