$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') {
  throw 'Build the EXE installer on Windows with the MSVC C++ build tools installed.'
}
Push-Location (Join-Path $PSScriptRoot '..')
try {
  if (-not $env:npm_config_cache) { $env:npm_config_cache = Join-Path $PWD '.npm-cache' }
  npm exec --yes --package '@tauri-apps/cli@2.12.4' -- tauri build --features desktop-app --bundles nsis @args -- --locked
  if ($LASTEXITCODE -ne 0) { throw 'Windows installer build failed.' }
} finally {
  Pop-Location
}
