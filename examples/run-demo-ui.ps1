# Interactive counterpart to run-demo.ps1: launches the REAL ratatui setup
# TUI (not unattended) so you can see and drive the wizard. Must be run in a
# real terminal (it needs a TTY); pipes/CI will just fall back to unattended.
#
#   pwsh -File examples\run-demo-ui.ps1
#   pwsh -File examples\run-demo-ui.ps1 -Theme mono        # try a preset
#   pwsh -File examples\run-demo-ui.ps1 -NoColor           # force mono via env
#
# Keys once it's up: Tab/←→ focus · ↑↓ move · Space toggle · Enter Next ·
# Esc Back · q quit. Page 1 picks tools, page 2 asks for an output dir, then
# it installs the selection (writes only under the temp dir).

param(
  [string]$Exe     = "$PSScriptRoot\..\target\release\insmaller.exe",
  [string]$DemoIn  = "$PSScriptRoot",
  [string]$Theme   = "",        # "" = use demo.installer.toml; or modern|default|high-contrast|mono
  [switch]$NoColor
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Exe)) {
  Write-Host "binary not found: $Exe" -ForegroundColor Red
  Write-Host "build it first:  cargo build --offline --release -p insmaller"
  exit 1
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$work  = Join-Path $env:TEMP "insmaller-ui-$stamp"
$out   = Join-Path $work "demo-out"
New-Item -ItemType Directory -Force -Path $out | Out-Null

# wipe the demo sentinel so the wizard actually runs the steps (not a
# short-circuit "already installed").
Remove-Item (Join-Path $env:LOCALAPPDATA "insmaller-demo") -Recurse -Force -ErrorAction SilentlyContinue

Copy-Item $Exe (Join-Path $work "insmaller.exe")
foreach ($f in @("demo.installer.toml","demo.catalog.json","demo.wizard.toml")) {
  Copy-Item (Join-Path $DemoIn $f) (Join-Path $work $f)
}
$bin = Join-Path $work "insmaller.exe"
$env:DEMO_DIR = ($out -replace '\\','/')

if ($NoColor)            { $env:NO_COLOR = "1" }      else { Remove-Item Env:\NO_COLOR -ErrorAction SilentlyContinue }
if ($Theme -ne "")       { $env:INSMALLER_THEME = $Theme } else { Remove-Item Env:\INSMALLER_THEME -ErrorAction SilentlyContinue }

Write-Host "workspace: $work"
Write-Host "launching interactive setup (q to quit)..." -ForegroundColor Cyan
Push-Location $work
try {
  # catalog + wizard come from demo.installer.toml [settings].
  & $bin setup --config demo.installer.toml
} finally {
  Pop-Location
  Remove-Item Env:\NO_COLOR, Env:\INSMALLER_THEME -ErrorAction SilentlyContinue
}
Write-Host "`ninspect output: $out"
