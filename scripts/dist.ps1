# cargo dist 一键打包：构建 release 并组装 dist\（exe + DdHotapply.dll）
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\dd-mrm.exe dist\ -Force
# DLL 已由 hotapply/build.rs 嵌进 exe（自包含分发）；清掉历史随行副本防误导。
Remove-Item dist\DdHotapply.dll -Force -ErrorAction SilentlyContinue

''
Get-ChildItem dist | Select-Object Name, @{n='KB';e={[math]::Round($_.Length/1KB)}}
