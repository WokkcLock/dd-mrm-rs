# build.ps1 —— 构建 DdHotapply.dll（原生热应用引擎）。
# 依赖：VS2022 C++ 生成工具（vcvars64 + ml64）。产物输出到 crates\hotapply\bin\DdHotapply.dll。
# 用法：pwsh -File native\build.ps1
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$out = Join-Path $root '..\crates\hotapply\bin'
New-Item -ItemType Directory -Force -Path $out | Out-Null

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found (need VS installer)" }
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vsPath) { throw "VS2022 C++ tools not found (install 'Desktop development with C++')" }
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found under $vsPath" }

$cmd = "`"$vcvars`" >nul && cd /d `"$root`" && ml64 /nologo /c /Fo thunk64.obj thunk64.asm && cl /nologo /utf-8 /O2 /MT /LD DdHotapply.c thunk64.obj /Fe:DdHotapply.dll /link /nologo kernel32.lib advapi32.lib"
cmd /c $cmd
if ($LASTEXITCODE -ne 0) { throw "build failed: exit $LASTEXITCODE" }

Copy-Item (Join-Path $root 'DdHotapply.dll') (Join-Path $out 'DdHotapply.dll') -Force
Remove-Item "$root\thunk64.obj", "$root\DdHotapply.obj", "$root\DdHotapply.lib", "$root\DdHotapply.exp", "$root\DdHotapply.dll" -ErrorAction SilentlyContinue
Get-Item (Join-Path $out 'DdHotapply.dll') | Select-Object FullName, Length, LastWriteTime
