[CmdletBinding()]
param(
    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path,
    [string]$ArchivePath,
    [string]$DistDirectory,
    [string]$ExpectedTag
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Assert-ReleaseCondition {
    param(
        [bool]$Condition,
        [string]$Message
    )

    if (-not $Condition) {
        throw $Message
    }
}

function Assert-ExactNames {
    param(
        [string[]]$Actual,
        [string[]]$Expected,
        [string]$Label
    )

    $actualSorted = @($Actual | Sort-Object)
    $expectedSorted = @($Expected | Sort-Object)
    $difference = @(Compare-Object -ReferenceObject $expectedSorted -DifferenceObject $actualSorted)
    Assert-ReleaseCondition ($difference.Count -eq 0) "$Label contents differ from the locked package manifest: $($difference | Out-String)"
}

function Assert-Quickstart {
    param([string]$Text)

    Assert-ReleaseCondition ($Text -match '(?i)native desktop') "Windows QUICKSTART must describe the native desktop launcher."
    Assert-ReleaseCondition ($Text -match [regex]::Escape('-Repository C:\path\to\repository')) "Windows QUICKSTART must show how to pass a repository to the launcher."
    Assert-ReleaseCondition ($Text -match [regex]::Escape('.\sylvops.exe tui')) "Windows QUICKSTART must retain the explicit TUI fallback command."
    Assert-ReleaseCondition ($Text -match [regex]::Escape('Ctrl+]')) "Windows QUICKSTART must document explicit detach."
    Assert-ReleaseCondition ($Text -notmatch '(?i)opens the terminal UI') "Windows QUICKSTART still claims that the launcher opens the terminal UI."
    Assert-ReleaseCondition ($Text -notmatch '(?i)Tab or h/l: move between panels') "Windows QUICKSTART still contains the obsolete TUI panel shortcut."
}

function Assert-BundledReadme {
    param([string]$Text)

    foreach ($requiredText in @('sylvops up .', 'sylvops tui', 'SHA256SUMS', 'Open Anyway', 'native desktop')) {
        Assert-ReleaseCondition ($Text.Contains($requiredText)) "Bundled README is missing release guidance: $requiredText"
    }
}

function Get-ArchiveNames {
    param([string]$Path)

    if ($Path.EndsWith('.zip', [System.StringComparison]::OrdinalIgnoreCase)) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $archive = [System.IO.Compression.ZipFile]::OpenRead($Path)
        try {
            return @($archive.Entries | Where-Object { -not [string]::IsNullOrEmpty($_.Name) } | ForEach-Object { $_.FullName.Replace('\', '/') })
        }
        finally {
            $archive.Dispose()
        }
    }

    $entries = @(& tar -tzf $Path 2>&1)
    Assert-ReleaseCondition ($LASTEXITCODE -eq 0) "Could not inspect archive $Path with tar: $($entries -join [Environment]::NewLine)"
    return @($entries | ForEach-Object { $_.TrimStart('.', '/') } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and -not $_.EndsWith('/') })
}

function Get-ArchiveText {
    param(
        [string]$Path,
        [string]$EntryName
    )

    if ($Path.EndsWith('.zip', [System.StringComparison]::OrdinalIgnoreCase)) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $archive = [System.IO.Compression.ZipFile]::OpenRead($Path)
        try {
            $entry = $archive.GetEntry($EntryName)
            Assert-ReleaseCondition ($null -ne $entry) "Archive does not contain $EntryName."
            $reader = [System.IO.StreamReader]::new($entry.Open())
            try {
                return $reader.ReadToEnd()
            }
            finally {
                $reader.Dispose()
            }
        }
        finally {
            $archive.Dispose()
        }
    }

    $text = @(& tar -xOzf $Path $EntryName 2>&1)
    Assert-ReleaseCondition ($LASTEXITCODE -eq 0) "Could not read $EntryName from $Path with tar: $($text -join [Environment]::NewLine)"
    return $text -join [Environment]::NewLine
}

function Assert-Archive {
    param([string]$Path)

    Assert-ReleaseCondition (Test-Path -LiteralPath $Path -PathType Leaf) "Release archive is missing: $Path"
    Assert-ReleaseCondition ((Get-Item -LiteralPath $Path).Length -gt 0) "Release archive is empty: $Path"
    $name = Split-Path -Leaf $Path
    $expectedByArchive = @{
        'sylvops-windows-x86_64.zip' = @('sylvops.exe', 'README.md', 'LICENSE', 'Start-SylvOps.ps1', 'Start SylvOps.cmd', 'QUICKSTART.txt')
        'sylvops-linux-x86_64.tar.gz' = @('sylvops', 'README.md', 'LICENSE')
        'sylvops-macos-x86_64.tar.gz' = @('sylvops', 'README.md', 'LICENSE')
        'sylvops-macos-aarch64.tar.gz' = @('sylvops', 'README.md', 'LICENSE')
    }
    Assert-ReleaseCondition $expectedByArchive.ContainsKey($name) "Unexpected release archive name: $name"
    Assert-ExactNames (Get-ArchiveNames -Path $Path) $expectedByArchive[$name] $name
    Assert-BundledReadme (Get-ArchiveText -Path $Path -EntryName 'README.md')
    if ($name -eq 'sylvops-windows-x86_64.zip') {
        Assert-Quickstart (Get-ArchiveText -Path $Path -EntryName 'QUICKSTART.txt')
    }
    Write-Host "[ok] $name has the locked package contents"
}

$root = (Resolve-Path -LiteralPath $RepositoryRoot).Path
$cargo = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.toml')
$versionMatch = [regex]::Match($cargo, '(?ms)^\[workspace\.package\]\s*.*?^version\s*=\s*"([^"]+)"')
Assert-ReleaseCondition $versionMatch.Success "Could not read workspace.package.version from Cargo.toml."
$version = $versionMatch.Groups[1].Value
Assert-ReleaseCondition ($version -eq '0.1.0-beta.1') "Release-proof validation is locked to workspace version 0.1.0-beta.1, found $version."
if (-not [string]::IsNullOrWhiteSpace($ExpectedTag)) {
    Assert-ReleaseCondition ($ExpectedTag -eq "v$version") "Release tag $ExpectedTag does not match workspace version $version."
}

$quickstart = Get-Content -Raw -LiteralPath (Join-Path $root 'packaging\windows\QUICKSTART.txt')
Assert-Quickstart $quickstart
$readme = Get-Content -Raw -LiteralPath (Join-Path $root 'README.md')
Assert-BundledReadme $readme

$ci = Get-Content -Raw -LiteralPath (Join-Path $root '.github\workflows\ci.yml')
foreach ($command in @(
    'cargo fmt --all -- --check',
    'cargo clippy --workspace --all-targets --all-features -- -D warnings',
    'cargo test --workspace --all-targets'
)) {
    Assert-ReleaseCondition ($ci.Contains($command)) "CI is missing required command: $command"
}
foreach ($runner in @('ubuntu-latest', 'windows-latest', 'macos-latest')) {
    Assert-ReleaseCondition ($ci.Contains($runner)) "CI is missing required native runner: $runner"
}

$release = Get-Content -Raw -LiteralPath (Join-Path $root '.github\workflows\release.yml')
foreach ($asset in @(
    'sylvops-windows-x86_64.zip',
    'sylvops-linux-x86_64.tar.gz',
    'sylvops-macos-x86_64.tar.gz',
    'sylvops-macos-aarch64.tar.gz'
)) {
    Assert-ReleaseCondition ($release.Contains($asset)) "Release workflow is missing required archive: $asset"
}
Assert-ReleaseCondition ($release.Contains('uses: ./.github/workflows/ci.yml')) "Release workflow does not call the complete CI workflow."
Assert-ReleaseCondition ($release.Contains('cargo build --release --locked')) "Release workflow does not build with Cargo.lock enforced."
Assert-ReleaseCondition ($release.Contains('test -x "target/${{ matrix.target }}/release/sylvops"')) "Release workflow does not verify Unix executable permissions."
Assert-ReleaseCondition ($release.Contains('actions/attest-build-provenance@')) "Release workflow does not create build-provenance attestations."
Assert-ReleaseCondition ($release.Contains('scripts/validate-release.ps1')) "Release workflow does not invoke release-package validation."
Assert-ReleaseCondition ($release.Contains('environment: beta-release')) "Release publication is not protected by the beta-release evidence gate."

$candidateCommit = @(& git -C $root rev-parse HEAD 2>&1)
Assert-ReleaseCondition ($LASTEXITCODE -eq 0) "Could not identify the candidate commit: $($candidateCommit -join [Environment]::NewLine)"
Write-Host "[ok] source release contract validated for $($candidateCommit[0]) ($version)"

if (-not [string]::IsNullOrWhiteSpace($ArchivePath)) {
    Assert-Archive -Path (Resolve-Path -LiteralPath $ArchivePath).Path
}

if (-not [string]::IsNullOrWhiteSpace($DistDirectory)) {
    $dist = (Resolve-Path -LiteralPath $DistDirectory).Path
    $archives = @(
        'sylvops-windows-x86_64.zip',
        'sylvops-linux-x86_64.tar.gz',
        'sylvops-macos-x86_64.tar.gz',
        'sylvops-macos-aarch64.tar.gz'
    )
    $expectedFiles = @($archives + 'SHA256SUMS')
    $actualFiles = @(Get-ChildItem -LiteralPath $dist -File | ForEach-Object { $_.Name })
    $directories = @(Get-ChildItem -LiteralPath $dist -Directory)
    Assert-ReleaseCondition ($directories.Count -eq 0) "Release candidate directory contains unexpected subdirectories."
    Assert-ExactNames $actualFiles $expectedFiles 'release candidate'

    $manifestPath = Join-Path $dist 'SHA256SUMS'
    $manifest = @{}
    foreach ($line in Get-Content -LiteralPath $manifestPath) {
        $match = [regex]::Match($line, '^([0-9a-fA-F]{64})\s+\*?(.+)$')
        Assert-ReleaseCondition $match.Success "Malformed SHA256SUMS line: $line"
        $manifestName = $match.Groups[2].Value
        Assert-ReleaseCondition (-not $manifest.ContainsKey($manifestName)) "Duplicate SHA256SUMS entry: $manifestName"
        $manifest[$manifestName] = $match.Groups[1].Value.ToLowerInvariant()
    }
    Assert-ExactNames @($manifest.Keys) $archives 'SHA256SUMS'
    foreach ($archive in $archives) {
        $path = Join-Path $dist $archive
        $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
        Assert-ReleaseCondition ($actualHash -eq $manifest[$archive]) "Checksum mismatch for $archive."
        Assert-Archive -Path $path
        Write-Host "[ok] $archive sha256=$actualHash"
    }
    Write-Host "[ok] release candidate has exactly four validated archives and one checksum manifest"
}
