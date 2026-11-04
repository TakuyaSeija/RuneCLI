# RuneAgent offline installer for Windows - Atoryn Studio
#
#   Unpack the package, then in PowerShell:  powershell -ExecutionPolicy Bypass -File .\install.ps1
#
# Installs the rune.exe that sits beside this script into %USERPROFILE%\.local\bin
# (Administrator not needed, no system change, PATH left as it is). It downloads
# nothing: everything comes from this package, and every file is checked against
# the package's own SHA256SUMS before anything is installed, so a damaged or
# half-copied package stops here with nothing changed.
#
# What the checksum proves, honestly: the files belong together and match the
# package you hold. It does not prove who built it - that comes from where you
# got the package. The published package's own sha256 is listed in Atoryn's
# signed release, and that list can be verified against the release key.
#
# Options (environment, set first in the same window): $env:RUNE_INSTALL_DIR
#
# Runs in Windows PowerShell 5.1 and PowerShell 7. Nothing here ever closes
# your PowerShell window.

& {
Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Fail([string]$Message) { throw [System.InvalidOperationException]::new("rune offline installer: $Message") }
function Say([string]$Message) { Write-Host $Message }

$package = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $package) { $package = (Get-Location).Path }
$program = Join-Path $package 'rune.exe'
$sums = Join-Path $package 'SHA256SUMS'
if (-not (Test-Path -LiteralPath $program -PathType Leaf)) { Fail 'this folder holds no rune.exe - unpack the whole package first' }
if (-not (Test-Path -LiteralPath $sums -PathType Leaf)) { Fail 'this folder holds no SHA256SUMS - unpack the whole package first' }

# 1. Every listed file must match its hash, and the list must cover rune.exe.
$allowed = @('rune.exe', 'install.ps1', 'INSTALL.txt', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt')
$coversProgram = $false
foreach ($line in [System.IO.File]::ReadAllLines($sums)) {
    $trimmed = $line.Trim()
    if (-not $trimmed) { continue }
    $parts = $trimmed -split '\s+', 2
    if ($parts.Count -ne 2) { Fail "unreadable line in SHA256SUMS: $trimmed" }
    $hash = $parts[0].ToLowerInvariant()
    $name = ($parts[1] -replace '^\*', '').Trim()
    if ($allowed -notcontains $name) { Fail "unexpected file in the checksum list: $name" }
    $path = Join-Path $package $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { Fail "missing from the package: $name" }
    $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $hash) { Fail "CHECKSUM MISMATCH for $name - nothing was installed. Do not use this package." }
    if ($name -eq 'rune.exe') { $coversProgram = $true }
}
if (-not $coversProgram) { Fail 'the checksum list does not cover rune.exe' }
Say '  every file matches the package checksums'

# 2. The binary must run here and say what it is.
$version = (& $program --version 2>$null | Select-Object -First 1)
if (-not $version) { Fail 'the rune.exe in this package does not run on this system' }
Say "  $version (offline package)"

$dir = $env:RUNE_INSTALL_DIR
if (-not $dir) { $dir = Join-Path $env:USERPROFILE '.local\bin' }
if (-not [System.IO.Path]::IsPathRooted($dir)) { Fail 'RUNE_INSTALL_DIR must be an absolute path' }
New-Item -ItemType Directory -Path $dir -Force | Out-Null

# 3. Install. A running rune.exe cannot be replaced, but it can be renamed first.
$target = Join-Path $dir 'rune.exe'
if (Test-Path -LiteralPath $target) {
    $aside = Join-Path $dir ("rune.old." + [guid]::NewGuid().ToString('N'))
    try { Move-Item -LiteralPath $target -Destination $aside -Force } catch { Fail 'a running rune.exe could not be moved aside; close Rune and try again' }
}
Copy-Item -LiteralPath $program -Destination $target -Force

$docs = Join-Path $env:LOCALAPPDATA 'RuneAgent'
New-Item -ItemType Directory -Path $docs -Force | Out-Null
foreach ($file in @('LICENSE.txt', 'THIRD-PARTY-NOTICES.txt')) {
    $from = Join-Path $package $file
    if (Test-Path -LiteralPath $from) { Copy-Item -LiteralPath $from -Destination (Join-Path $docs $file) -Force }
}

Say "Installed $target ($version). License and notices: $docs"
Say "Keep this package's folder if you want to install it again later."
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($path -and ($path.Split(';') -contains $dir)) {
    Say 'Start it in a project folder with: rune'
} else {
    Say "Add $dir to your PATH, then start it in a project folder with: rune"
}
}
