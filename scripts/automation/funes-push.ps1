# Native Windows boundary hook: retry indexing, then publish the stored memory.
param(
    [Parameter(Mandatory = $true)][string]$Memory,
    [string]$Harness = 'codex',
    [switch]$Worker
)
$ErrorActionPreference = 'Stop'
$logPath = Join-Path $PSScriptRoot 'funes-sync.log'
function Write-Log([string]$Message) {
    Add-Content -LiteralPath $logPath -Encoding UTF8 -Value ("{0:o} {1}" -f (Get-Date), $Message)
}
function Invoke-Funes([string[]]$Arguments) {
    $ErrorActionPreference = "Continue"
    $null | & $binary @Arguments 2>&1 | ForEach-Object { Add-Content -LiteralPath $logPath -Encoding UTF8 -Value "$_" }
    return $LASTEXITCODE
}
function Quote-Literal([string]$Value) { "'" + $Value.Replace("'", "''") + "'" }

try {
    if (-not $Worker) {
        $null = [Console]::In.ReadToEnd()
        $command = '& ' + (Quote-Literal $PSCommandPath) + ' -Worker -Memory ' + (Quote-Literal $Memory) +
            ' -Harness ' + (Quote-Literal $Harness)
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
        $process = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe') -WindowStyle Hidden -WorkingDirectory (Get-Location).Path -PassThru `
            -ArgumentList @('-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded)
        $process.Dispose()
        exit 0
    }

    $binary = $env:FUNES_BIN
    if (-not $binary) {
        $found = Get-Command funes.exe -CommandType Application -ErrorAction SilentlyContinue
        if ($found) { $binary = $found.Source }
    }
    if (-not $binary -and $env:LOCALAPPDATA) {
        $binary = Join-Path $env:LOCALAPPDATA 'Programs\funes\bin\funes.exe'
    }
    if (-not $binary -or -not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        Write-Log 'push ABORT: funes not found; set FUNES_BIN to the full executable path.'
        exit 0
    }
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        $result = Invoke-Funes -Arguments @('index', '--harness', $Harness)
        if ($result -eq 0) { Write-Log "index[$Harness]: ok (before push)"; break }
        Write-Log "index[$Harness]: busy or failed, retry $attempt"
        Start-Sleep -Seconds 2
    }
    Write-Log "push: start ($Memory)"
    $result = Invoke-Funes -Arguments @('push', $Memory)
    switch ($result) {
        0 { Write-Log 'push: ok' }
        2 { Write-Log 'push: WARN - secrets held back; run funes scrub, then retry' }
        default { Write-Log "push: FAILED (exit $result)" }
    }
} catch {
    Write-Log "push: FAILED ($_ )"
}
exit 0
