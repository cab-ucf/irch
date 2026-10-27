# Self-updating irch: irm $R/install.ps1 | iex
# -Server: an always-on storage node instead (a task, at login).
# Fetched by PowerShell, it is not marked as from the web: no SmartScreen.
param([switch]$Server)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$R = 'https://github.com/cab-ucf/irch/releases/latest/download'
$cpu = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }

if ($Server) {
  $dir = "$env:LOCALAPPDATA\irch-server"
  New-Item -ItemType Directory -Force $dir | Out-Null
  Invoke-WebRequest "$R/irch-$cpu-pc-windows-msvc.exe" -OutFile "$dir\irch.exe"
  & "$dir\irch.exe" --install
  exit $LASTEXITCODE
}

# The installer, for you alone: no administrator, no questions.
$url = (Invoke-RestMethod "$R/latest.json").platforms."windows-$cpu-nsis".url
$setup = "$env:TEMP\irch-setup.exe"
Invoke-WebRequest $url -OutFile $setup
Start-Process $setup -ArgumentList '/S' -Wait
Remove-Item $setup
$exe = Get-ChildItem "$env:LOCALAPPDATA\irch" -Filter irch*.exe -Recurse |
  Where-Object Name -NotMatch 'uninstall' | Select-Object -First 1
if ($exe) { Start-Process $exe.FullName }
Write-Host 'irch is in your Start menu, and updates itself.'
