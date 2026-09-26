[CmdletBinding()]
param(
    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path,
    [string]$ArchivePath,
    [string]$InstallerPath,
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

    foreach ($requiredText in @('sylvops up .', 'sylvops tui', 'SHA256SUMS', 'native desktop', 'sylvops-windows-x86_64-setup.exe', 'Start Menu')) {
        Assert-ReleaseCondition ($Text.Contains($requiredText)) "Bundled README is missing release guidance: $requiredText"
    }
}

function Assert-NoLegacyReleasePhaseWording {
    param(
        [string]$Path,
        [string]$Text
    )

    Assert-ReleaseCondition ($Text -notmatch '(?i)\b(beta|prerelease|pre-release|stable release|post-beta)\b') "$Path still uses legacy beta/stable release-phase wording."
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

function Assert-WindowsInstaller {
    param([string]$Path)

    Assert-ReleaseCondition (Test-Path -LiteralPath $Path -PathType Leaf) "Windows installer is missing: $Path"
    Assert-ReleaseCondition ((Get-Item -LiteralPath $Path).Length -gt 0) "Windows installer is empty: $Path"
    Assert-ReleaseCondition ((Split-Path -Leaf $Path) -eq 'sylvops-windows-x86_64-setup.exe') "Unexpected Windows installer name: $Path"

    if ($IsWindows) {
        $signature = Get-AuthenticodeSignature -LiteralPath $Path
        Assert-ReleaseCondition ($null -ne $signature.SignerCertificate) "Windows installer is not Authenticode signed: $Path"
        Assert-ReleaseCondition ($signature.Status -notin @('HashMismatch', 'NotSigned')) "Windows installer has an invalid Authenticode signature: $($signature.Status)"
    }
    Write-Host "[ok] signed Windows installer is present"
}

$root = (Resolve-Path -LiteralPath $RepositoryRoot).Path
$cargo = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.toml')
$versionMatch = [regex]::Match($cargo, '(?ms)^\[workspace\.package\]\s*.*?^version\s*=\s*"([^"]+)"')
Assert-ReleaseCondition $versionMatch.Success "Could not read workspace.package.version from Cargo.toml."
$version = $versionMatch.Groups[1].Value
Assert-ReleaseCondition ($version -match '^0\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') "Workspace version must be an ordinary semantic 0.x version without prerelease or build metadata, found $version."
if (-not [string]::IsNullOrWhiteSpace($ExpectedTag)) {
    Assert-ReleaseCondition ($ExpectedTag -eq "v$version") "Release tag $ExpectedTag does not match workspace version $version."
}

$applicationId = 'com.devemit.sylvops'
$publisher = 'devemit'
$coreLibrary = Get-Content -Raw -LiteralPath (Join-Path $root 'crates\sylvops-core\src\lib.rs')
Assert-ReleaseCondition ($coreLibrary.Contains("APPLICATION_ID: &str = `"$applicationId`"")) "The runtime application ID does not match the package identity."
Assert-ReleaseCondition ($coreLibrary.Contains("APPLICATION_PUBLISHER: &str = `"$publisher`"")) "The runtime publisher does not match the package identity."
$packagerConfigPath = Join-Path $root 'Packager.toml'
Assert-ReleaseCondition (Test-Path -LiteralPath $packagerConfigPath -PathType Leaf) "Packager.toml is missing."
$packagerConfig = Get-Content -Raw -LiteralPath $packagerConfigPath
foreach ($requiredSetting in @(
    'product-name = "SylvOps"',
    "version = `"$version`"",
    "identifier = `"$applicationId`"",
    "publisher = `"$publisher`"",
    'formats = ["nsis"]',
    'installMode = "currentUser"',
    'allow-downgrades = false',
    'binaries-dir = "target/installer-input"',
    'packaging/icons/sylvops.ico'
)) {
    Assert-ReleaseCondition ($packagerConfig.Contains($requiredSetting)) "Packager.toml is missing the locked Windows packaging setting: $requiredSetting"
}

$cliManifest = Get-Content -Raw -LiteralPath (Join-Path $root 'crates\sylvops-cli\Cargo.toml')
foreach ($requiredMetadata in @(
    '[package.metadata.winresource]',
    'ProductName = "SylvOps"',
    "CompanyName = `"$publisher`"",
    'OriginalFilename = "sylvops.exe"'
)) {
    Assert-ReleaseCondition ($cliManifest.Contains($requiredMetadata)) "CLI manifest is missing Windows executable metadata: $requiredMetadata"
}

$buildScriptPath = Join-Path $root 'crates\sylvops-cli\build.rs'
Assert-ReleaseCondition (Test-Path -LiteralPath $buildScriptPath -PathType Leaf) "The CLI Windows resource build script is missing."
$buildScript = Get-Content -Raw -LiteralPath $buildScriptPath
Assert-ReleaseCondition ($buildScript.Contains('packaging/icons/sylvops.ico')) "The CLI build script does not embed the shared SylvOps icon."

foreach ($iconPath in @('packaging\icons\sylvops.png', 'packaging\icons\sylvops.ico')) {
    $resolvedIconPath = Join-Path $root $iconPath
    Assert-ReleaseCondition (Test-Path -LiteralPath $resolvedIconPath -PathType Leaf) "Application icon is missing: $iconPath"
    Assert-ReleaseCondition ((Get-Item -LiteralPath $resolvedIconPath).Length -gt 0) "Application icon is empty: $iconPath"
}

$packagerVersionPath = Join-Path $root 'packaging\cargo-packager.version'
Assert-ReleaseCondition (Test-Path -LiteralPath $packagerVersionPath -PathType Leaf) "The cargo-packager version pin is missing."
$packagerVersion = (Get-Content -Raw -LiteralPath $packagerVersionPath).Trim()
Assert-ReleaseCondition ($packagerVersion -eq '0.11.8') "cargo-packager must stay pinned to the reviewed 0.11.8 release, found $packagerVersion."

foreach ($scriptPath in @(
    'scripts\package-windows-installer.ps1',
    'scripts\sign-windows.ps1',
    'scripts\test-windows-installer.ps1'
)) {
    Assert-ReleaseCondition (Test-Path -LiteralPath (Join-Path $root $scriptPath) -PathType Leaf) "Windows installer workflow script is missing: $scriptPath"
}

$quickstart = Get-Content -Raw -LiteralPath (Join-Path $root 'packaging\windows\QUICKSTART.txt')
Assert-ReleaseCondition ($quickstart -match "(?m)^SYLVOPS $([regex]::Escape($version))$") "Windows QUICKSTART version does not match workspace version $version."

$windowsInstallScriptText = Get-Content -Raw -LiteralPath (Join-Path $root 'scripts\install.ps1')
$windowsDefault = '[string]$Version = "{0}"' -f $version
Assert-ReleaseCondition ($windowsInstallScriptText.Contains($windowsDefault)) "Windows install script default does not match workspace version $version."

$unixInstallScriptText = Get-Content -Raw -LiteralPath (Join-Path $root 'scripts\install.sh')
Assert-ReleaseCondition ($unixInstallScriptText.Contains("SYLVOPS_VERSION:-$version")) "Unix install script default does not match workspace version $version."

$lock = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.lock')
$workspacePackages = [regex]::Matches($lock, '(?ms)^\[\[package\]\]\r?\nname = "(sylvops-[^"]+)"\r?\nversion = "([^"]+)"')
Assert-ReleaseCondition ($workspacePackages.Count -eq 6) "Cargo.lock does not contain the six expected SylvOps workspace packages."
foreach ($package in $workspacePackages) {
    Assert-ReleaseCondition ($package.Groups[2].Value -eq $version) "Cargo.lock version for $($package.Groups[1].Value) does not match workspace version $version."
}

Assert-Quickstart $quickstart
$readme = Get-Content -Raw -LiteralPath (Join-Path $root 'README.md')
Assert-BundledReadme $readme
Assert-ReleaseCondition ($readme.Contains('Start-Process sylvops')) "README does not document the installed CLI discovery surface."
Assert-ReleaseCondition ($readme.Contains('preserves configuration, session data, repositories, worktrees, and branches')) "README does not state the Windows uninstall preservation contract."

$releasingGuide = Get-Content -Raw -LiteralPath (Join-Path $root 'docs\development\releasing.md')
foreach ($requiredText in @(
    'cargo-packager 0.11.8',
    'WINDOWS_SIGNING_CERTIFICATE_BASE64',
    'WINDOWS_SIGNING_CERTIFICATE_PASSWORD',
    'WINDOWS_SIGNING_TIMESTAMP_URL',
    '%LOCALAPPDATA%\Programs\SylvOps',
    '%LOCALAPPDATA%\SylvOps',
    '%APPDATA%\SylvOps'
)) {
    Assert-ReleaseCondition ($releasingGuide.Contains($requiredText)) "Release guide is missing Windows installer guidance: $requiredText"
}

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
Assert-ReleaseCondition ($release.Contains('environment: release')) "Release publication is not protected by the release environment gate."
Assert-ReleaseCondition ($release.Contains('gh release create')) "Release workflow does not publish through GitHub Releases."
Assert-ReleaseCondition ($release.Contains('target/release/sylvops.exe --version')) "Windows package job does not verify the application version."
Assert-ReleaseCondition ($release.Contains('target/${{ matrix.target }}/release/sylvops --version')) "Unix package jobs do not verify the application version."
Assert-ReleaseCondition ($release.Contains('packaging/cargo-packager.version')) "Windows packaging does not use the checked-in cargo-packager version pin."
Assert-ReleaseCondition ($release.Contains('cargo install cargo-packager --version $packagerVersion --locked')) "Windows packaging does not install the exact locked cargo-packager version."
Assert-ReleaseCondition ($release.Contains('WINDOWS_SIGNING_CERTIFICATE_BASE64')) "Release packaging does not load the protected Windows signing certificate."
Assert-ReleaseCondition ($release.Contains('WINDOWS_SIGNING_CERTIFICATE_PASSWORD')) "Release packaging does not load the protected Windows signing certificate password."
Assert-ReleaseCondition ($release -match '(?ms)^  package-windows:.*?^    environment: release$') "The Windows signing job does not use the protected release environment."
Assert-ReleaseCondition ($release.Contains('scripts/package-windows-installer.ps1')) "Release packaging does not build the signed Windows installer."
Assert-ReleaseCondition ($release.Contains('scripts/test-windows-installer.ps1')) "Release packaging does not run the native Windows installer smoke test."
Assert-ReleaseCondition ($release.Contains('sylvops-windows-x86_64-setup.exe')) "Release packaging does not publish the Windows installer asset."
Assert-NoLegacyReleasePhaseWording -Path '.github\workflows\release.yml' -Text $release

$ciInstallerJob = Get-Content -Raw -LiteralPath (Join-Path $root '.github\workflows\ci.yml')
Assert-ReleaseCondition ($ciInstallerJob.Contains('windows-installer')) "CI is missing the clean native Windows installer job."
Assert-ReleaseCondition ($ciInstallerJob.Contains('New-SelfSignedCertificate')) "CI does not create an isolated test signing identity for installer verification."
Assert-ReleaseCondition ($ciInstallerJob.Contains('scripts/package-windows-installer.ps1')) "CI does not exercise the production Windows packaging script."
Assert-ReleaseCondition ($ciInstallerJob.Contains('scripts/test-windows-installer.ps1')) "CI does not install, launch, verify, and uninstall the Windows package."

$activeNonDocumentationSurfaces = @(
    'packaging\windows\QUICKSTART.txt',
    'scripts\install.ps1',
    'scripts\install.sh',
    'crates\sylvops-tui\src\lib.rs'
)
foreach ($relativePath in $activeNonDocumentationSurfaces) {
    $text = Get-Content -Raw -LiteralPath (Join-Path $root $relativePath)
    Assert-NoLegacyReleasePhaseWording -Path $relativePath -Text $text
}

$historicalPlanRoot = Join-Path $root 'docs\wayfinding\mvp-beta'
foreach ($historicalFile in Get-ChildItem -LiteralPath $historicalPlanRoot -Recurse -File -Filter '*.md') {
    $text = Get-Content -Raw -LiteralPath $historicalFile.FullName
    Assert-ReleaseCondition ($text.Contains('Historical release-planning record:')) "$($historicalFile.FullName) is legacy release planning without a historical marker."
}

foreach ($relativePath in @(
    'docs\decisions\0011-atomic-windows-conpty-job-launch.md',
    'docs\decisions\0012-beta-management-and-embedded-tui.md',
    'docs\decisions\0013-hierarchical-tui-and-navigation-state.md',
    'docs\decisions\0016-calm-desktop-visual-language.md'
)) {
    $text = Get-Content -Raw -LiteralPath (Join-Path $root $relativePath)
    Assert-ReleaseCondition ($text.Contains('Historical context:')) "$relativePath uses legacy release-phase terminology without a historical marker."
}

$documentationFiles = @(
    (Get-Item -LiteralPath (Join-Path $root 'README.md')),
    (Get-Item -LiteralPath (Join-Path $root 'AGENTS.md')),
    (Get-Item -LiteralPath (Join-Path $root 'CONTEXT.md'))
) + @(Get-ChildItem -LiteralPath (Join-Path $root 'docs') -Recurse -File -Filter '*.md')
foreach ($documentationFile in $documentationFiles) {
    $text = Get-Content -Raw -LiteralPath $documentationFile.FullName
    if ($text.Contains('Historical release-planning record:') -or $text.Contains('Historical context:')) {
        continue
    }
    $visibleText = $text -replace '\]\([^)]+\)', ']'
    $relativePath = [System.IO.Path]::GetRelativePath($root, $documentationFile.FullName)
    Assert-NoLegacyReleasePhaseWording -Path $relativePath -Text $visibleText
}

$releaseCommit = @(& git -C $root rev-parse HEAD 2>&1)
Assert-ReleaseCondition ($LASTEXITCODE -eq 0) "Could not identify the release commit: $($releaseCommit -join [Environment]::NewLine)"
Write-Host "[ok] source release contract validated for $($releaseCommit[0]) ($version)"

if (-not [string]::IsNullOrWhiteSpace($ArchivePath)) {
    Assert-Archive -Path (Resolve-Path -LiteralPath $ArchivePath).Path
}

if (-not [string]::IsNullOrWhiteSpace($InstallerPath)) {
    Assert-WindowsInstaller -Path (Resolve-Path -LiteralPath $InstallerPath).Path
}

if (-not [string]::IsNullOrWhiteSpace($DistDirectory)) {
    $dist = (Resolve-Path -LiteralPath $DistDirectory).Path
    $archives = @(
        'sylvops-windows-x86_64.zip',
        'sylvops-linux-x86_64.tar.gz',
        'sylvops-macos-x86_64.tar.gz',
        'sylvops-macos-aarch64.tar.gz'
    )
    $installer = 'sylvops-windows-x86_64-setup.exe'
    $releaseAssets = @($archives + $installer)
    $expectedFiles = @($releaseAssets + 'SHA256SUMS')
    $actualFiles = @(Get-ChildItem -LiteralPath $dist -File | ForEach-Object { $_.Name })
    $directories = @(Get-ChildItem -LiteralPath $dist -Directory)
    Assert-ReleaseCondition ($directories.Count -eq 0) "Release bundle directory contains unexpected subdirectories."
    Assert-ExactNames $actualFiles $expectedFiles 'release bundle'

    $manifestPath = Join-Path $dist 'SHA256SUMS'
    $manifest = @{}
    foreach ($line in Get-Content -LiteralPath $manifestPath) {
        $match = [regex]::Match($line, '^([0-9a-fA-F]{64})\s+\*?(.+)$')
        Assert-ReleaseCondition $match.Success "Malformed SHA256SUMS line: $line"
        $manifestName = $match.Groups[2].Value
        Assert-ReleaseCondition (-not $manifest.ContainsKey($manifestName)) "Duplicate SHA256SUMS entry: $manifestName"
        $manifest[$manifestName] = $match.Groups[1].Value.ToLowerInvariant()
    }
    Assert-ExactNames @($manifest.Keys) $releaseAssets 'SHA256SUMS'
    foreach ($asset in $releaseAssets) {
        $path = Join-Path $dist $asset
        $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
        Assert-ReleaseCondition ($actualHash -eq $manifest[$asset]) "Checksum mismatch for $asset."
        if ($asset -eq $installer) {
            Assert-WindowsInstaller -Path $path
        }
        else {
            Assert-Archive -Path $path
        }
        Write-Host "[ok] $asset sha256=$actualHash"
    }
    Write-Host "[ok] release bundle has exactly five validated assets and one checksum manifest"
}
