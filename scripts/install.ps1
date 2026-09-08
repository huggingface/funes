# Download and verify a native x86-64 Windows release before replacing an existing installation.
[CmdletBinding()]
param(
    [string]$InstallDir = $env:FUNES_INSTALL_DIR,
    [string]$Version = 'latest',
    [switch]$NoPathUpdate
)

function Get-FunesFile([string]$Uri, [string]$Destination) {
    if (-not $Uri.StartsWith('https://')) { throw 'Release downloads require HTTPS.' }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $Destination
}

function Get-FunesArchitecture {
    if ($env:PROCESSOR_ARCHITEW6432) { return $env:PROCESSOR_ARCHITEW6432 }
    return $env:PROCESSOR_ARCHITECTURE
}

function Read-FunesVersion([string]$Path) {
    $value = [IO.File]::ReadAllText($Path).Trim()
    if ($value -notmatch '^v?([0-9]+\.[0-9]+\.[0-9]+)$') { throw 'Invalid release VERSION metadata.' }
    return $Matches[1]
}

function Read-FunesDigest([string]$Path, [string]$Asset) {
    $seen = @{}
    $digest = $null
    foreach ($line in [IO.File]::ReadAllLines($Path)) {
        if ($line -cnotmatch '^([0-9a-f]{64})  ([A-Za-z0-9_.-]+)$') { throw 'Malformed SHA256SUMS manifest.' }
        $hash = $Matches[1]
        $name = $Matches[2]
        if ($seen.ContainsKey($name)) { throw 'Duplicate release asset in SHA256SUMS.' }
        $seen[$name] = $true
        if ($name -ceq $Asset) { $digest = $hash }
    }
    if (-not $digest) { throw "Missing checksum for $Asset." }
    return $digest
}

function Install-Funes([string]$Destination, [string]$RequestedVersion, [bool]$SkipPathUpdate) {
    $ErrorActionPreference = 'Stop'
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT -or (Get-FunesArchitecture) -ne 'AMD64') {
        throw 'This installer supports native Windows x86-64 only.'
    }
    if (-not $env:LOCALAPPDATA -and -not $Destination) { throw 'Set -InstallDir because LOCALAPPDATA is unavailable.' }
    $defaultDir = if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA 'Programs\funes\bin' } else { $null }
    if (-not $Destination) { $Destination = $defaultDir }
    $Destination = [IO.Path]::GetFullPath($Destination)
    if ($RequestedVersion -ne 'latest' -and $RequestedVersion -notmatch '^v?[0-9]+\.[0-9]+\.[0-9]+$') {
        throw 'Use -Version latest or a release version such as 1.3.0.'
    }
    $null = New-Item -ItemType Directory -Force -Path $Destination
    $staging = Join-Path $Destination ('.funes-install-' + [Guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $staging
    $base = 'https://huggingface.co/buckets/huggingface/funes/resolve'
    $asset = 'funes-x86_64-windows.exe'
    try {
        $metadata = Join-Path $staging 'VERSION'
        if ($RequestedVersion -eq 'latest') {
            Get-FunesFile "$base/VERSION" $metadata
            $wanted = Read-FunesVersion $metadata
        } else { $wanted = $RequestedVersion.TrimStart('v') }
        $tag = "v$wanted"
        Get-FunesFile "$base/$tag/VERSION" $metadata
        if ((Read-FunesVersion $metadata) -cne $wanted) { throw 'Tagged VERSION does not match the requested release.' }
        $manifest = Join-Path $staging 'SHA256SUMS'
        Get-FunesFile "$base/$tag/SHA256SUMS" $manifest
        $expected = Read-FunesDigest $manifest $asset
        $staged = Join-Path $staging 'funes.exe'
        Get-FunesFile "$base/$tag/$asset" $staged
        if ((Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash.ToLowerInvariant() -cne $expected) {
            throw 'Release checksum mismatch; the installed binary was not changed.'
        }
        $reported = & $staged --version
        if ($LASTEXITCODE -ne 0 -or "$reported".Trim() -cne "funes $wanted") {
            throw 'Downloaded binary reports an unexpected version; the installed binary was not changed.'
        }
        $target = Join-Path $Destination 'funes.exe'
        for ($attempt = 1; $attempt -le 5; $attempt++) {
            try {
                if ([IO.File]::Exists($target)) {
                    [IO.File]::Replace($staged, $target, (Join-Path $staging 'previous.exe'))
                } else { [IO.File]::Move($staged, $target) }
                break
            } catch {
                if ($attempt -eq 5) { throw "Cannot replace $target. Close running agents and MCP servers, then rerun the installer. $($_.Exception.Message)" }
                Start-Sleep -Milliseconds 500
            }
        }
        if (-not $SkipPathUpdate -and $defaultDir -and $Destination.TrimEnd('\') -ieq $defaultDir.TrimEnd('\')) {
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            $entries = @($userPath -split ';' | Where-Object { $_ })
            if (-not ($entries | Where-Object { $_.TrimEnd('\') -ieq $Destination.TrimEnd('\') })) {
                [Environment]::SetEnvironmentVariable('Path', (($entries + $Destination) -join ';'), 'User')
            }
            Write-Host 'Open a new PowerShell or CMD window to use the updated PATH.'
        } else { Write-Host "Run $target, or add $Destination to PATH." }
        Write-Host "Installed funes $wanted at $target"
    } finally {
        if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    Install-Funes $InstallDir $Version $NoPathUpdate.IsPresent
}
