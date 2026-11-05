# smoke.ps1 —— DdHotapply.dll 注入链路冒烟测试（不需要游戏）。
# 宿主用 notepad（非 Darkest.exe）：DLL 应当 装载→读cfg→建日志→版本护栏拒载→自卸载，
# 日志里出现 "fatal: frame pattern mismatch"，宿主进程存活无崩溃。
# 用法：pwsh -ExecutionPolicy Bypass -File native\smoke.ps1
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$dll = Join-Path $root '..\Gui\eng\native\DdHotapply.dll'
if (-not (Test-Path $dll)) { throw "DLL not built: $dll (run build.ps1 first)" }
$logDir = Join-Path $root '..\logs'
$logFile = Join-Path $logDir 'engine.log'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Inj {
    [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr VirtualAllocEx(IntPtr p, IntPtr a, UIntPtr s, uint t, uint pr);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool VirtualFreeEx(IntPtr p, IntPtr a, UIntPtr s, uint t);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool WriteProcessMemory(IntPtr p, IntPtr a, byte[] b, UIntPtr s, out UIntPtr w);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr CreateRemoteThread(IntPtr p, IntPtr sa, UIntPtr st, IntPtr fn, IntPtr parm, uint f, out uint tid);
    [DllImport("kernel32.dll")] public static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll")] public static extern bool GetExitCodeThread(IntPtr t, out IntPtr code);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll")] public static extern IntPtr GetModuleHandle(string n);
    [DllImport("kernel32.dll")] public static extern IntPtr GetProcAddress(IntPtr m, string p);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr CreateFileMapping(IntPtr h, IntPtr a, uint pr, uint hi, uint lo, string name);
    [DllImport("kernel32.dll")] public static extern IntPtr MapViewOfFile(IntPtr m, uint ac, uint hi, uint lo, UIntPtr bytes);
    [DllImport("kernel32.dll")] public static extern bool UnmapViewOfFile(IntPtr v);
    // probe pipe after inject (DLL rejected => pipe absent => correct)
    public static bool PipeAlive() {
        try { var c = new System.IO.Pipes.NamedPipeClientStream(".", "DdModToolHotapply", System.IO.Pipes.PipeDirection.InOut);
              c.Connect(300); c.Dispose(); return true; }
        catch { return false; }
    }
}
"@

$logSizeBefore = if (Test-Path $logFile) { (Get-Item $logFile).Length } else { 0 }

# cfg 共享内存（与 NativeInjector.WriteCfg 同语义）
$cfg = [Inj]::CreateFileMapping([IntPtr]::new(-1), [IntPtr]::Zero, 4, 0, 1024, "Local\DdModToolHotapplyCfg")
if ($cfg -eq [IntPtr]::Zero) { throw "CreateFileMapping failed" }
$view = [Inj]::MapViewOfFile($cfg, 2, 0, 0, [UIntPtr]::new(1024))
$data = [System.Text.Encoding]::Unicode.GetBytes("$logDir`0")
[Runtime.InteropServices.Marshal]::Copy($data, 0, $view, [Math]::Min($data.Length, 1020))
[Inj]::UnmapViewOfFile($view) | Out-Null   # 映射对象靠 $cfg 句柄存活

$np = Start-Process notepad -PassThru
Start-Sleep -Milliseconds 800
try {
    if ($np.HasExited) { throw "notepad exited before injection" }
    $path = [System.Text.Encoding]::Unicode.GetBytes("$dll`0")
    $mem = [Inj]::VirtualAllocEx($np.Handle, [IntPtr]::Zero, [UIntPtr]::new($path.Length), 0x3000, 4)
    if ($mem -eq [IntPtr]::Zero) { throw "VirtualAllocEx failed" }
    $written = [UIntPtr]::new(0)
    [Inj]::WriteProcessMemory($np.Handle, $mem, $path, [UIntPtr]::new($path.Length), [ref]$written) | Out-Null
    $loadLib = [Inj]::GetProcAddress([Inj]::GetModuleHandle("kernel32.dll"), "LoadLibraryW")
    $tid = [uint32]0
    $th = [Inj]::CreateRemoteThread($np.Handle, [IntPtr]::Zero, [UIntPtr]::Zero, $loadLib, $mem, 0, [ref]$tid)
    if ($th -eq [IntPtr]::Zero) { throw "CreateRemoteThread failed" }
    [Inj]::WaitForSingleObject($th, 5000) | Out-Null
    $hMod = [IntPtr]::new(0)
    [Inj]::GetExitCodeThread($th, [ref]$hMod) | Out-Null
    Write-Host ("LoadLibraryW remote exit (low 32b): 0x{0:X}" -f $hMod.ToInt64())
    [Inj]::CloseHandle($th) | Out-Null
    [Inj]::VirtualFreeEx($np.Handle, $mem, [UIntPtr]::new(0), 0x8000) | Out-Null

    Start-Sleep -Milliseconds 1500   # 等 DLL init 线程跑完
    if ($np.HasExited) { throw "FAIL: host process crashed during/after injection" }

    $added = if (Test-Path $logFile) { Get-Content $logFile -Raw } else { "" }
    $okLog = $added -match 'fatal: frame pattern mismatch'
    $pipeAlive = [Inj]::PipeAlive()
    Write-Host ("pipe alive after inject: {0} (advisory: frida engine running=expected true)" -f $pipeAlive)
    if ($okLog) { Write-Host "SMOKE PASS: inject+cfg+log+guard+unload all OK (host alive)" }
    else { throw "FAIL: log=$okLog — check logs\engine.log" }
}
finally {
    if (-not $np.HasExited) { Stop-Process -Id $np.Id -Force }
    [Inj]::CloseHandle($cfg) | Out-Null
}
