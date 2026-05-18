# Runs exactly what .github/workflows/ci.yml runs, with the pinned toolchain
# from rust-toolchain.toml. Green here means CI's test+lint jobs are green
# (OS-specific failures on the other runners still cannot be reproduced from
# one machine). Run before pushing.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

Write-Host "toolchain:" (rustc --version)

Write-Host "`n[1/3] cargo test --workspace --locked"
cargo test --workspace --locked
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "`n[2/3] cargo build --workspace --locked --features cdylib"
cargo build --workspace --locked --features cdylib
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "`n[3/3] cargo clippy --workspace --locked -- -D warnings"
cargo clippy --workspace --locked -- -D warnings
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "`npreflight OK - CI test+lint will pass on this toolchain" -ForegroundColor Green
