# RuneAgent installer for Windows - Atoryn Studio
#
#   irm {{.Origin}}{{.Base}}/install.ps1 | iex
#
# Installs the newest signed release of rune.exe for this machine into
# %USERPROFILE%\.local\bin (Administrator not needed, no system change). Every
# download is verified against the release signing key below BEFORE anything is
# installed; any mismatch stops the installer with nothing changed.
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
# Linux and macOS have their own installer: {{.Origin}}{{.Base}}/install.sh
#
# Options (environment, set first in the same PowerShell window):
#   $env:RUNE_VERSION = 'X.Y.Z'      $env:RUNE_INSTALL_DIR = 'C:\absolute\folder'
#
# Runs in Windows PowerShell 5.1 and PowerShell 7. Everything happens inside
# one script block, called on the last line: a download cut short can never
# run half a script, and nothing here ever closes your PowerShell window.

& {
Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Origin = {{psq .Origin}}
$Base = {{psq .Base}}
$Latest = {{psq .Version}}
$KeyFingerprint = {{psq .Fingerprint}}
# The release key: ECDSA P-256, its point's X and Y in hex. The signature on
# the checksum list is checked with Windows' own cryptography (CNG).
$KeyX = {{psq .KeyX}}
$KeyY = {{psq .KeyY}}

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
    Out ('  ' + $Gbad + 'rune installer: ' + $Message) 'Red'
    throw [System.InvalidOperationException]::new('rune installer: ' + $Message)
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

function ConvertFrom-Hex([string]$Hex) {
    $bytes = New-Object byte[] ($Hex.Length / 2)
    for ($i = 0; $i -lt $bytes.Length; $i++) { $bytes[$i] = [Convert]::ToByte($Hex.Substring(2 * $i, 2), 16) }
    return ,$bytes
}

# OpenSSL writes an ECDSA signature as DER: SEQUENCE { INTEGER r, INTEGER s }.
# Windows checks the plain 64-byte form r || s. Anything that is not exactly
# one such sequence is refused.
function ConvertFrom-DerSignature([byte[]]$Der) {
    if ($Der.Length -lt 8 -or $Der.Length -gt 72 -or $Der[0] -ne 0x30 -or $Der[1] -ne ($Der.Length - 2)) { return $null }
    $raw = New-Object byte[] 64
    $pos = 2
    foreach ($half in 0, 1) {
        if ($pos + 2 -gt $Der.Length -or $Der[$pos] -ne 0x02) { return $null }
        $length = [int]$Der[$pos + 1]
        $pos += 2
        if ($length -lt 1 -or $length -gt 33 -or $pos + $length -gt $Der.Length) { return $null }
        $start = $pos
        $count = $length
        if ($count -eq 33) {
            if ($Der[$start] -ne 0) { return $null }
            $start++
            $count--
        }
        [Array]::Copy($Der, $start, $raw, $half * 32 + (32 - $count), $count)
        $pos += $length
    }
    if ($pos -ne $Der.Length) { return $null }
    return ,$raw
}

function Test-ReleaseSignature([byte[]]$Data, [byte[]]$Der) {
    $raw = ConvertFrom-DerSignature $Der
    if ($null -eq $raw) { return $false }
    # BCRYPT_ECCKEY_BLOB: "ECS1" (an ECDSA P-256 public key), 32-byte coordinates, then X and Y.
    $blob = [byte[]](@(0x45, 0x43, 0x53, 0x31, 0x20, 0x00, 0x00, 0x00) + (ConvertFrom-Hex $KeyX) + (ConvertFrom-Hex $KeyY))
    $key = [System.Security.Cryptography.CngKey]::Import($blob, [System.Security.Cryptography.CngKeyBlobFormat]::EccPublicBlob)
    $ecdsa = New-Object System.Security.Cryptography.ECDsaCng -ArgumentList $key
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return $ecdsa.VerifyHash($sha.ComputeHash($Data), $raw)
    } finally {
        $sha.Dispose()
        $ecdsa.Dispose()
        $key.Dispose()
    }
}

function Get-Download([string]$Url, [string]$Path) {
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Path
    } catch {
        Fail "download failed: $Url"
    }
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
    if ([string]::IsNullOrEmpty($KeyX) -or [string]::IsNullOrEmpty($Latest)) {
        Fail "no release has been published yet - see $Origin$Base/"
    }
    if ($PSVersionTable.PSEdition -eq 'Core' -and -not $IsWindows) {
        Fail "this installer is for Windows; on Linux and macOS use $Origin$Base/install.sh"
    }
    $os = [Environment]::OSVersion.Version
    if ($os.Major -lt 10 -or ($os.Major -eq 10 -and $os.Build -lt 17763)) {
        Fail "RuneAgent needs Windows 10 version 1809 or later"
    }
    $principal = New-Object Security.Principal.WindowsPrincipal ([Security.Principal.WindowsIdentity]::GetCurrent())
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        # Nothing here needs Administrator. An elevated window still installs for the account it runs as;
        # Rune itself should be started from a normal terminal.
        Warn "this PowerShell runs as Administrator; RuneAgent is installed for the account $env:USERNAME. Start Rune itself from a normal terminal."
    }

    # The machine's own processor, from the system's environment in the registry: a PowerShell that
    # runs emulated as x64 on ARM64 is told "x64" by .NET Framework and by its process environment.
    $machine = $null
    try {
        $machine = (Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment' -Name PROCESSOR_ARCHITECTURE -ErrorAction Stop).PROCESSOR_ARCHITECTURE
    } catch { }
    if (-not $machine) {
        try { $machine = [string][System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture } catch { }
    }
    if (-not $machine) {
        $machine = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    }
    switch -regex ($machine) {
        '^(X64|AMD64)$' { $arch = 'x86_64'; $archName = 'x64' }
        '^(Arm64|ARM64)$' { $arch = 'aarch64'; $archName = 'ARM64' }
        default { Fail "unsupported processor: $machine" }
    }

    $version = if ($env:RUNE_VERSION) { $env:RUNE_VERSION } else { $Latest }
    if ($version -notmatch '^(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})$') {
        Fail 'RUNE_VERSION must look like 1.2.3'
    }
    $dir = if ($env:RUNE_INSTALL_DIR) { $env:RUNE_INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
    if ($dir -notmatch '^([A-Za-z]:\\|\\\\)') {
        Fail 'RUNE_INSTALL_DIR must be an absolute path, such as C:\Tools\rune'
    }

    if ($Origin -like 'https://*') {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    }
    $name = "rune-$version-windows-$arch.zip"
    $url = "$Origin$Base/download/$version"
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('rune-install-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Header $version "Windows $archName"
        Note 'Fetching signed release list'
        Get-Download "$url/SHA256SUMS" (Join-Path $tmp 'SHA256SUMS')
        Get-Download "$url/SHA256SUMS.sig" (Join-Path $tmp 'SHA256SUMS.sig')

        # 1. The checksum list must carry the release key's signature.
        $sums = [System.IO.File]::ReadAllBytes((Join-Path $tmp 'SHA256SUMS'))
        $signature = [System.IO.File]::ReadAllBytes((Join-Path $tmp 'SHA256SUMS.sig'))
        $verified = $false
        try { $verified = Test-ReleaseSignature $sums $signature } catch { $verified = $false }
        if (-not $verified) {
            Fail "SIGNATURE CHECK FAILED for $version - nothing was installed. Do not use these files."
        }
        Step "Signature verified  (key $KeyFingerprint)"

        # 2. The archive must be in that signed list with exactly that hash.
        $expected = $null
        foreach ($line in [System.Text.Encoding]::ASCII.GetString($sums) -split '\r?\n') {
            if ($line -match '^([0-9a-f]{64})  (\S+)$' -and $Matches[2] -eq $name) { $expected = $Matches[1] }
        }
        if (-not $expected) { Fail "no build for Windows $archName in release $version" }
        Note "Downloading $name"
        $archive = Join-Path $tmp $name
        Get-Download "$url/$name" $archive
        $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            Fail "CHECKSUM MISMATCH for $name - nothing was installed. Do not use this file."
        }
        Step "Checksum verified  (SHA-256)"

        # 3. The archive holds only plain files with plain names.
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [System.IO.Compression.ZipFile]::OpenRead($archive)
        try {
            foreach ($entry in $zip.Entries) {
                if (@('rune.exe', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt') -notcontains $entry.FullName) {
                    Fail "unexpected file in the archive: $($entry.FullName)"
                }
            }
        } finally {
            $zip.Dispose()
        }
        $x = Join-Path $tmp 'x'
        [System.IO.Compression.ZipFile]::ExtractToDirectory($archive, $x)
        $program = Join-Path $x 'rune.exe'
        if (-not (Test-Path -LiteralPath $program -PathType Leaf)) { Fail 'the archive holds no rune.exe' }
        $reported = ''
        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try { $reported = (& $program --version 2>$null | Out-String) } catch { $reported = '' } finally { $ErrorActionPreference = $previous }
        if ($reported -notmatch [regex]::Escape($version)) { Fail 'the downloaded program does not run on this system' }

        # 4. Install. A running rune.exe cannot be replaced, but it can be renamed:
        #    windows already open keep their program, and the next Rune you open is the new one.
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $target = Join-Path $dir 'rune.exe'
        $staged = Join-Path $dir ('rune.new.' + $PID + '.exe')
        Copy-Item -LiteralPath $program -Destination $staged -Force
        if (Test-Path -LiteralPath $target) {
            Move-Item -LiteralPath $target -Destination (Join-Path $dir ('rune.old.' + [guid]::NewGuid().ToString('N').Substring(0, 8) + '.exe')) -Force
        }
        Move-Item -LiteralPath $staged -Destination $target -Force
        # Earlier programs that are no longer running go; one still in use stays until a later install.
        Get-ChildItem -LiteralPath $dir -Filter 'rune.old.*.exe' | ForEach-Object {
            try { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction Stop } catch { }
        }
        $docs = Join-Path $env:LOCALAPPDATA 'RuneAgent'
        New-Item -ItemType Directory -Force -Path $docs | Out-Null
        foreach ($file in 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt') {
            $from = Join-Path $x $file
            if (Test-Path -LiteralPath $from) { Copy-Item -LiteralPath $from -Destination (Join-Path $docs $file) -Force }
        }
        Step "Installed $target"

        # 5. Nothing left to set up: this window can run rune now, and new windows will too.
        Add-RuneToPath $dir
        Ready $version
        Write-Host 'The first time Rune starts, it asks once for Administrator confirmation to set up the account it runs commands in.'
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

try {
    Install-Rune
    $global:LASTEXITCODE = 0
} catch {
    # Fail already printed its own red line; anything else is shown here, once.
    if (-not $_.Exception.Message.StartsWith('rune installer: ')) {
        Out ('  ' + $Gbad + $_.Exception.Message) 'Red'
    }
    $global:LASTEXITCODE = 1
}
}
