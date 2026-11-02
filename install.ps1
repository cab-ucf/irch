# Self-updating irch: irm $R/install.ps1 | iex
# -Server: an always-on storage node instead (a task, at login).
# Fetched by PowerShell, it is not marked as from the web: no SmartScreen.
# With minisign here, checked against irch's key as updates are; without,
# trusted as GitHub serves it, and said aloud.
param([switch]$Server)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$R = 'https://github.com/cab-ucf/irch/releases/latest/download'
$cpu = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$KEY = 'RWSPfztwzR/MhakRL6cA70pN03n/nMCdTvoY5UHC2IcZuhcR+41RQI95'
$minisign = Get-Command minisign -ErrorAction SilentlyContinue
if (-not $minisign) {
  Write-Warning "No minisign: nothing here is checked but GitHub's TLS."
}
function Get($url, $to) {
  Invoke-WebRequest $url -OutFile $to
  if (-not $minisign) { return }
  Invoke-WebRequest "$url.sig" -OutFile "$to.sig"
  $sig = [Convert]::FromBase64String([IO.File]::ReadAllText("$to.sig"))
  [IO.File]::WriteAllBytes("$to.minisig", $sig)
  $said = & $minisign -Vm $to -x "$to.minisig" -P $KEY
  Remove-Item "$to.sig", "$to.minisig"
  if ($LASTEXITCODE -or -not ($said -like "*file:$(Split-Path $url -Leaf)*")) {
    Remove-Item $to
    throw "${url}: not signed by irch"
  }
}

if ($Server) {
  $dir = "$env:LOCALAPPDATA\irch-server"
  New-Item -ItemType Directory -Force $dir | Out-Null
  Get "$R/irch-$cpu-pc-windows-msvc.exe" "$dir\irch.exe"
  & "$dir\irch.exe" --install
  exit $LASTEXITCODE
}

# The installer, for you alone: no administrator, no questions.
$url = (Invoke-RestMethod "$R/latest.json").platforms."windows-$cpu-nsis".url
$setup = "$env:TEMP\irch-setup.exe"
Get $url $setup
Start-Process $setup -ArgumentList '/S' -Wait
Remove-Item $setup
$exe = Get-ChildItem "$env:LOCALAPPDATA\irch" -Filter irch*.exe -Recurse |
  Where-Object Name -NotMatch 'uninstall' | Select-Object -First 1
if ($exe) { Start-Process $exe.FullName }
Write-Host 'irch is in your Start menu, and updates itself.'
