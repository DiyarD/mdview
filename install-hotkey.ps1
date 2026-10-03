# Installs mdview's global hotkey via the tiny tray helper.
# (The old Start-Menu .lnk Hotkey trick proved unreliable, so this just
# delegates to mdview.exe --install-hotkey, which sets up autostart +
# a real RegisterHotKey hotkey.)
param([string]$Exe = (Join-Path $PSScriptRoot "target\release\mdview.exe"))
& (Resolve-Path $Exe).Path --install-hotkey
