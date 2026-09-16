[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$Repository,

    [string]$StateDirectory,

    [switch]$SkipTui
)

$ErrorActionPreference = "Stop"
$executable = Join-Path $PSScriptRoot "sylvops.exe"
$baseArguments = @()

if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "sylvops.exe is missing from this package."
}

if (-not [string]::IsNullOrWhiteSpace($StateDirectory)) {
    New-Item -ItemType Directory -Path $StateDirectory -Force | Out-Null
    $baseArguments = @("--state-dir", (Resolve-Path -LiteralPath $StateDirectory).Path)
}

if ([string]::IsNullOrWhiteSpace($Repository)) {
    $Repository = Read-Host "Git repository to open (press Enter for the current directory)"
    if ([string]::IsNullOrWhiteSpace($Repository)) {
        $Repository = (Get-Location).Path
    }
}

$resolvedRepository = (Resolve-Path -LiteralPath $Repository).Path
$gitRoot = (& git -C $resolvedRepository rev-parse --show-toplevel)
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($gitRoot)) {
    throw "The selected directory is not inside a Git repository."
}
$gitRoot = (Resolve-Path -LiteralPath $gitRoot.Trim()).Path

function ConvertTo-ComparablePath {
    param([Parameter(Mandatory)][string]$Path)

    $value = $Path
    if ($value.StartsWith("\\?\", [System.StringComparison]::Ordinal)) {
        $value = $value.Substring(4)
    }
    return [System.IO.Path]::GetFullPath($value).TrimEnd('\')
}

$comparableGitRoot = ConvertTo-ComparablePath -Path $gitRoot

function Invoke-SylvOps {
    param([Parameter(Mandatory)][string[]]$Arguments)

    $result = & $executable @baseArguments @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "SylvOps command failed: $($Arguments -join ' ')"
    }
    return $result
}

Write-Host "Starting the local SylvOps daemon..."
# Keep daemon startup attached directly to this console. Routing its output through a PowerShell
# pipeline can retain an inherited pipe handle in the detached daemon and make the pipeline wait.
& $executable @baseArguments "daemon" "start"
if ($LASTEXITCODE -ne 0) {
    throw "SylvOps could not start its daemon."
}
Write-Host "Loading SylvOps state..."
$snapshot = (Invoke-SylvOps -Arguments @("snapshot") | Out-String | ConvertFrom-Json)
$project = $snapshot.projects |
    Where-Object {
        (ConvertTo-ComparablePath -Path $_.canonical_repository_path) -ieq $comparableGitRoot
    } |
    Select-Object -First 1

if ($null -eq $project) {
    $workspace = $snapshot.workspaces | Sort-Object created_at -Descending | Select-Object -First 1
    if ($null -eq $workspace) {
        Write-Host "Creating the first workspace..."
        Invoke-SylvOps -Arguments @("workspace", "add", "Local") | Out-Host
        $snapshot = (Invoke-SylvOps -Arguments @("snapshot") | Out-String | ConvertFrom-Json)
        $workspace = $snapshot.workspaces | Sort-Object created_at -Descending | Select-Object -First 1
    }

    Write-Host "Registering $gitRoot..."
    Invoke-SylvOps -Arguments @("project", "add", "--workspace", $workspace.id, $gitRoot) | Out-Host
    $snapshot = (Invoke-SylvOps -Arguments @("snapshot") | Out-String | ConvertFrom-Json)
    $project = $snapshot.projects |
        Where-Object {
            (ConvertTo-ComparablePath -Path $_.canonical_repository_path) -ieq $comparableGitRoot
        } |
        Select-Object -First 1
}

$rootWorktree = $snapshot.worktrees |
    Where-Object { $_.project_id -eq $project.id -and $_.is_root_checkout } |
    Select-Object -First 1
if ($null -eq $rootWorktree) {
    throw "SylvOps did not return a root checkout for the selected repository."
}

$session = $snapshot.sessions |
    Where-Object {
        $_.worktree_id -eq $rootWorktree.id -and
        $_.state -in @("starting", "running", "needs_feedback")
    } |
    Select-Object -First 1
if ($null -eq $session) {
    Write-Host "Creating the first persistent shell session..."
    Invoke-SylvOps -Arguments @(
        "session", "create",
        "--worktree", $rootWorktree.id,
        "--provider", "shell",
        "--name", "First shell"
    ) | Out-Host
}

if (-not $SkipTui) {
    Write-Host "Opening SylvOps. Press ? for shortcuts; press q to leave sessions running."
    Invoke-SylvOps -Arguments @("tui") | Out-Host
}
