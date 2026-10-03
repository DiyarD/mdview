param(
  [Parameter(Mandatory = $true)][string]$Path,
  [string]$ProcessName = "mdview"
)
# Drive the native open-file dialog (same process as the app) via UIA:
# wait for it, type the path into the filename box, press Open.
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$p = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
  Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { Write-Output "NO_WINDOW"; exit 1 }
$root = [System.Windows.Automation.AutomationElement]::RootElement
$pc = New-Object System.Windows.Automation.PropertyCondition(
  [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $p.Id)
$dlg = $null
$deadline = (Get-Date).AddSeconds(10)
while ((Get-Date) -lt $deadline -and -not $dlg) {
  $dlg = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $pc)
  if (-not $dlg) { Start-Sleep -Milliseconds 250 }
}
if (-not $dlg) { Write-Output "DIALOG_NOT_FOUND"; exit 1 }
# Vista-style dialog: filename edit = AutomationId 1148; fall back to first edit.
$edit = $dlg.FindFirst([System.Windows.Automation.TreeScope]::Descendants,
  (New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::AutomationIdProperty, "1148")))
if (-not $edit) {
  $ec = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
    [System.Windows.Automation.ControlType]::Edit)
  $edit = $dlg.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $ec)
}
if (-not $edit) { Write-Output "EDIT_NOT_FOUND"; exit 1 }
$vp = $edit.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
$vp.SetValue($Path)
Start-Sleep -Milliseconds 200
# Open = IDOK (AutomationId "1"); fall back to name match.
$btn = $dlg.FindFirst([System.Windows.Automation.TreeScope]::Descendants,
  (New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::AutomationIdProperty, "1")))
if (-not $btn) {
  $bc = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, "Open")
  $btn = $dlg.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $bc)
}
if (-not $btn) { Write-Output "OPEN_BTN_NOT_FOUND"; exit 1 }
$bp = $btn.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
$bp.Invoke()
Write-Output "PICK_SENT"
