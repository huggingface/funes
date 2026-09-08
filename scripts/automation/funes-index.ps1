# Native Windows per-turn index hook. The foreground drains stdin and detaches a worker.
param(
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
    & $binary @Arguments 2>&1 | ForEach-Object { Add-Content -LiteralPath $logPath -Encoding UTF8 -Value "$_" }
    return $LASTEXITCODE
}
function Quote-Literal([string]$Value) { "'" + $Value.Replace("'", "''") + "'" }

try {
    if (-not $Worker) {
        $null = [Console]::In.ReadToEnd()
        $command = '& ' + (Quote-Literal $PSCommandPath) + ' -Worker -Harness ' + (Quote-Literal $Harness)
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
        Write-Log 'index ABORT: funes not found; set FUNES_BIN to the full executable path.'
        exit 0
    }
    Write-Log "index[$Harness]: start"
    $result = Invoke-Funes -Arguments @('index', '--harness', $Harness)
    if ($result -eq 0) { Write-Log "index[$Harness]: ok" }
    else { Write-Log "index[$Harness]: FAILED (exit $result)" }
} catch {
    Write-Log "index: FAILED ($_ )"
}
exit 0
