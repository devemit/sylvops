[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$InstallerPath,
    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9A-Fa-f]{40}$')]
    [string]$CertificateThumbprint,
    [switch]$RequireTrustedSignature,
    [switch]$RequireTimestamp
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-InstallerCondition {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-SignedByExpectedCertificate {
    param([string]$Path)
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    Assert-InstallerCondition ($null -ne $signature.SignerCertificate) "No Authenticode signer was found for $Path."
    Assert-InstallerCondition ($signature.SignerCertificate.Thumbprint -eq $CertificateThumbprint) "Unexpected Authenticode signer for $Path."
    Assert-InstallerCondition ($signature.Status -notin @('HashMismatch', 'NotSigned')) "Invalid Authenticode signature for ${Path}: $($signature.Status)"
    Assert-InstallerCondition (-not $RequireTrustedSignature -or $signature.Status -eq 'Valid') "The Authenticode signature for $Path is not trusted: $($signature.Status)."
    Assert-InstallerCondition (-not $RequireTimestamp -or $null -ne $signature.TimeStamperCertificate) "The Authenticode signature for $Path is missing its required RFC 3161 timestamp."
}

function Wait-ForDesktopProcess {
    param([string]$ExecutablePath)
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        $process = Get-CimInstance Win32_Process -OperationTimeoutSec 2 | Where-Object {
            $_.ExecutablePath -eq $ExecutablePath -and $_.CommandLine -match '(?i)(^|\s)desktop(\s|$)'
        } | Select-Object -First 1
        if ($null -ne $process) {
            return $process
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'The installed desktop process did not start within 20 seconds.'
}

function Invoke-BoundedProcess {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [int]$TimeoutSeconds,
        [string]$Operation,
        [switch]$CaptureOutput
    )
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $CaptureOutput
    $startInfo.RedirectStandardError = $CaptureOutput
    foreach ($argument in $ArgumentList) {
        $startInfo.ArgumentList.Add($argument)
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "$Operation did not start."
        }
        $standardOutput = if ($CaptureOutput) { $process.StandardOutput.ReadToEndAsync() } else { $null }
        $standardError = if ($CaptureOutput) { $process.StandardError.ReadToEndAsync() } else { $null }
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            try {
                $process.Kill($true)
            }
            catch {
                Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            }
            throw "$Operation did not finish within $TimeoutSeconds seconds."
        }
        $process.WaitForExit()
        if ($CaptureOutput -and (-not $standardOutput.Wait(5000) -or -not $standardError.Wait(5000))) {
            throw "$Operation output did not close within 5 seconds."
        }
        return [PSCustomObject]@{
            ExitCode = $process.ExitCode
            StdOut = if ($CaptureOutput) { $standardOutput.GetAwaiter().GetResult() } else { '' }
            StdErr = if ($CaptureOutput) { $standardError.GetAwaiter().GetResult() } else { '' }
        }
    }
    finally {
        $process.Dispose()
    }
}

function Wait-ForProcessExit {
    param(
        [System.Diagnostics.Process]$Process,
        [int]$TimeoutSeconds,
        [string]$Operation
    )
    if (-not $Process.WaitForExit($TimeoutSeconds * 1000)) {
        Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
        throw "$Operation did not finish within $TimeoutSeconds seconds."
    }
    $Process.Refresh()
    return $Process.ExitCode
}

function Wait-ForCondition {
    param(
        [scriptblock]$Condition,
        [int]$TimeoutSeconds,
        [string]$FailureMessage
    )
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        if (& $Condition) {
            return
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw $FailureMessage
}

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$cargo = Get-Content -Raw -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml')
$versionMatch = [regex]::Match($cargo, '(?ms)^\[workspace\.package\]\s*.*?^version\s*=\s*"([^"]+)"')
Assert-InstallerCondition $versionMatch.Success 'Could not read the expected SylvOps version.'
$expectedVersion = $versionMatch.Groups[1].Value
$resolvedInstaller = (Resolve-Path -LiteralPath $InstallerPath).Path
Assert-SignedByExpectedCertificate $resolvedInstaller

$testId = [Guid]::NewGuid().ToString('N')
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "sylvops-installer-$testId"
$stateRoot = Join-Path $testRoot 'state'
$repositoryDirectory = Join-Path $testRoot 'repository'
$worktreeDirectory = Join-Path $testRoot 'worktree'
$localDataDirectory = Join-Path $env:LOCALAPPDATA 'SylvOps'
$roamingConfigDirectory = Join-Path $env:APPDATA 'SylvOps'
$localDataExisted = Test-Path -LiteralPath $localDataDirectory
$roamingConfigExisted = Test-Path -LiteralPath $roamingConfigDirectory
$localSentinel = Join-Path $localDataDirectory "installer-preserve-$testId.txt"
$roamingSentinel = Join-Path $roamingConfigDirectory "installer-preserve-$testId.txt"
$expectedInstallLocation = Join-Path $env:LOCALAPPDATA 'Programs\SylvOps'
$installedExecutable = Join-Path $expectedInstallLocation 'sylvops.exe'
$uninstallerPath = Join-Path $expectedInstallLocation 'uninstall.exe'
$installed = $false
$desktopProcess = $null

try {
    New-Item -ItemType Directory -Path $repositoryDirectory, $localDataDirectory, $roamingConfigDirectory -Force | Out-Null
    Set-Content -LiteralPath $localSentinel -Value 'preserve SylvOps local data'
    Set-Content -LiteralPath $roamingSentinel -Value 'preserve SylvOps roaming configuration'

    $gitInit = Invoke-BoundedProcess -FilePath 'git' -ArgumentList @('-C', $repositoryDirectory, 'init', '--quiet') -TimeoutSeconds 30 -Operation 'Git repository initialization'
    Assert-InstallerCondition ($gitInit.ExitCode -eq 0) 'Could not initialize the installer smoke-test repository.'
    Set-Content -LiteralPath (Join-Path $repositoryDirectory 'README.md') -Value '# installer smoke test'
    $gitAdd = Invoke-BoundedProcess -FilePath 'git' -ArgumentList @('-C', $repositoryDirectory, 'add', 'README.md') -TimeoutSeconds 30 -Operation 'Git staging'
    Assert-InstallerCondition ($gitAdd.ExitCode -eq 0) 'Could not stage the smoke-test repository content.'
    $gitCommit = Invoke-BoundedProcess -FilePath 'git' -ArgumentList @('-C', $repositoryDirectory, '-c', 'user.name=SylvOps CI', '-c', 'user.email=sylvops-ci@example.invalid', 'commit', '--quiet', '-m', 'test: seed installer smoke repository') -TimeoutSeconds 30 -Operation 'Git fixture commit'
    Assert-InstallerCondition ($gitCommit.ExitCode -eq 0) 'Could not commit the smoke-test repository content.'
    $gitWorktree = Invoke-BoundedProcess -FilePath 'git' -ArgumentList @('-C', $repositoryDirectory, 'worktree', 'add', '--quiet', '-b', 'installer-smoke-branch', $worktreeDirectory) -TimeoutSeconds 30 -Operation 'Git worktree creation'
    Assert-InstallerCondition ($gitWorktree.ExitCode -eq 0) 'Could not create the smoke-test worktree and branch.'

    $installed = $true
    $install = Start-Process -FilePath $resolvedInstaller -ArgumentList '/S' -PassThru
    $installExitCode = Wait-ForProcessExit -Process $install -TimeoutSeconds 120 -Operation 'Installer'
    Assert-InstallerCondition ($installExitCode -eq 0) "Installer exited with code $installExitCode."

    $uninstallKey = Get-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\SylvOps'
    Assert-InstallerCondition ($uninstallKey.DisplayName -eq 'SylvOps') 'The registered uninstall display name is incorrect.'
    Assert-InstallerCondition ($uninstallKey.Publisher -eq 'devemit') 'The registered uninstall publisher is incorrect.'
    Assert-InstallerCondition ($uninstallKey.DisplayVersion -eq $expectedVersion) 'The registered uninstall version is incorrect.'
    $installLocation = $uninstallKey.InstallLocation.Trim('"')
    Assert-InstallerCondition ($installLocation -eq $expectedInstallLocation) 'The installer did not use the locked per-user application directory.'

    Assert-InstallerCondition (Test-Path -LiteralPath $installedExecutable -PathType Leaf) 'The installed SylvOps executable is missing.'
    Assert-InstallerCondition (Test-Path -LiteralPath $uninstallerPath -PathType Leaf) 'The registered uninstaller is missing.'
    Assert-SignedByExpectedCertificate $installedExecutable
    Assert-SignedByExpectedCertificate $uninstallerPath

    $appPathsKey = Get-Item -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\App Paths\sylvops.exe'
    Assert-InstallerCondition ($appPathsKey.GetValue('') -eq $installedExecutable) 'The current-user CLI App Paths registration is incorrect.'
    $startMenuShortcut = Get-ChildItem -LiteralPath (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs') -Filter 'SylvOps.lnk' -File -Recurse | Select-Object -First 1
    Assert-InstallerCondition ($null -ne $startMenuShortcut) 'The SylvOps Start Menu shortcut is missing.'

    $version = Invoke-BoundedProcess -FilePath $installedExecutable -ArgumentList @('--version') -TimeoutSeconds 30 -Operation 'Installed CLI version check' -CaptureOutput
    Assert-InstallerCondition ($version.ExitCode -eq 0) 'The installed CLI version check failed.'
    $versionOutput = $version.StdOut.Trim()
    Assert-InstallerCondition ($versionOutput -eq "sylvops $expectedVersion") "Installed CLI version is '$versionOutput'."

    $launch = Invoke-BoundedProcess -FilePath $installedExecutable -ArgumentList @('--state-dir', $stateRoot, 'up', $repositoryDirectory) -TimeoutSeconds 30 -Operation 'Installed desktop launch'
    Assert-InstallerCondition ($launch.ExitCode -eq 0) 'The installed launch surface failed.'
    $desktopProcess = Wait-ForDesktopProcess -ExecutablePath $installedExecutable
    $status = Invoke-BoundedProcess -FilePath $installedExecutable -ArgumentList @('--state-dir', $stateRoot, 'daemon', 'status') -TimeoutSeconds 30 -Operation 'Installed daemon status check' -CaptureOutput
    $statusOutput = $status.StdOut + $status.StdErr
    Assert-InstallerCondition ($status.ExitCode -eq 0 -and $statusOutput.Contains('SylvOps daemon is healthy')) 'The installed daemon did not become healthy.'

    $stop = Invoke-BoundedProcess -FilePath $installedExecutable -ArgumentList @('--state-dir', $stateRoot, 'daemon', 'stop') -TimeoutSeconds 30 -Operation 'Installed daemon stop'
    Assert-InstallerCondition ($stop.ExitCode -eq 0) 'The installed daemon did not stop cleanly.'
    Stop-Process -Id $desktopProcess.ProcessId -Force -ErrorAction SilentlyContinue
    $desktopProcess = $null

    $uninstall = Start-Process -FilePath $uninstallerPath -ArgumentList '/S' -PassThru
    $uninstallExitCode = Wait-ForProcessExit -Process $uninstall -TimeoutSeconds 120 -Operation 'Uninstaller'
    Assert-InstallerCondition ($uninstallExitCode -eq 0) "Uninstaller exited with code $uninstallExitCode."
    Wait-ForCondition -TimeoutSeconds 30 -FailureMessage 'Uninstaller cleanup did not finish within 30 seconds.' -Condition {
        -not (Test-Path -LiteralPath $expectedInstallLocation) -and
            -not (Test-Path -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\SylvOps')
    }
    $installed = $false

    Assert-InstallerCondition (-not (Test-Path -LiteralPath $installedExecutable)) 'Uninstall left the application executable behind.'
    Assert-InstallerCondition (-not (Test-Path -LiteralPath $expectedInstallLocation)) 'Uninstall left the application directory behind.'
    Assert-InstallerCondition (-not (Test-Path -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\SylvOps')) 'Uninstall left its registration behind.'
    Assert-InstallerCondition (-not (Test-Path -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\App Paths\sylvops.exe')) 'Uninstall left its CLI registration behind.'
    Assert-InstallerCondition (-not (Test-Path -LiteralPath 'HKCU:\Software\devemit\SylvOps')) 'Uninstall left installer-owned product state behind.'
    Assert-InstallerCondition (-not (Test-Path -LiteralPath $startMenuShortcut.FullName)) 'Uninstall left its Start Menu shortcut behind.'
    Assert-InstallerCondition (Test-Path -LiteralPath $localSentinel -PathType Leaf) 'Uninstall removed SylvOps local user data.'
    Assert-InstallerCondition (Test-Path -LiteralPath $roamingSentinel -PathType Leaf) 'Uninstall removed SylvOps roaming configuration.'
    Assert-InstallerCondition (Test-Path -LiteralPath (Join-Path $repositoryDirectory '.git')) 'Uninstall removed the user repository.'
    Assert-InstallerCondition (Test-Path -LiteralPath $worktreeDirectory -PathType Container) 'Uninstall removed the user worktree.'
    $gitShowRef = Invoke-BoundedProcess -FilePath 'git' -ArgumentList @('-C', $repositoryDirectory, 'show-ref', '--verify', '--quiet', 'refs/heads/installer-smoke-branch') -TimeoutSeconds 30 -Operation 'Git branch preservation check'
    Assert-InstallerCondition ($gitShowRef.ExitCode -eq 0) 'Uninstall removed the user branch.'

    Write-Host '[ok] signed per-user installer installed, launched, verified, and uninstalled without removing user content'
}
finally {
    if ($null -ne $desktopProcess) {
        Stop-Process -Id $desktopProcess.ProcessId -Force -ErrorAction SilentlyContinue
    }
    if ($installed -and (Test-Path -LiteralPath $uninstallerPath -PathType Leaf)) {
        try {
            $cleanup = Start-Process -FilePath $uninstallerPath -ArgumentList '/S' -PassThru
            $cleanupExitCode = Wait-ForProcessExit -Process $cleanup -TimeoutSeconds 120 -Operation 'Cleanup uninstaller'
            if ($cleanupExitCode -ne 0) {
                Write-Warning "Cleanup uninstaller exited with code $cleanupExitCode."
            }
            Wait-ForCondition -TimeoutSeconds 30 -FailureMessage 'Cleanup uninstaller did not finish within 30 seconds.' -Condition {
                -not (Test-Path -LiteralPath $expectedInstallLocation)
            }
        }
        catch {
            Write-Warning "Installer cleanup failed: $($_.Exception.Message)"
        }
    }
    Remove-Item -LiteralPath $localSentinel -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $roamingSentinel -Force -ErrorAction SilentlyContinue
    if (-not $localDataExisted -and (Test-Path -LiteralPath $localDataDirectory)) {
        Remove-Item -LiteralPath $localDataDirectory -ErrorAction SilentlyContinue
    }
    if (-not $roamingConfigExisted -and (Test-Path -LiteralPath $roamingConfigDirectory)) {
        Remove-Item -LiteralPath $roamingConfigDirectory -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $testRoot) {
        $resolvedTestRoot = (Resolve-Path -LiteralPath $testRoot).Path
        $resolvedTempRoot = (Resolve-Path -LiteralPath ([System.IO.Path]::GetTempPath())).Path
        if ($resolvedTestRoot.StartsWith($resolvedTempRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
        }
    }
}
