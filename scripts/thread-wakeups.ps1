# 一个进程每个线程每秒被切上 CPU 几次（上下文切换），按线程名列出来。
#
# 用法：.\scripts\thread-wakeups.ps1 -ProcessId 1234 [-Seconds 10]
#
# 线程名是 Rust 那边 thread::Builder::name 设的（Windows 上走 SetThreadDescription），
# 没名字的是系统或者库自己起的线程（音频驱动、窗口系统之类）。
# 计数用的是 WMI 的原始线程计数器，不随系统语言变。

[CmdletBinding()]
param(
    [Parameter(Mandatory)] [int]$ProcessId,
    [int]$Seconds = 10
)
Add-Type @"
using System; using System.Runtime.InteropServices;
public class TW {
 [DllImport("kernel32.dll")] public static extern IntPtr OpenThread(uint access, bool inherit, uint id);
 [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
 [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] public static extern int GetThreadDescription(IntPtr h, out IntPtr desc);
 [DllImport("kernel32.dll")] public static extern IntPtr LocalFree(IntPtr p);
 public static string Name(uint id) {
   IntPtr h = OpenThread(0x0800 /* THREAD_QUERY_LIMITED_INFORMATION */, false, id);
   if (h == IntPtr.Zero) return "?";
   try { IntPtr p; if (GetThreadDescription(h, out p) < 0) return "?"; string s = Marshal.PtrToStringUni(p); LocalFree(p); return s; }
   finally { CloseHandle(h); }
 }
}
"@

function Snapshot {
    $m = @{}
    Get-CimInstance Win32_PerfRawData_PerfProc_Thread -Filter "IDProcess=$ProcessId" |
        ForEach-Object { $m[[uint32]$_.IDThread] = [int64]$_.ContextSwitchesPersec }
    return $m
}

$a = Snapshot
$t0 = Get-Date
Start-Sleep -Seconds $Seconds
$b = Snapshot
$wall = ((Get-Date) - $t0).TotalSeconds

$rows = foreach ($id in $b.Keys) {
    $before = if ($a.ContainsKey($id)) { $a[$id] } else { 0 }
    $name = [TW]::Name($id)
    if ([string]::IsNullOrEmpty($name)) { $name = "（没名字）" }
    [pscustomobject]@{ 线程 = $id; 名字 = $name; 唤醒每秒 = [math]::Round(($b[$id] - $before) / $wall, 1) }
}
$rows | Sort-Object 唤醒每秒 -Descending | Format-Table -AutoSize
"合计 {0:N0} 次/秒" -f ($rows | Measure-Object 唤醒每秒 -Sum).Sum
