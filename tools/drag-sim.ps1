param(
  [Parameter(Mandatory = $true)][string[]]$Files,
  [string]$ProcessName = "mdview"
)
# Real OLE drag-and-drop simulation: presses the mouse over the target
# window, runs WinForms DoDragDrop with CF_HDROP payloads (exactly what
# Explorer provides), jiggles, releases -> the drop lands for real.
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -ReferencedAssemblies System, System.Windows.Forms -TypeDefinition @"
using System;
using System.Threading;
using System.Windows.Forms;

public static class DragSim {
  [System.Runtime.InteropServices.DllImport("user32.dll")]
  public static extern bool SetCursorPos(int x, int y);
  [System.Runtime.InteropServices.DllImport("user32.dll")]
  public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, UIntPtr dwExtraInfo);

  public static void Drag(string[] files, int x, int y, int sx, int sy) {
    Exception err = null;
    SetCursorPos(sx, sy);
    Thread.Sleep(250);
    var th = new Thread(() => {
      try {
        mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero); // LEFTDOWN at source point
        Thread.Sleep(80);
        using (var form = new Form()) {
          var data = new DataObject();
          data.SetData(DataFormats.FileDrop, files);
          form.DoDragDrop(data, DragDropEffects.Copy);
        }
      } catch (Exception e) { err = e; }
      mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero); // safety LEFTUP
    });
    th.SetApartmentState(ApartmentState.STA);
    th.Start();
    Thread.Sleep(500);
    SetCursorPos((sx + x) / 2, (sy + y) / 2); // enter the target window
    Thread.Sleep(300);
    SetCursorPos(x + 6, y + 4);   // DragOver jiggle
    Thread.Sleep(250);
    SetCursorPos(x - 4, y - 3);
    Thread.Sleep(350);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero); // LEFTUP -> Drop
    if (!th.Join(8000)) throw new Exception("drag thread hung");
    if (err != null) throw err;
  }
}
"@
Add-Type @"
using System; using System.Runtime.InteropServices;
public class WinF {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
$p = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
  Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { Write-Output "NO_WINDOW"; exit 1 }
[void][WinF]::ShowWindow($p.MainWindowHandle, 9)
[void][WinF]::SetForegroundWindow($p.MainWindowHandle)
Start-Sleep -Milliseconds 350
$r = New-Object WinF+RECT
if (-not [WinF]::GetWindowRect($p.MainWindowHandle, [ref]$r)) { Write-Output "NO_RECT"; exit 1 }
$cx = [int](($r.Left + $r.Right) / 2)
$cy = [int](($r.Top + $r.Bottom) / 2)
# Real drags start with the button down OUTSIDE the target (source app),
# then move in — pressing down ON the drop target confuses the OLE cycle.
$sx = $r.Left - 60
if ($sx -lt 5) { $sx = [Math]::Min($r.Right + 60, 1200) }
$sy = [Math]::Max($r.Top - 40, 40)
[DragSim]::Drag($Files, $cx, $cy, $sx, $sy)
Write-Output "DRAG_DONE from $sx,$sy to $cx,$cy"
