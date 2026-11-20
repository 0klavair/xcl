# Build everything (the e2e test needs the test title + DLL) and run all tests.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
cargo build --workspace
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --workspace -- --test-threads=4
exit $LASTEXITCODE
