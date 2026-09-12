# Hermetic installer and detached-hook tests on Windows PowerShell 5.1.
$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ("funes test $([char]0x6D4B)$([char]0x8BD5) & (x) ' " + [Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory $root
$savedLocal = $env:LOCALAPPDATA
$savedBin = $env:FUNES_BIN
$savedUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Must-Fail([scriptblock]$Action) {
    $failed = $false
    try { & $Action } catch { $failed = $true }
    Assert $failed 'Expected the operation to fail'
}
try {
    $fake = Join-Path $root 'fake.exe'
    Add-Type -OutputAssembly $fake -OutputType ConsoleApplication -TypeDefinition @'
using System;
using System.IO;
using System.Threading;
public class FunesFixture {
    public static int Main(string[] args) {
        string log = Environment.GetEnvironmentVariable("FUNES_TEST_LOG");
        if (args.Length > 0 && args[0] == "--version") {
            if (!String.IsNullOrEmpty(log)) File.AppendAllText(log, "version\n");
            Console.WriteLine("funes " + (Environment.GetEnvironmentVariable("FUNES_TEST_VERSION") ?? "1.3.0"));
            return 0;
        }
        if (!Console.IsInputRedirected) return 42;
        File.AppendAllText(log, String.Join("|", args) + "\n");
        Console.Error.WriteLine("fixture diagnostic on stderr");
        if (args[0] == "index") Thread.Sleep(Int32.Parse(Environment.GetEnvironmentVariable("FUNES_TEST_DELAY") ?? "0"));
        File.AppendAllText(log, "done:" + args[0] + "\n");
        return args[0] == "push" ? 2 : 0;
    }
}
'@
    $fixture = Join-Path $root 'release'
    $null = New-Item -ItemType Directory $fixture
    Copy-Item $fake (Join-Path $fixture 'funes-x86_64-windows.exe')
    [IO.File]::WriteAllText((Join-Path $fixture 'VERSION'), "1.3.0`n")
    $hash = (Get-FileHash $fake -Algorithm SHA256).Hash.ToLowerInvariant()
    $validManifest = "$hash  funes-x86_64-windows.exe`n"
    [IO.File]::WriteAllText((Join-Path $fixture 'SHA256SUMS'), $validManifest)
    . (Join-Path $PSScriptRoot 'install.ps1')
    function Get-FunesFile([string]$Uri, [string]$Destination) {
        Assert ($Uri.StartsWith('https://')) 'Download must use HTTPS'
        Copy-Item -LiteralPath (Join-Path $fixture ($Uri.Split('/')[-1])) -Destination $Destination
    }
    $env:LOCALAPPDATA = Join-Path $root 'profile'
    $env:FUNES_TEST_LOG = Join-Path $root 'calls.txt'
    $destination = Join-Path $root 'installed bin'
    Install-Funes $destination '1.3.0' $true
    $target = Join-Path $destination 'funes.exe'
    Assert (Test-Path -LiteralPath $target) 'Pinned installation failed'
    Install-Funes $destination 'latest' $true
    $before = (Get-FileHash -LiteralPath $target).Hash

    Remove-Item -LiteralPath $env:FUNES_TEST_LOG
    [IO.File]::WriteAllText((Join-Path $fixture 'SHA256SUMS'), (('0' * 64) + "  funes-x86_64-windows.exe`n"))
    Must-Fail { Install-Funes $destination '1.3.0' $true }
    Assert (-not (Test-Path -LiteralPath $env:FUNES_TEST_LOG)) 'Executed binary before checksum verification'
    Assert ((Get-FileHash -LiteralPath $target).Hash -eq $before) 'Checksum failure changed installation'
    [IO.File]::WriteAllText((Join-Path $fixture 'SHA256SUMS'), $validManifest + $validManifest)
    Must-Fail { Install-Funes $destination '1.3.0' $true }
    [IO.File]::WriteAllText((Join-Path $fixture 'SHA256SUMS'), $validManifest)
    $env:FUNES_TEST_VERSION = '9.9.9'
    Must-Fail { Install-Funes $destination '1.3.0' $true }
    Assert ((Get-FileHash -LiteralPath $target).Hash -eq $before) 'Version failure changed installation'
    Remove-Item Env:FUNES_TEST_VERSION
    Must-Fail { Install-Funes $destination '../bad' $true }
    function Get-FunesArchitecture { 'ARM64' }
    Must-Fail { Install-Funes $destination '1.3.0' $true }
    function Get-FunesArchitecture { 'AMD64' }
    Install-Funes '' '1.3.0' $false
    Install-Funes '' '1.3.0' $false
    $defaultDir = Join-Path $env:LOCALAPPDATA 'Programs\funes\bin'
    $entries = [Environment]::GetEnvironmentVariable('Path', 'User') -split ';'
    Assert (@($entries | Where-Object { $_ -eq $defaultDir }).Count -eq 1) 'PATH update duplicated the directory'

    $hooks = Join-Path $root 'hooks'
    $null = New-Item -ItemType Directory $hooks
    Copy-Item (Join-Path $PSScriptRoot 'automation\*.ps1') $hooks
    $env:FUNES_BIN = $fake
    $env:FUNES_TEST_DELAY = '6000'
    Remove-Item -LiteralPath $env:FUNES_TEST_LOG -ErrorAction SilentlyContinue
    $script = Join-Path $hooks 'funes-index.ps1'
    $command = "& '" + $script.Replace("'", "''") + "' 'codex'"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand ' + $encoded
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $process = [Diagnostics.Process]::Start($info)
    $process.StandardInput.WriteLine('{}')
    $process.StandardInput.Close()
    Assert ($process.WaitForExit(5000)) 'Foreground hook blocked on its six-second worker'
    Assert ($process.ExitCode -eq 0) 'Foreground hook failed'
    $process.Dispose()
    $deadline = (Get-Date).AddSeconds(20)
    do {
        Start-Sleep -Milliseconds 200
        $calls = if (Test-Path -LiteralPath $env:FUNES_TEST_LOG) { [IO.File]::ReadAllText($env:FUNES_TEST_LOG) } else { '' }
    } until ($calls.Contains('done:index') -or (Get-Date) -gt $deadline)
    Assert ($calls.Contains("index|--harness|codex`ndone:index")) 'Detached worker lost its arguments or did not finish'
    $script = Join-Path $hooks 'funes-push.ps1'
    $command = "& '" + $script.Replace("'", "''") + "' -Memory 'acme/a''b & (x) %PATH%' -Harness 'codex'"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand ' + $encoded
    $process = [Diagnostics.Process]::Start($info)
    $process.StandardInput.WriteLine('{}')
    $process.StandardInput.Close()
    Assert ($process.WaitForExit(5000)) 'Foreground push hook blocked on indexing'
    Assert ($process.ExitCode -eq 0) 'Foreground push hook failed'
    $process.Dispose()
    $deadline = (Get-Date).AddSeconds(20)
    do {
        Start-Sleep -Milliseconds 200
        $hookLog = [IO.File]::ReadAllText((Join-Path $hooks 'funes-sync.log'))
    } until ($hookLog.Contains('WARN - secrets held back') -or (Get-Date) -gt $deadline)
    $calls = [IO.File]::ReadAllText($env:FUNES_TEST_LOG)
    Assert ($calls.Contains("push|acme/a'b & (x) %PATH%")) 'Push arguments were interpreted as commands'
    Assert ($hookLog.Contains('WARN - secrets held back')) 'Secret-gate exit code was not recorded'
    Write-Host 'Windows installer and hook tests passed.'
} finally {
    $env:LOCALAPPDATA = $savedLocal
    $env:FUNES_BIN = $savedBin
    [Environment]::SetEnvironmentVariable('Path', $savedUserPath, 'User')
    Remove-Item Env:FUNES_TEST_LOG, Env:FUNES_TEST_VERSION, Env:FUNES_TEST_DELAY -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
