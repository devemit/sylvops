[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$CandidateInstallerPath,
    [Parameter(Mandatory)]
    [string]$CandidateExecutablePath,
    [Parameter(Mandatory)]
    [string]$PreviousInstallerPath,
    [Parameter(Mandatory)]
    [string]$SuccessManifestPath,
    [Parameter(Mandatory)]
    [string]$FailedHealthManifestPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-UpgradeCondition {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) {
        throw $Message
    }
}

function Wait-ForProcessExit {
    param(
        [System.Diagnostics.Process]$Process,
        [int]$TimeoutSeconds,
        [string]$Operation
    )
    if (-not $Process.WaitForExit($TimeoutSeconds * 1000)) {
        try {
            $Process.Kill($true)
        }
        catch {
            Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
        }
        throw "$Operation did not finish within $TimeoutSeconds seconds."
    }
    $Process.Refresh()
    return $Process.ExitCode
}

function Invoke-BoundedProcess {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [int]$TimeoutSeconds,
        [string]$Operation
    )
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $false
    $startInfo.RedirectStandardError = $false
    foreach ($argument in $ArgumentList) {
        $startInfo.ArgumentList.Add($argument)
    }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        Assert-UpgradeCondition $process.Start() "$Operation did not start."
        $exitCode = Wait-ForProcessExit -Process $process -TimeoutSeconds $TimeoutSeconds -Operation $Operation
        return [PSCustomObject]@{
            ExitCode = $exitCode
        }
    }
    finally {
        $process.Dispose()
    }
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
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    throw $FailureMessage
}

function Get-InstalledVersion {
    $productVersion = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($script:installedExecutable).ProductVersion
    $match = [regex]::Match($productVersion, '^([0-9]+\.[0-9]+\.[0-9]+)')
    Assert-UpgradeCondition $match.Success "The installed executable version metadata is invalid: '$productVersion'."
    return "sylvops $($match.Groups[1].Value)"
}

function Stop-SylvOps {
    param([string]$StateRoot)
    if (Test-Path -LiteralPath $script:installedExecutable -PathType Leaf) {
        try {
            $null = Invoke-BoundedProcess -FilePath $script:installedExecutable -ArgumentList @('--state-dir', $StateRoot, 'daemon', 'stop') -TimeoutSeconds 20 -Operation 'Daemon stop'
        }
        catch {
            Write-Verbose "Daemon stop was unnecessary: $($_.Exception.Message)"
        }
    }
    Get-CimInstance Win32_Process -OperationTimeoutSec 2 | Where-Object {
        $_.ExecutablePath -eq $script:installedExecutable -and $_.CommandLine -match '(?i)(^|\s)desktop(\s|$)'
    } | ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }
}

function Install-Package {
    param([string]$Path, [string]$Operation)
    $process = Start-Process -FilePath $Path -ArgumentList '/S' -PassThru
    $exitCode = Wait-ForProcessExit -Process $process -TimeoutSeconds 120 -Operation $Operation
    Assert-UpgradeCondition ($exitCode -eq 0) "$Operation exited with code $exitCode."
}

function Initialize-StateRoot {
    param([string]$StateRoot)
    $start = Invoke-BoundedProcess -FilePath $script:installedExecutable -ArgumentList @('--state-dir', $StateRoot, 'daemon', 'start') -TimeoutSeconds 30 -Operation 'Previous daemon start'
    Assert-UpgradeCondition ($start.ExitCode -eq 0) 'The previous daemon did not start.'
    $stop = Invoke-BoundedProcess -FilePath $script:installedExecutable -ArgumentList @('--state-dir', $StateRoot, 'daemon', 'stop') -TimeoutSeconds 30 -Operation 'Previous daemon stop'
    Assert-UpgradeCondition ($stop.ExitCode -eq 0) 'The previous daemon did not stop.'
}

function New-UpgradeHandoff {
    param(
        [string]$StateRoot,
        [string]$ManifestPath,
        [bool]$RelaunchDesktop,
        [switch]$PreserveAttempt
    )
    $dataDirectory = Join-Path $StateRoot 'data'
    $stagingRoot = Join-Path $dataDirectory 'upgrades'
    New-Item -ItemType Directory -Path $stagingRoot -Force | Out-Null
    if (-not $PreserveAttempt) {
        Remove-Item -LiteralPath (Join-Path $stagingRoot 'native-upgrade-attempt.json') -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath (Join-Path $stagingRoot 'rollback') -Recurse -Force -ErrorAction SilentlyContinue
    }
    Copy-Item -LiteralPath $script:candidateInstaller -Destination (Join-Path $stagingRoot 'payload.staged') -Force
    Copy-Item -LiteralPath $ManifestPath -Destination (Join-Path $stagingRoot 'release.json') -Force
    $helperPath = Join-Path $stagingRoot 'sylvops-upgrade-helper.exe'
    Copy-Item -LiteralPath $script:installedExecutable -Destination $helperPath -Force
    $envelope = Get-Content -Raw -LiteralPath $ManifestPath | ConvertFrom-Json
    $handoff = [ordered]@{
        release = $envelope.release
        staging_root = $stagingRoot
        installed_executable = $script:installedExecutable
        data_directory = $dataDirectory
        config_directory = (Join-Path $StateRoot 'config')
        runtime_directory = (Join-Path $StateRoot 'run')
        client_process_ids = @(2147483647)
        relaunch_desktop = $RelaunchDesktop
    }
    $handoffPath = Join-Path $stagingRoot 'handoff.json'
    $handoff | ConvertTo-Json -Depth 12 -Compress | Set-Content -LiteralPath $handoffPath -Encoding utf8NoBOM
    return [PSCustomObject]@{
        HelperPath = $helperPath
        HandoffPath = $handoffPath
        AttemptPath = (Join-Path $stagingRoot 'native-upgrade-attempt.json')
        TargetVersion = [string]$envelope.release.target_version
    }
}

function Start-UpgradeHelper {
    param($Handoff)
    return Start-Process -FilePath $Handoff.HelperPath -ArgumentList @('update-helper', '--handoff', $Handoff.HandoffPath) -PassThru
}

function Restore-PreviousPackage {
    param([string]$StateRoot)
    Stop-SylvOps -StateRoot $StateRoot
    Install-Package -Path $script:previousInstaller -Operation 'Previous package restore'
    Assert-UpgradeCondition ((Get-InstalledVersion) -eq $script:previousVersion) 'The previous package was not restored.'
}

$script:candidateInstaller = (Resolve-Path -LiteralPath $CandidateInstallerPath).Path
$script:candidateExecutable = (Resolve-Path -LiteralPath $CandidateExecutablePath).Path
$script:previousInstaller = (Resolve-Path -LiteralPath $PreviousInstallerPath).Path
$successManifest = (Resolve-Path -LiteralPath $SuccessManifestPath).Path
$failedHealthManifest = (Resolve-Path -LiteralPath $FailedHealthManifestPath).Path
$script:installRoot = Join-Path $env:LOCALAPPDATA 'Programs\SylvOps'
$script:installedExecutable = Join-Path $script:installRoot 'sylvops.exe'
$uninstaller = Join-Path $script:installRoot 'uninstall.exe'
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "sylvops-native-upgrade-$([Guid]::NewGuid().ToString('N'))"
$candidateHash = (Get-FileHash -LiteralPath $script:candidateExecutable -Algorithm SHA256).Hash
$installed = $false

try {
    New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
    Install-Package -Path $script:previousInstaller -Operation 'Previous package install'
    $installed = $true
    $script:previousVersion = Get-InstalledVersion

    $successState = Join-Path $testRoot 'success-state'
    Initialize-StateRoot -StateRoot $successState
    $success = New-UpgradeHandoff -StateRoot $successState -ManifestPath $successManifest -RelaunchDesktop $true
    $successProcess = Start-UpgradeHelper -Handoff $success
    $successExitCode = Wait-ForProcessExit -Process $successProcess -TimeoutSeconds 180 -Operation 'Successful native upgrade helper'
    Assert-UpgradeCondition ($successExitCode -eq 0) "Successful native upgrade helper exited with code $successExitCode."
    Assert-UpgradeCondition ((Get-InstalledVersion) -eq "sylvops $($success.TargetVersion)") 'The helper did not install the candidate version.'
    Wait-ForCondition -TimeoutSeconds 30 -FailureMessage 'The updated desktop did not relaunch from the installed surface.' -Condition {
        $null -ne (Get-CimInstance Win32_Process -OperationTimeoutSec 2 | Where-Object {
            $_.ExecutablePath -eq $script:installedExecutable -and $_.CommandLine -match '(?i)(^|\s)desktop(\s|$)'
        } | Select-Object -First 1)
    }
    $status = Invoke-BoundedProcess -FilePath $script:installedExecutable -ArgumentList @('--state-dir', $successState, 'daemon', 'status') -TimeoutSeconds 30 -Operation 'Updated daemon health check'
    Assert-UpgradeCondition ($status.ExitCode -eq 0) 'The updated daemon did not pass its health check.'
    Assert-UpgradeCondition (-not (Test-Path -LiteralPath (Join-Path $successState 'data\upgrades\rollback'))) 'N-1 was retained after the candidate passed health checks.'
    Restore-PreviousPackage -StateRoot $successState

    $lockedState = Join-Path $testRoot 'locked-state'
    Initialize-StateRoot -StateRoot $lockedState
    $locked = New-UpgradeHandoff -StateRoot $lockedState -ManifestPath $successManifest -RelaunchDesktop $false
    $lockedStream = [System.IO.File]::Open($script:installedExecutable, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::None)
    try {
        $lockedProcess = Start-UpgradeHelper -Handoff $locked
        $lockedExitCode = Wait-ForProcessExit -Process $lockedProcess -TimeoutSeconds 90 -Operation 'Locked executable upgrade helper'
        Assert-UpgradeCondition ($lockedExitCode -ne 0) 'The helper replaced an exclusively locked executable.'
    }
    finally {
        $lockedStream.Dispose()
    }
    Assert-UpgradeCondition ((Get-InstalledVersion) -eq $script:previousVersion) 'The locked-executable failure changed the installed version.'
    Stop-SylvOps -StateRoot $lockedState

    $interruptedState = Join-Path $testRoot 'interrupted-state'
    Initialize-StateRoot -StateRoot $interruptedState
    $interrupted = New-UpgradeHandoff -StateRoot $interruptedState -ManifestPath $successManifest -RelaunchDesktop $false
    $interruptedProcess = Start-UpgradeHelper -Handoff $interrupted
    Wait-ForCondition -TimeoutSeconds 90 -FailureMessage 'The interrupted helper never began applying the candidate.' -Condition {
        if (-not (Test-Path -LiteralPath $interrupted.AttemptPath -PathType Leaf)) { return $false }
        $attempt = Get-Content -Raw -LiteralPath $interrupted.AttemptPath | ConvertFrom-Json
        return $attempt.phase -eq 'applying'
    }
    Wait-ForCondition -TimeoutSeconds 90 -FailureMessage 'The candidate executable was not installed before helper interruption.' -Condition {
        try {
            (Get-FileHash -LiteralPath $script:installedExecutable -Algorithm SHA256).Hash -eq $candidateHash
        }
        catch {
            $false
        }
    }
    Assert-UpgradeCondition (-not $interruptedProcess.HasExited) 'The helper exited before the interruption probe.'
    Stop-Process -Id $interruptedProcess.Id -Force
    $null = Wait-ForProcessExit -Process $interruptedProcess -TimeoutSeconds 20 -Operation 'Interrupted upgrade helper termination'
    Wait-ForCondition -TimeoutSeconds 90 -FailureMessage 'The watchdog did not restore N-1 after helper interruption.' -Condition {
        try { (Get-InstalledVersion) -eq $script:previousVersion } catch { $false }
    }
    $interruptedAttempt = Get-Content -Raw -LiteralPath $interrupted.AttemptPath | ConvertFrom-Json
    Assert-UpgradeCondition ($interruptedAttempt.phase -eq 'rolled_back' -and $interruptedAttempt.rollback_attempts -eq 1 -and $interruptedAttempt.diagnostic -eq 'helper_interrupted') 'The interrupted helper did not record one redacted rollback.'
    Stop-SylvOps -StateRoot $interruptedState

    $failedState = Join-Path $testRoot 'failed-health-state'
    Initialize-StateRoot -StateRoot $failedState
    $rollbackBallast = Join-Path $script:installRoot 'upgrade-test-ballast'
    New-Item -ItemType Directory -Path $rollbackBallast -Force | Out-Null
    foreach ($index in 1..2048) {
        [IO.File]::WriteAllText((Join-Path $rollbackBallast "$index.txt"), 'bounded rollback interruption probe')
    }
    $failed = New-UpgradeHandoff -StateRoot $failedState -ManifestPath $failedHealthManifest -RelaunchDesktop $false
    $failedProcess = Start-UpgradeHelper -Handoff $failed
    Wait-ForCondition -TimeoutSeconds 180 -FailureMessage 'The failed-health helper did not begin its rollback.' -Condition {
        if (-not (Test-Path -LiteralPath $failed.AttemptPath -PathType Leaf)) { return $false }
        $attempt = Get-Content -Raw -LiteralPath $failed.AttemptPath | ConvertFrom-Json
        return $attempt.phase -eq 'rollback_started'
    }
    $candidateRegistry = Join-Path (Split-Path -Parent $failed.HandoffPath) 'candidate-registry'
    Wait-ForCondition -TimeoutSeconds 60 -FailureMessage 'The rollback did not enter its post-mutation registry snapshot phase.' -Condition {
        (Get-ChildItem -LiteralPath $candidateRegistry -Filter '*.reg' -File -ErrorAction SilentlyContinue).Count -eq 3
    }
    Assert-UpgradeCondition (-not $failedProcess.HasExited) 'The failed-health helper completed rollback before the interruption probe.'
    Stop-Process -Id $failedProcess.Id -Force
    $null = Wait-ForProcessExit -Process $failedProcess -TimeoutSeconds 20 -Operation 'Rollback helper interruption'
    Wait-ForCondition -TimeoutSeconds 180 -FailureMessage 'The watchdog did not resume the interrupted rollback.' -Condition {
        try {
            $attempt = Get-Content -Raw -LiteralPath $failed.AttemptPath | ConvertFrom-Json
            $attempt.phase -eq 'rolled_back' -and (Get-InstalledVersion) -eq $script:previousVersion
        }
        catch {
            $false
        }
    }
    $failedAttempt = Get-Content -Raw -LiteralPath $failed.AttemptPath | ConvertFrom-Json
    Assert-UpgradeCondition ($failedAttempt.phase -eq 'rolled_back' -and $failedAttempt.rollback_attempts -eq 1 -and $failedAttempt.diagnostic -eq 'health_check_failed') 'The failed health check did not preserve one redacted rollback diagnostic.'
    Remove-Item -LiteralPath $rollbackBallast -Recurse -Force -ErrorAction SilentlyContinue

    $repeated = New-UpgradeHandoff -StateRoot $failedState -ManifestPath $failedHealthManifest -RelaunchDesktop $false -PreserveAttempt
    $repeatedResult = Invoke-BoundedProcess -FilePath $repeated.HelperPath -ArgumentList @('update-helper', '--handoff', $repeated.HandoffPath) -TimeoutSeconds 30 -Operation 'Repeated rollback refusal'
    Assert-UpgradeCondition ($repeatedResult.ExitCode -ne 0) 'The same failed target was allowed to enter an update loop.'
    $repeatedAttempt = Get-Content -Raw -LiteralPath $repeated.AttemptPath | ConvertFrom-Json
    Assert-UpgradeCondition ($repeatedAttempt.phase -eq 'rolled_back' -and $repeatedAttempt.rollback_attempts -eq 1) 'Retry refusal changed the completed rollback attempt.'
    Assert-UpgradeCondition ((Get-InstalledVersion) -eq $script:previousVersion) 'Retry refusal changed the restored installation.'
    Stop-SylvOps -StateRoot $failedState

    Write-Host '[ok] native Windows N-1 helper success, lock refusal, helper and rollback interruption recovery, health rollback, relaunch, and loop prevention verified'
}
finally {
    Get-CimInstance Win32_Process -OperationTimeoutSec 2 | Where-Object {
        $_.ExecutablePath -like "$testRoot*" -or $_.ExecutablePath -eq $script:installedExecutable
    } | ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }
    if ($installed -and (Test-Path -LiteralPath $uninstaller -PathType Leaf)) {
        try {
            $cleanup = Start-Process -FilePath $uninstaller -ArgumentList '/S' -PassThru
            $null = Wait-ForProcessExit -Process $cleanup -TimeoutSeconds 120 -Operation 'Native upgrade cleanup uninstaller'
        }
        catch {
            Write-Warning "Native upgrade cleanup failed: $($_.Exception.Message)"
        }
    }
    if (Test-Path -LiteralPath $testRoot) {
        $resolvedTestRoot = (Resolve-Path -LiteralPath $testRoot).Path
        $resolvedTempRoot = (Resolve-Path -LiteralPath ([System.IO.Path]::GetTempPath())).Path
        if ($resolvedTestRoot.StartsWith($resolvedTempRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
        }
    }
}
