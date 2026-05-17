# insmaller end-to-end demo. Runs ENTIRELY in a fresh temp folder OUTSIDE
# the project: copies the release binary + demo configs there, exercises the
# real install path (dry-run, real run, idempotent re-run) and the wizard
# (unattended), and asserts results. Safe: no network, no builds; all writes
# stay under the temp dir. The temp dir is left in place for inspection.
#
#   pwsh -File examples\run-demo.ps1
#
# (run from the project root, or pass -Exe / -DemoIn explicitly)

param(
  [string]$Exe    = "$PSScriptRoot\..\target\release\insmaller.exe",
  [string]$DemoIn = "$PSScriptRoot"
)

$ErrorActionPreference = "Stop"
$fail = 0
function Check($name, $ok) {
  if ($ok) { Write-Host "  PASS  $name" -ForegroundColor Green }
  else     { Write-Host "  FAIL  $name" -ForegroundColor Red; $script:fail = 1 }
}

# 1) fresh temp workspace OUTSIDE the project
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$work  = Join-Path $env:TEMP "insmaller-e2e-$stamp"
New-Item -ItemType Directory -Force -Path $work | Out-Null
$out = Join-Path $work "demo-out"
New-Item -ItemType Directory -Force -Path $out | Out-Null
Write-Host "workspace: $work"

# hermetic: the demo's sentinel dir lives under the user profile (NOT the
# temp dir), so wipe it first or a 2nd run short-circuits as "installed".
# `sentinel_dir_name = "insmaller-demo"` in demo.installer.toml.
$sdir = Join-Path $env:LOCALAPPDATA "insmaller-demo"
Remove-Item $sdir -Recurse -Force -ErrorAction SilentlyContinue

# 2) make it standalone: copy binary + demo configs into the temp dir
Copy-Item $Exe (Join-Path $work "insmaller.exe")
foreach ($f in @("demo.installer.toml","demo.catalog.json","demo.wizard.toml")) {
  Copy-Item (Join-Path $DemoIn $f) (Join-Path $work $f)
}
$bin = Join-Path $work "insmaller.exe"
$cfg = Join-Path $work "demo.installer.toml"
$cat = Join-Path $work "demo.catalog.json"
$wiz = Join-Path $work "demo.wizard.toml"

# the demo `prompt` step resolves DEMO_DIR from the env (EnvResolver path).
# forward slashes work cross-engine and in PowerShell redirection.
$env:DEMO_DIR = ($out -replace '\\','/')

Push-Location $work
try {
  # 3) dry-run first (sentinel not yet set) -> shows the plan, writes nothing
  Write-Host "`n[1] dry-run (plan only)"
  # catalog comes from demo.installer.toml [settings] catalog — only --config.
  & $bin install demo --config $cfg --dry-run
  Check "dry-run created no files" (-not (Test-Path (Join-Path $out "hello.txt")))

  # 4) real install -> exercises prompt+shell+ensure_line+copy+sentinel_meta
  Write-Host "`n[2] real install"
  & $bin install demo --config $cfg
  Check "exit ok"                 ($LASTEXITCODE -eq 0)
  Check "shell wrote hello.txt"   (Test-Path (Join-Path $out "hello.txt"))
  Check "ensure_line profile.txt" (Test-Path (Join-Path $out "profile.txt"))
  Check "copy copied/hello.txt"   (Test-Path (Join-Path $out "copied\hello.txt"))

  # 5) idempotent re-run -> sentinel short-circuits, summary still ok
  Write-Host "`n[3] idempotent re-run"
  & $bin install demo --config $cfg
  Check "re-run exit ok" ($LASTEXITCODE -eq 0)

  # 6) terse desugar path
  Write-Host "`n[4] terse spec (hello: desugar)"
  & $bin install hello --config $cfg
  Check "terse hello exit ok" ($LASTEXITCODE -eq 0)

  # 7) wizard, unattended (no TTY -> StaticAnswerer; --answers supplies it)
  Write-Host "`n[5] wizard (unattended, dry-run)"
  $ans = Join-Path $work "answers.toml"
  $demoDirToml = ($out -replace '\\','/')
  $lines = @('INSTALL_TOOLS = ["demo"]', "DEMO_DIR = `"$demoDirToml`"")
  Set-Content -Path $ans -Value $lines -Encoding utf8
  & $bin setup --config $cfg --answers $ans --dry-run
  Check "wizard setup exit ok" ($LASTEXITCODE -eq 0)

  # 8) theme: demo.installer.toml sets theme="high-contrast"; env must
  # override + parse cleanly even on the unattended path (Palette::resolve
  # runs before the wizard branch).
  Write-Host "`n[6] theme env override (NO_COLOR / INSMALLER_THEME)"
  $env:NO_COLOR = "1"
  & $bin setup --config $cfg --answers $ans --dry-run
  Check "NO_COLOR run exit ok" ($LASTEXITCODE -eq 0)
  Remove-Item Env:\NO_COLOR
  $env:INSMALLER_THEME = "mono"
  & $bin setup --config $cfg --answers $ans --dry-run
  Check "INSMALLER_THEME run exit ok" ($LASTEXITCODE -eq 0)
  Remove-Item Env:\INSMALLER_THEME

  # 9) zero-flag: name the config `insmaller.toml` in cwd -> discovered
  # automatically (no --config); catalog still comes from its [settings].
  Write-Host "`n[7] zero-flag config discovery (insmaller.toml in cwd)"
  Copy-Item $cfg (Join-Path $work "insmaller.toml")
  & $bin install demo --dry-run
  Check "discovered config, dry-run ok" ($LASTEXITCODE -eq 0)
  Remove-Item (Join-Path $work "insmaller.toml")
}
finally {
  Pop-Location
}

Write-Host ""
if ($fail -eq 0) { Write-Host "ALL DEMO CHECKS PASSED" -ForegroundColor Green }
else             { Write-Host "SOME CHECKS FAILED"     -ForegroundColor Red }
Write-Host "inspect: $work"
exit $fail
