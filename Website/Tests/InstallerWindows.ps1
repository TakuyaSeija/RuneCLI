# Install the staged release through install.ps1, as a person runs it, then
# start the installed program. CI (installer-windows) runs this file in
# Windows PowerShell 5.1 and in PowerShell 7 against a local site serving a
# release staged for that job only; RUNE_TEST_VERSION names its version.
$ErrorActionPreference = 'Stop'
$env:RUNE_INSTALL_DIR = Join-Path $env:RUNNER_TEMP 'bin'
$env:LOCALAPPDATA = Join-Path $env:RUNNER_TEMP 'local'
$site = 'http://127.0.0.1:18766/Studio/install.ps1'

irm $site | iex
if ($LASTEXITCODE -ne 0) { throw 'install.ps1 failed' }
$rune = Join-Path $env:RUNE_INSTALL_DIR 'rune.exe'
$reported = (& $rune --version | Out-String).Trim()
if ($reported -ne "rune $env:RUNE_TEST_VERSION") { throw "installed program says '$reported'" }

$env:RUNE_HOME = Join-Path $env:RUNNER_TEMP 'rune-home'
& $rune -config check
if ($LASTEXITCODE -ne 0) { throw 'rune -config check failed' }
if (-not (Test-Path (Join-Path $env:LOCALAPPDATA 'RuneAgent\LICENSE.txt'))) { throw 'license notices missing' }

# A second install over the first, as an update: the earlier program is set aside, not lost.
irm $site | iex
if ($LASTEXITCODE -ne 0) { throw 'reinstall failed' }
$reported = (& $rune --version | Out-String).Trim()
if ($reported -ne "rune $env:RUNE_TEST_VERSION") { throw "reinstalled program says '$reported'" }
Write-Host "installed, started and reinstalled rune $env:RUNE_TEST_VERSION"
exit 0
