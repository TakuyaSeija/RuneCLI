# RuneAgent offline installer for Windows - Atoryn Studio
#
#   Unpack the package, then in PowerShell:  powershell -ExecutionPolicy Bypass -File .\install.ps1
#
# Installs the rune.exe that sits beside this script into %USERPROFILE%\.local\bin
# (Administrator not needed, no system change). It downloads nothing: everything
# comes from this package, and every file is checked against the package's own
# SHA256SUMS before anything is installed, so a damaged or half-copied package
# stops here with nothing changed.
#
# What the checksum proves, honestly: the files belong together and match the
# package you hold. It does not prove who built it - that comes from where you
# got the package.
#
# So that no setup step is left for you, it adds that folder to this window's
# PATH at once and to your own user PATH in the registry, so new PowerShell
# windows find rune too, and it says which of the two it did. It never edits a
# shell profile.
#
# The screen follows the same language as the Linux and macOS installer: one
# quiet header line, green for a finished step, yellow for a warning, red for a
# failure, one accent colour for work in progress, and the single command you
# have to type set in bold between two thin rules, as the last line on screen.
# This file stays ASCII: the glyphs are code points, chosen only when the console
# is UTF-8, so a plain console reads them as ok / -- / ! instead.
#
# Options (environment, set first in the same window): $env:RUNE_INSTALL_DIR
#
# Runs in Windows PowerShell 5.1 and PowerShell 7. Nothing here ever closes
# your PowerShell window.

& {
Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# ---------------------------------------------------------------------------
# Terminal: colour and glyphs
# ---------------------------------------------------------------------------
$script:Color = -not $env:NO_COLOR
$script:Utf = $false
try { if ([Console]::OutputEncoding.CodePage -eq 65001) { $script:Utf = $true } } catch { }

if ($script:Utf) {
    $Gok = [string][char]0x2713 + '  '      # check mark
    $Gnote = [string][char]0x2022 + '  '    # bullet
    $Gbad = [string][char]0x2717 + '  '     # ballot x
    $Gwarn = '!  '
    $Grule = [string][char]0x2500           # box drawing horizontal
    $Gdot = [string][char]0x00B7            # middle dot
    $Ind = '     '
} else {
    $Gok = 'ok  '
    $Gnote = '--  '
    $Gbad = 'x   '
    $Gwarn = '!   '
    $Grule = '-'
    $Gdot = '-'
    $Ind = '      '
}

function Out([string]$Text, [string]$Ink) {
    if ($script:Color -and $Ink) { Write-Host $Text -ForegroundColor $Ink } else { Write-Host $Text }
}
function Step([string]$Message) { Out ('  ' + $Gok + $Message) 'Green' }
function Note([string]$Message) { Out ('  ' + $Gnote + $Message) 'DarkGray' }
function Warn([string]$Message) { Out ('  ' + $Gwarn + $Message) 'Yellow' }
function Fail([string]$Message) {
    Out ('  ' + $Gbad + 'rune offline installer: ' + $Message) 'Red'
    throw [System.InvalidOperationException]::new('rune offline installer: ' + $Message)
}

# One quiet header line, the same one every installer prints.
function Header([string]$Version, [string]$Platform) {
    Write-Host ''
    if ($script:Color) {
        Write-Host '  ' -NoNewline
        Write-Host ('RuneAgent ' + $Version) -ForegroundColor White -NoNewline
        Write-Host ('  ' + $Gdot + '  ' + $Platform + '  ' + $Gdot + '  Atoryn Studio') -ForegroundColor DarkGray
    } else {
        Write-Host ('  RuneAgent ' + $Version + '  ' + $Gdot + '  ' + $Platform + '  ' + $Gdot + '  Atoryn Studio')
    }
    Write-Host ''
}

# The one thing to type: bold between two thin rules, no background, and always
# the last line on its screen.
function CmdLine([string]$Command) {
    Write-Host ''
    if (-not $script:Color) { Write-Host ('  ' + $Command); return }
    $rule = $Grule * $Command.Length
    Out ('  ' + $rule) 'DarkGray'
    Write-Host '  ' -NoNewline
    Write-Host $Command -ForegroundColor White
    Out ('  ' + $rule) 'DarkGray'
}

function Ready([string]$Version) {
    Write-Host ''
    if (-not $script:Color) {
        Write-Host 'Ready. Start it in a project folder with: rune'
        return
    }
    Write-Host ('  RuneAgent ' + $Version + ' is ready. Start it in a project folder:')
    CmdLine 'rune'
    Write-Host ''
}

# Add-RuneToPath dir: this window at once, and the person's own PATH so new
# windows find rune without any further step. Says exactly what it changed.
function Add-RuneToPath([string]$Dir) {
    $inWindow = @($env:PATH -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ieq $Dir.TrimEnd('\') }).Count -gt 0
    if (-not $inWindow) {
        $env:PATH = "$env:PATH;$Dir"
    }
    $inUserPath = $false
    try {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $parts = if ([string]::IsNullOrEmpty($userPath)) { @() } else { $userPath -split ';' }
        $inUserPath = @($parts | Where-Object { $_ -and $_.TrimEnd('\') -ieq $Dir.TrimEnd('\') }).Count -gt 0
        if (-not $inUserPath) {
            $newUserPath = if ([string]::IsNullOrEmpty($userPath)) { $Dir } else { "$userPath;$Dir" }
            [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
            $inUserPath = $true
        }
    } catch {
        Warn "could not add $Dir to your user PATH; add it yourself in System settings, Environment variables"
    }
    if ($inWindow -and $inUserPath) {
        Step "$Dir is already on your PATH"
    } elseif ($inUserPath) {
        Step "Added $Dir to your PATH  (this window and new windows)"
    } else {
        Step "Added $Dir to this window's PATH"
    }
}

function Install-Rune {
    $package = Split-Path -Parent $MyInvocation.MyCommand.Path
    if (-not $package) { $package = (Get-Location).Path }
    $program = Join-Path $package 'rune.exe'
    $sums = Join-Path $package 'SHA256SUMS'
    if (-not (Test-Path -LiteralPath $program -PathType Leaf)) { Fail 'this folder holds no rune.exe - unpack the whole package first' }
    if (-not (Test-Path -LiteralPath $sums -PathType Leaf)) { Fail 'this folder holds no SHA256SUMS - unpack the whole package first' }

    $dir = $env:RUNE_INSTALL_DIR
    if (-not $dir) { $dir = Join-Path $env:USERPROFILE '.local\bin' }
    if (-not [System.IO.Path]::IsPathRooted($dir)) { Fail 'RUNE_INSTALL_DIR must be an absolute path' }

    # 1. Every listed file must match its hash, and the list must cover rune.exe.
    Note 'Checking every file against the package checksums'
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

    # 2. The binary must run here and say what it is.
    $reported = ''
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $reported = (& $program --version 2>$null | Out-String).Trim() } catch { $reported = '' } finally { $ErrorActionPreference = $previous }
    if ($reported -notmatch '^rune (\d+\.\d+\.\d+)') { Fail 'the rune.exe in this package does not run on this system' }
    $version = $Matches[1]

    Header $version 'Windows (offline package)'
    Step 'Package checksums verified  (SHA-256)'

    # 3. Install. A running rune.exe cannot be replaced, but it can be renamed first.
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    $target = Join-Path $dir 'rune.exe'
    if (Test-Path -LiteralPath $target) {
        $aside = Join-Path $dir ("rune.old." + [guid]::NewGuid().ToString('N'))
        try { Move-Item -LiteralPath $target -Destination $aside -Force } catch { Fail 'a running rune.exe could not be moved aside; close Rune and try again' }
    }
    Copy-Item -LiteralPath $program -Destination $target -Force
    Step "Installed $target"

    $docs = Join-Path $env:LOCALAPPDATA 'RuneAgent'
    New-Item -ItemType Directory -Path $docs -Force | Out-Null
    foreach ($file in @('LICENSE.txt', 'THIRD-PARTY-NOTICES.txt')) {
        $from = Join-Path $package $file
        if (Test-Path -LiteralPath $from) { Copy-Item -LiteralPath $from -Destination (Join-Path $docs $file) -Force }
    }
    Note "License and notices: $docs"
    Note "Keep this package's folder if you want to install it again later."

    # 4. Nothing left to set up: this window can run rune now, and new windows will too.
    Add-RuneToPath $dir
    Ready $version
}

try {
    Install-Rune
    $global:LASTEXITCODE = 0
} catch {
    if (-not $_.Exception.Message.StartsWith('rune offline installer: ')) {
        Out ('  ' + $Gbad + $_.Exception.Message) 'Red'
    }
    $global:LASTEXITCODE = 1
}
}
