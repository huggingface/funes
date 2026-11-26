# Install the funes binary from the Hugging Face bucket (huggingface/funes), on Windows.
#
#   irm "https://huggingface.co/buckets/huggingface/funes/resolve/install.ps1" | iex
#
# Downloads the prebuilt binary, verifies it against its tagged release's checksum and version,
# installs it, and puts its directory on your user PATH. Runs on Windows PowerShell 5.1 and later.
# Parameters, when run from a file; piped to iex, set the environment variables instead:
#   -InstallDir <dir>   install dir          (default: ~\.local\bin; env: FUNES_INSTALL_DIR)
#   -Version <tag>      release tag to fetch (default: latest;        env: FUNES_VERSION)
#   -NoPathUpdate       leave the user PATH as it is
[CmdletBinding()]
param(
    [string]$InstallDir = $env:FUNES_INSTALL_DIR,
    [string]$Version = $env:FUNES_VERSION,
    [switch]$NoPathUpdate
)

$FunesRepo = 'huggingface/funes'
$FunesBucket = 'huggingface/funes'
$FunesInstallRev = 1  # bump when this script changes: its downloads name the revision

# The processor the system runs on, which a 32-bit PowerShell on 64-bit Windows reports apart.
function Get-FunesArchitecture {
    if ($env:PROCESSOR_ARCHITEW6432) { return $env:PROCESSOR_ARCHITEW6432 }
    return $env:PROCESSOR_ARCHITECTURE
}

function Get-FunesFile([string]$Uri, [string]$Destination, [string]$UserAgent) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $Destination -UserAgent $UserAgent
}

function Test-FunesVersion([string]$Value) {
    return $Value -cmatch '^[0-9]+\.[0-9]+\.[0-9]+$'
}

# The one value a VERSION marker holds.
function Read-FunesVersion([string]$Path) {
    $values = @([IO.File]::ReadAllText($Path) -split '\s+' | Where-Object { $_ })
    if ($values.Count -ne 1) { throw 'the VERSION marker does not hold exactly one value' }
    return $values[0]
}

# The digest SHA256SUMS names for $Asset. The manifest is read whole: a malformed line, or an
# asset listed twice, refuses it.
function Read-FunesDigest([string]$Path, [string]$Asset) {
    $seen = @{}
    $digest = $null
    foreach ($line in [IO.File]::ReadAllLines($Path)) {
        if ($line -cnotmatch '^([0-9a-f]{64})\s+([^\s/]+)$') { throw 'SHA256SUMS is malformed' }
        if ($seen.ContainsKey($Matches[2])) { throw "SHA256SUMS lists $($Matches[2]) twice" }
        $seen[$Matches[2]] = $true
        if ($Matches[2] -ceq $Asset) { $digest = $Matches[1] }
    }
    if (-not $digest) { throw "SHA256SUMS does not contain $Asset" }
    return $digest
}

# Whether $Dir is an entry of the PATH text $Path, however it is spelled there.
function Test-FunesOnPath([string]$Path, [string]$Dir) {
    $wanted = $Dir.TrimEnd('\')
    foreach ($entry in ($Path -split ';')) {
        if ($entry -and [Environment]::ExpandEnvironmentVariables($entry).TrimEnd('\') -ieq $wanted) { return $true }
    }
    return $false
}

# Append $Dir to the Path value under the registry key $Key, unless it is there already; whether
# it was appended. The value is read unexpanded and written back as the kind it was, so a PATH
# spelled with %VARIABLES% stays one: [Environment]::SetEnvironmentVariable would write their
# expansions back for good.
function Add-FunesToPath([string]$Dir, [string]$Key = 'HKCU:\Environment') {
    $item = Get-Item -LiteralPath $Key
    $name = @($item.GetValueNames() | Where-Object { $_ -ieq 'Path' })[0]
    $current = ''
    $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    if ($name) {
        $current = [string]$item.GetValue($name, '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $kind = $item.GetValueKind($name)
    } else { $name = 'Path' }
    if (Test-FunesOnPath $current $Dir) { return $false }
    $separator = if ($current -and -not $current.EndsWith(';')) { ';' } else { '' }
    Set-ItemProperty -LiteralPath $Key -Name $name -Value "$current$separator$Dir" -Type $kind
    return $true
}

# Put $Staged where $Target is. Windows will not write over a running program but lets one be
# renamed, and an agent's MCP server is funes running: the installed binary steps aside as
# <target>.<pid>.old, and comes back if the new one cannot take its place. It is deleted once
# replaced unless it is running, and then by a later install or `funes update`, which leaves the
# binary it replaced the same way.
function Move-FunesIntoPlace([string]$Staged, [string]$Target) {
    Get-ChildItem -LiteralPath (Split-Path -Parent $Target) -Filter ((Split-Path -Leaf $Target) + '.*.old') -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
    if (-not (Test-Path -LiteralPath $Target)) {
        [IO.File]::Move($Staged, $Target)
        return
    }
    # Numbered past an aside of this window's that still runs, which the sweep could not delete.
    $old = "$Target.$PID.old"
    for ($n = 1; Test-Path -LiteralPath $old; $n++) { $old = "$Target.$PID.$n.old" }
    [IO.File]::Move($Target, $old)
    try { [IO.File]::Move($Staged, $Target) } catch {
        [IO.File]::Move($old, $Target)
        throw
    }
    Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
}

function Install-Funes([string]$InstallDir, [string]$Version, [bool]$NoPathUpdate) {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'  # Windows PowerShell draws a progress bar per chunk

    # The one target the release workflow builds for Windows; anything else builds from source.
    $architecture = Get-FunesArchitecture
    if ($architecture -ne 'AMD64') {
        throw "funes: no prebuilt binary for Windows/$architecture. Build from source: https://github.com/$FunesRepo#building-from-source"
    }
    $asset = 'funes-x86_64-windows.exe'

    if (-not $Version) { $Version = 'latest' }
    $userAgent = "funes-install/$FunesInstallRev (windows-x86_64)"
    if ($Version -ne 'latest' -and (Test-FunesVersion $Version.TrimStart('v'))) {
        $userAgent = "$userAgent; requested/v$($Version.TrimStart('v'))"
    }

    if (-not $InstallDir) { $InstallDir = Join-Path $env:USERPROFILE '.local\bin' }
    # Against this session's location, ~ expanded, as any path given PowerShell; GetFullPath would
    # use the process's directory, which Set-Location never moves.
    $provider = $null; $drive = $null
    $InstallDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir, [ref]$provider, [ref]$drive)
    if ($provider.Name -ne 'FileSystem') { throw "funes: the install dir is not a folder: $InstallDir" }
    $null = New-Item -ItemType Directory -Force -Path $InstallDir
    $staging = Join-Path $InstallDir ('.funes-install.' + [Guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $staging
    try {
        $root = "https://huggingface.co/buckets/$FunesBucket/resolve"
        if ($Version -eq 'latest') {
            try { Get-FunesFile "$root/VERSION" (Join-Path $staging 'latest-version') $userAgent }
            catch { throw 'funes: could not resolve the latest release.' }
            try { $wanted = Read-FunesVersion (Join-Path $staging 'latest-version') }
            catch { throw 'funes: the latest release has an invalid VERSION marker.' }
        } else { $wanted = $Version.TrimStart('v') }
        if (-not (Test-FunesVersion $wanted)) { throw "funes: invalid release version: $wanted" }

        $tag = "v$wanted"
        $release = "$root/$tag"
        try {
            Get-FunesFile "$release/VERSION" (Join-Path $staging 'release-version') $userAgent
            Get-FunesFile "$release/SHA256SUMS" (Join-Path $staging 'SHA256SUMS') $userAgent
        } catch { throw "funes: release metadata is incomplete for $tag." }
        $tagged = $null
        try { $tagged = Read-FunesVersion (Join-Path $staging 'release-version') } catch { }
        if ($tagged -cne $wanted) { throw "funes: release metadata does not match $tag." }
        try { $expected = Read-FunesDigest (Join-Path $staging 'SHA256SUMS') $asset }
        catch { throw "funes: SHA256SUMS is malformed or does not contain $asset." }

        # Named as a program: it is run before it is installed.
        $binary = Join-Path $staging 'funes.exe'
        Write-Host "Downloading $asset ($tag)..."
        try { Get-FunesFile "$release/$asset" $binary $userAgent }
        catch { throw "funes: download failed: $release/$asset" }
        if ((Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant() -cne $expected) {
            throw "funes: checksum verification failed for $asset; nothing was installed."
        }
        $reported = $null
        try { $reported = & $binary --version 2>$null } catch { }
        if ($LASTEXITCODE -ne 0 -or "$reported".Trim() -cne "funes $wanted") {
            throw "funes: $asset does not report the expected version $wanted; nothing was installed."
        }

        $target = Join-Path $InstallDir 'funes.exe'
        Move-FunesIntoPlace $binary $target
        Write-Host "Installed funes -> $target"
        Write-Host ("$reported".Trim())

        if (Test-FunesOnPath $env:Path $InstallDir) { return }
        if ($NoPathUpdate) {
            Write-Host ''
            Write-Host "$InstallDir is not on your PATH. Add it to run funes by name."
            return
        }
        if (Add-FunesToPath $InstallDir) {
            # Tell running programs the environment changed, so a terminal opened next has the new
            # PATH: .NET broadcasts that on any change to a user variable, here one that removes
            # nothing.
            [Environment]::SetEnvironmentVariable('FUNES_INSTALL_PATH', $null, 'User')
        }
        $env:Path = "$env:Path;$InstallDir"
        Write-Host ''
        Write-Host "Added $InstallDir to your PATH: this terminal has it, and so will the ones you open next."
    } finally {
        Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# The installer's test sets this to dot-source the functions alone. Not `$MyInvocation`: piped to
# iex, the script reads its caller's, which is a dot-source in every GitHub Actions step.
if (-not $FunesInstallDefineOnly) {
    Install-Funes $InstallDir $Version $NoPathUpdate.IsPresent
}
