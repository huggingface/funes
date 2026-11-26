# Tests of install.ps1, hermetic: every download is a copy of a fixture, every install lands in a
# scratch directory, and the PATH it edits is a scratch registry key's. Run with Windows
# PowerShell, which compiles the stand-in binary:
#
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-install.ps1
$ErrorActionPreference = 'Stop'

function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw "FAILED: $Message" } }

# The message $Action fails with; a failure of the test itself when it does not.
function Get-Failure([scriptblock]$Action) {
    try { $null = & $Action } catch { return "$($_.Exception.Message)" }
    throw 'FAILED: expected the installer to refuse'
}

# A name a shell would split, and one no ANSI code page spells.
$work = Join-Path ([IO.Path]::GetTempPath()) ("funes install $([char]0x6D4B)$([char]0x8BD5) " + [Guid]::NewGuid().ToString('N'))
$scratchKey = 'HKCU:\Software\funes-install-test-' + [Guid]::NewGuid().ToString('N')
$savedPath = $env:Path
# Not `$version`: dot-sourcing install.ps1 binds its own parameters in this scope.
$release = '9.9.9'
$asset = 'funes-x86_64-windows.exe'
try {
    $fixtures = Join-Path $work 'fixtures'
    $null = New-Item -ItemType Directory -Force (Join-Path $fixtures "v$release")

    # The stand-in for a release binary: reports $FUNES_TEST_VERSION, or the release's; leaves
    # $FUNES_TEST_EXECUTED behind when that names a file; and stays up when asked to serve.
    $fake = Join-Path $work 'fake.exe'
    Add-Type -OutputAssembly $fake -OutputType ConsoleApplication -TypeDefinition @"
using System;
using System.IO;
using System.Threading;
public class FunesFixture {
    public static int Main(string[] args) {
        string executed = Environment.GetEnvironmentVariable("FUNES_TEST_EXECUTED");
        if (!String.IsNullOrEmpty(executed)) File.WriteAllText(executed, "ran");
        if (args.Length > 0 && args[0] == "serve") { Thread.Sleep(60000); return 0; }
        Console.WriteLine("funes " + (Environment.GetEnvironmentVariable("FUNES_TEST_VERSION") ?? "$release"));
        return 0;
    }
}
"@

    function Write-Fixtures([string]$Sums) {
        Copy-Item -LiteralPath $fake -Destination (Join-Path $fixtures "v$release\$asset")
        [IO.File]::WriteAllText((Join-Path $fixtures 'VERSION'), "$release`n")
        [IO.File]::WriteAllText((Join-Path $fixtures "v$release\VERSION"), "$release`n")
        [IO.File]::WriteAllText((Join-Path $fixtures "v$release\SHA256SUMS"), $Sums)
    }
    $digest = (Get-FileHash -LiteralPath $fake -Algorithm SHA256).Hash.ToLowerInvariant()
    # The manifest as the release writes it: every asset, the others' too.
    $sums = ('0' * 64) + "  funes-x86_64-linux`n$digest  $asset`n"
    Write-Fixtures $sums

    $FunesInstallDefineOnly = $true
    . (Join-Path $PSScriptRoot 'install.ps1')
    $rev = $FunesInstallRev
    Assert ($rev -ge 1) 'the script names its revision'

    # Each request's path goes to $requests and its User-Agent to $agents, so a run proves which
    # asset was fetched and what every fetch said.
    $requests = New-Object Collections.Generic.List[string]
    $agents = New-Object Collections.Generic.List[string]
    function Get-FunesFile([string]$Uri, [string]$Destination, [string]$UserAgent) {
        $root = 'https://huggingface.co/buckets/huggingface/funes/resolve/'
        Assert ($Uri.StartsWith($root)) "a download from elsewhere than the release bucket: $Uri"
        $requests.Add($Uri.Substring($root.Length))
        $agents.Add($UserAgent)
        Copy-Item -LiteralPath (Join-Path $fixtures $Uri.Substring($root.Length)) -Destination $Destination
    }
    # No test reaches the user's own PATH: the one that edits a PATH names its registry key.
    function Add-FunesToPath([string]$Dir, [string]$Key) { throw 'FAILED: an install reached for the user PATH' }

    # The latest release, into a directory that does not exist yet.
    $install = Join-Path $work 'the bin'
    Install-Funes $install '' $true
    $target = Join-Path $install 'funes.exe'
    Assert ((& $target --version) -ceq "funes $release") 'the latest release was not installed'
    Assert ($requests -contains "v$release/$asset") 'the Windows asset was not the one fetched'
    Assert ($requests[0] -ceq 'VERSION') 'the latest release was not resolved first'
    Assert ((@($agents | Sort-Object -Unique) -join '|') -ceq "funes-install/$rev (windows-x86_64)") 'a download did not say what it is for'
    Assert (@(Get-ChildItem -LiteralPath $install -Force).Count -eq 1) 'the staging directory was left behind'

    # A pinned install names the tag it asked for, and resolves nothing.
    $requests.Clear(); $agents.Clear()
    Install-Funes $install "v$release" $true
    Assert (-not ($requests -contains 'VERSION')) 'a pinned install resolved the latest release'
    Assert ((@($agents | Sort-Object -Unique) -join '|') -ceq "funes-install/$rev (windows-x86_64); requested/v$release") 'a pinned download did not name its tag'
    Assert ((Get-Failure { Install-Funes $install '../9.9.9' $true }) -match 'invalid release version') 'a version that is not one was accepted'

    # A relative install dir is the session's location's, and ~ its home, not the process's.
    $here = Join-Path $work 'here'
    $null = New-Item -ItemType Directory -Force $here
    $home0 = (Get-PSProvider FileSystem).Home
    Push-Location -LiteralPath $here
    try {
        (Get-PSProvider FileSystem).Home = $here
        Install-Funes 'rel bin' "v$release" $true
        Assert (Test-Path -LiteralPath (Join-Path $here 'rel bin\funes.exe')) 'a relative install dir was not the location''s'
        Install-Funes '~\tilde bin' "v$release" $true
        Assert (Test-Path -LiteralPath (Join-Path $here 'tilde bin\funes.exe')) 'a ~ install dir was not the home'
        Assert ((Get-Failure { Install-Funes 'HKCU:\Software' "v$release" $true }) -match 'not a folder') 'a registry key was taken for an install dir'
    } finally {
        (Get-PSProvider FileSystem).Home = $home0
        Pop-Location
    }

    # A processor with no prebuilt asset stops before it touches the install dir.
    $unsupported = Join-Path $work 'unsupported'
    function Get-FunesArchitecture { 'ARM64' }
    Assert ((Get-Failure { Install-Funes $unsupported '' $true }) -match 'no prebuilt binary for Windows/ARM64') 'an unsupported processor was offered a binary'
    Assert (-not (Test-Path -LiteralPath $unsupported)) 'an unsupported processor left an install dir'
    function Get-FunesArchitecture { 'AMD64' }

    # A binary the manifest does not vouch for is never run, and replaces nothing.
    $before = (Get-FileHash -LiteralPath $target).Hash
    $env:FUNES_TEST_EXECUTED = Join-Path $work 'executed'
    Write-Fixtures (('0' * 64) + "  $asset`n")
    Assert ((Get-Failure { Install-Funes $install '' $true }) -match 'checksum verification failed') 'a binary with the wrong checksum was accepted'
    Assert (-not (Test-Path -LiteralPath $env:FUNES_TEST_EXECUTED)) 'a binary was run before its checksum was verified'
    Assert ((Get-FileHash -LiteralPath $target).Hash -eq $before) 'a refused binary replaced the installed one'
    Remove-Item Env:FUNES_TEST_EXECUTED

    # A manifest that is not one vouches for nothing.
    foreach ($bad in @("$digest  $asset`n$digest  $asset`n", ($digest.ToUpperInvariant() + "  $asset`n"), "$digest`n", "$digest  dir/$asset`n", ('0' * 64) + "  funes-x86_64-linux`n")) {
        Write-Fixtures $bad
        Assert ((Get-Failure { Install-Funes $install '' $true }) -match 'SHA256SUMS is malformed or does not contain') "a bad manifest was accepted: $bad"
    }
    Write-Fixtures $sums

    # Nor is a binary that reports another version than its release, or a release whose own
    # marker names another.
    $env:FUNES_TEST_VERSION = '9.9.8'
    Assert ((Get-Failure { Install-Funes $install '' $true }) -match 'does not report the expected version') 'a binary of another version was accepted'
    Remove-Item Env:FUNES_TEST_VERSION
    [IO.File]::WriteAllText((Join-Path $fixtures "v$release\VERSION"), "9.9.8`n")
    Assert ((Get-Failure { Install-Funes $install '' $true }) -match 'release metadata does not match') 'a release marked as another was accepted'
    Write-Fixtures $sums
    Assert ((Get-FileHash -LiteralPath $target).Hash -eq $before) 'a refused release replaced the installed one'

    # funes is replaced while it runs, as it does under an agent's MCP server: the running binary
    # steps aside, and the install after it has exited deletes it.
    $running = @(Start-Process -FilePath $target -ArgumentList 'serve' -PassThru -WindowStyle Hidden)
    try {
        Start-Sleep -Milliseconds 300
        Install-Funes $install '' $true
        Assert ((& $target --version) -ceq "funes $release") 'funes was not replaced while it ran'
        Assert (@(Get-ChildItem -LiteralPath $install -Filter 'funes.exe.*.old').Count -eq 1) 'the running binary did not step aside'
        # Again from the same window while both run: the second steps aside beside the first.
        $running += Start-Process -FilePath $target -ArgumentList 'serve' -PassThru -WindowStyle Hidden
        Start-Sleep -Milliseconds 300
        Install-Funes $install '' $true
        Assert (@(Get-ChildItem -LiteralPath $install -Filter 'funes.exe.*.old').Count -eq 2) 'a second running binary did not step aside beside the first'
    } finally {
        foreach ($process in $running) {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            $process.WaitForExit()
        }
    }
    Install-Funes $install '' $true
    Assert (@(Get-ChildItem -LiteralPath $install -Force).Count -eq 1) 'the binary that stepped aside was not deleted once it exited'

    # A directory already on PATH is left alone; one that is not is added, unless told otherwise.
    # (Add-FunesToPath, replaced above, fails the test if an install calls it unasked.)
    $env:Path = "$savedPath;$install\"
    Install-Funes $install '' $false
    Assert ($env:Path -ceq "$savedPath;$install\") 'a directory on PATH was added again'
    $added = New-Object Collections.Generic.List[string]
    function Add-FunesToPath([string]$Dir, [string]$Key) { $added.Add($Dir); return $false }
    $env:Path = $savedPath
    Install-Funes $install '' $false
    Assert (($added -join '|') -ceq $install) 'a directory off PATH was not added to the user PATH'
    Assert ($env:Path -ceq "$savedPath;$install") 'this session did not get the directory'
    $env:Path = $savedPath

    # The PATH edit itself, on a registry key of the test's own: appended once, unexpanded, and
    # as the kind of value it was.
    . (Join-Path $PSScriptRoot 'install.ps1')
    $null = New-Item -Path $scratchKey
    $env:FUNES_TEST_DIR = $install
    $raw = '%USERPROFILE%\bin;C:\Tools\'
    New-ItemProperty -LiteralPath $scratchKey -Name Path -Value $raw -PropertyType ExpandString | Out-Null
    $read = { (Get-Item -LiteralPath $scratchKey).GetValue('Path', '', 'DoNotExpandEnvironmentNames') }
    Assert (Add-FunesToPath $install $scratchKey) 'a new directory was not appended'
    Assert ((& $read) -ceq "$raw;$install") 'the PATH was not appended to as it was written'
    Assert ((Get-Item -LiteralPath $scratchKey).GetValueKind('Path') -eq 'ExpandString') 'an expandable PATH stopped being one'
    Assert (-not (Add-FunesToPath "$install\" $scratchKey)) 'a directory was appended twice'
    Assert (-not (Add-FunesToPath 'c:\tools' $scratchKey)) 'an entry was not recognized under another spelling'
    Set-ItemProperty -LiteralPath $scratchKey -Name Path -Value 'C:\Tools;%FUNES_TEST_DIR%;' -Type String
    Assert (-not (Add-FunesToPath $install $scratchKey)) 'an entry was not recognized through its variable'
    Assert (Add-FunesToPath 'C:\Elsewhere' $scratchKey) 'a new directory was not appended'
    Assert ((& $read) -ceq 'C:\Tools;%FUNES_TEST_DIR%;C:\Elsewhere') 'a PATH ending in a separator was given another'
    Assert ((Get-Item -LiteralPath $scratchKey).GetValueKind('Path') -eq 'String') 'a plain PATH changed kind'
    Remove-ItemProperty -LiteralPath $scratchKey -Name Path
    Assert (Add-FunesToPath $install $scratchKey) 'a PATH was not created where there was none'
    Assert ((& $read) -ceq $install) 'a created PATH holds more than the directory'

    # Piped to iex from a dot-sourced script, it installs. The alias outranks the real function.
    Remove-Variable FunesInstallDefineOnly
    $installed = $false
    function Register-Install { $script:installed = $true }
    Set-Alias Install-Funes Register-Install
    . { Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'install.ps1') | Invoke-Expression }
    Remove-Item Alias:Install-Funes
    Assert $installed 'piped to iex from a dot-sourced script, nothing was installed'

    Write-Host 'install.ps1: all tests passed.'
} finally {
    $env:Path = $savedPath
    Remove-Item Env:FUNES_TEST_EXECUTED, Env:FUNES_TEST_VERSION, Env:FUNES_TEST_DIR -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $scratchKey -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
