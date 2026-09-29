[CmdletBinding()]
param(
    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path,
    [string]$ArchivePath,
    [string]$InstallerPath,
    [string[]]$MacosPackagePath,
    [string[]]$LinuxPackagePath,
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

    foreach ($requiredText in @('sylvops up .', 'sylvops tui', 'SHA256SUMS', 'native desktop', 'sylvops-windows-x86_64-setup.exe', 'Start Menu', 'sylvops-linux-x86_64.AppImage', 'sylvops-linux-x86_64.deb')) {
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

function Assert-MacosPackage {
    param([string]$Path)

    Assert-ReleaseCondition (Test-Path -LiteralPath $Path -PathType Leaf) "macOS package is missing: $Path"
    Assert-ReleaseCondition ((Get-Item -LiteralPath $Path).Length -gt 0) "macOS package is empty: $Path"
    $name = Split-Path -Leaf $Path
    Assert-ReleaseCondition ($name -in @('sylvops-macos-x86_64.dmg', 'sylvops-macos-aarch64.dmg')) "Unexpected macOS package name: $name"

    Write-Host "[ok] signed and notarized macOS package is present: $name"
}

function Assert-LinuxPackage {
    param([string]$Path)

    Assert-ReleaseCondition (Test-Path -LiteralPath $Path -PathType Leaf) "Linux package is missing: $Path"
    Assert-ReleaseCondition ((Get-Item -LiteralPath $Path).Length -gt 0) "Linux package is empty: $Path"
    $name = Split-Path -Leaf $Path
    Assert-ReleaseCondition ($name -in @('sylvops-linux-x86_64.AppImage', 'sylvops-linux-x86_64.deb')) "Unexpected Linux package name: $name"
    if ($name.EndsWith('.AppImage') -and -not $IsWindows) {
        & test -x $Path
        Assert-ReleaseCondition ($LASTEXITCODE -eq 0) "AppImage is not executable: $Path"
    }

    Write-Host "[ok] native Linux package is present: $name"
}

function Assert-UpdateManifest {
    param(
        [string]$Path,
        [string]$AssetPath,
        [string]$ExpectedVersion
    )

    Assert-ReleaseCondition (Test-Path -LiteralPath $Path -PathType Leaf) "Update manifest is missing: $Path"
    $manifest = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
    Assert-ReleaseCondition ($manifest.release.schema_version -eq 1) "Update manifest schema is not version 1: $Path"
    Assert-ReleaseCondition ($manifest.release.target_version -eq $ExpectedVersion) "Update manifest version does not match the package version: $Path"
    Assert-ReleaseCondition ($manifest.signature -match '^[A-Za-z0-9+/]{86}==$') "Update manifest signature is not a 64-byte base64 Ed25519 signature: $Path"
    $asset = Get-Item -LiteralPath $AssetPath
    Assert-ReleaseCondition ([uint64]$manifest.release.byte_length -eq [uint64]$asset.Length) "Update manifest length does not match its asset: $Path"
    $digest = (Get-FileHash -Algorithm SHA256 -LiteralPath $AssetPath).Hash.ToLowerInvariant()
    Assert-ReleaseCondition ($manifest.release.sha256 -eq $digest) "Update manifest digest does not match its asset: $Path"
    Assert-ReleaseCondition ($manifest.release.installer_url.EndsWith('/' + $asset.Name)) "Update manifest URL does not name its asset: $Path"
    Write-Host "[ok] signed update manifest matches $($asset.Name)"
}

function Assert-ReleaseEvidence {
    param(
        [string]$Path,
        [string]$ExpectedVersion
    )

    $evidence = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
    Assert-ReleaseCondition ($evidence.schema_version -eq 1) "Release evidence schema is invalid."
    Assert-ReleaseCondition ($evidence.version -eq $ExpectedVersion) "Release evidence version does not match."
    Assert-ReleaseCondition ($evidence.commit -match '^[0-9a-f]{40}$') "Release evidence commit is invalid."
    foreach ($target in @('windows_x86_64', 'linux_x86_64', 'macos_x86_64', 'macos_aarch64')) {
        Assert-ReleaseCondition ($evidence.native_package_jobs.$target -eq 'passed') "Release evidence is missing passed native validation for $target."
    }
    Assert-ReleaseCondition ($evidence.upgrade_contract.signed_metadata -eq 'verified') "Release evidence is missing signed-metadata validation."
    Assert-ReleaseCondition ($evidence.upgrade_contract.staged_payloads -eq 'verified') "Release evidence is missing staged-payload validation."
    Assert-ReleaseCondition ($evidence.upgrade_contract.coordinator_rollback_unit -eq 'passed') "Release evidence is missing coordinator rollback validation."
    Assert-ReleaseCondition ($evidence.upgrade_contract.detached_helper_rollback_integration -eq 'passed') "Release evidence is missing detached-helper rollback integration validation."
    if ($ExpectedVersion -eq '0.1.0') {
        Assert-ReleaseCondition ($evidence.upgrade_contract.native_package_upgrade_and_rollback -in @('passed', 'not_applicable_initial_release')) "Initial release evidence has an invalid native-package upgrade result."
    }
    else {
        Assert-ReleaseCondition ($evidence.upgrade_contract.native_package_upgrade_and_rollback -eq 'passed') "Release evidence is missing native N-1 package upgrade and rollback validation."
    }
    Write-Host "[ok] release evidence covers every supported native target"
}

$root = (Resolve-Path -LiteralPath $RepositoryRoot).Path
$cargo = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.toml')
$versionMatch = [regex]::Match($cargo, '(?ms)^\[workspace\.package\]\s*.*?^version\s*=\s*"([^"]+)"')
Assert-ReleaseCondition $versionMatch.Success "Could not read workspace.package.version from Cargo.toml."
$version = $versionMatch.Groups[1].Value
Assert-ReleaseCondition ($version -match '^0\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') "Workspace version must be an ordinary semantic 0.x version without prerelease or build metadata, found $version."
if (-not [string]::IsNullOrWhiteSpace($ExpectedTag)) {
    Assert-ReleaseCondition ($ExpectedTag -eq "v$version") "Release tag $ExpectedTag does not match workspace version $version."
    Assert-ReleaseCondition (-not [string]::IsNullOrWhiteSpace($env:SYLVOPS_UPDATE_PUBLIC_KEY_BASE64)) "Tagged releases require UPDATE_SIGNING_PUBLIC_KEY_BASE64 to be embedded in application binaries."
}

$applicationId = 'com.devemit.sylvops'
$publisher = 'devemit'
$coreLibrary = Get-Content -Raw -LiteralPath (Join-Path $root 'crates\sylvops-core\src\lib.rs')
Assert-ReleaseCondition ($coreLibrary.Contains("APPLICATION_ID: &str = `"$applicationId`"")) "The runtime application ID does not match the package identity."
Assert-ReleaseCondition ($coreLibrary.Contains('LINUX_DESKTOP_ID: &str = "sylvops"')) "The shared Linux desktop-file identity must match sylvops.desktop."
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
    'scripts\test-windows-installer.ps1',
    'scripts\test-windows-native-upgrade.ps1'
)) {
    Assert-ReleaseCondition (Test-Path -LiteralPath (Join-Path $root $scriptPath) -PathType Leaf) "Windows installer workflow script is missing: $scriptPath"
}

$macosPackagerConfigPath = Join-Path $root 'packaging\macos\Packager.toml'
Assert-ReleaseCondition (Test-Path -LiteralPath $macosPackagerConfigPath -PathType Leaf) "The macOS packager configuration is missing."
$macosPackagerConfig = Get-Content -Raw -LiteralPath $macosPackagerConfigPath
foreach ($requiredSetting in @(
    'product-name = "SylvOps"',
    "version = `"$version`"",
    "identifier = `"$applicationId`"",
    'formats = ["app"]',
    'packaging/icons/sylvops.png',
    'entitlements = "packaging/macos/entitlements.plist"'
)) {
    Assert-ReleaseCondition ($macosPackagerConfig.Contains($requiredSetting)) "The macOS packager configuration is missing the locked setting: $requiredSetting"
}

foreach ($macosPath in @(
    'packaging\macos\entitlements.plist',
    'scripts\package-macos.sh',
    'scripts\test-macos-package.sh'
)) {
    Assert-ReleaseCondition (Test-Path -LiteralPath (Join-Path $root $macosPath) -PathType Leaf) "The macOS package workflow file is missing: $macosPath"
}
$macosPackageScript = Get-Content -Raw -LiteralPath (Join-Path $root 'scripts\package-macos.sh')
foreach ($requiredText in @('run_with_timeout', '--options runtime', 'Developer ID Application:', 'notarytool submit', 'stapler staple', 'stapler validate')) {
    Assert-ReleaseCondition ($macosPackageScript.Contains($requiredText)) "The macOS package script is missing a release control: $requiredText"
}
$macosSmokeScript = Get-Content -Raw -LiteralPath (Join-Path $root 'scripts\test-macos-package.sh')
foreach ($requiredText in @('run_with_timeout', 'spctl --assess', 'open -na', 'CFBundleIdentifier', 'TeamIdentifier', 'package-preserve.txt', 'show-ref --verify')) {
    Assert-ReleaseCondition ($macosSmokeScript.Contains($requiredText)) "The macOS package smoke test is missing an acceptance check: $requiredText"
}

$linuxPackagerConfigPath = Join-Path $root 'packaging\linux\Packager.toml'
Assert-ReleaseCondition (Test-Path -LiteralPath $linuxPackagerConfigPath -PathType Leaf) "The Linux packager configuration is missing."
$linuxPackagerConfig = Get-Content -Raw -LiteralPath $linuxPackagerConfigPath
foreach ($requiredSetting in @(
    'product-name = "SylvOps"',
    "version = `"$version`"",
    "identifier = `"$applicationId`"",
    "publisher = `"$publisher`"",
    "authors = [`"$publisher`"]",
    'formats = ["appimage", "deb"]',
    'packaging/icons/sylvops.png',
    'generate-desktop-entry = true',
    'desktop-template = "packaging/linux/sylvops.desktop.hbs"',
    'usr/share/metainfo/com.devemit.sylvops.metainfo.xml',
    'dpkg-repack',
    'policykit-1'
)) {
    Assert-ReleaseCondition ($linuxPackagerConfig.Contains($requiredSetting)) "The Linux packager configuration is missing the locked setting: $requiredSetting"
}

$linuxDesktopEntryPath = Join-Path $root 'packaging\linux\sylvops.desktop.hbs'
Assert-ReleaseCondition (Test-Path -LiteralPath $linuxDesktopEntryPath -PathType Leaf) "The Linux desktop entry template is missing."
$linuxDesktopEntry = Get-Content -Raw -LiteralPath $linuxDesktopEntryPath
foreach ($requiredText in @('Type=Application', 'Name={{name}}', 'Exec={{exec}}', 'Icon={{icon}}', 'Terminal=false', 'StartupWMClass=sylvops')) {
    Assert-ReleaseCondition ($linuxDesktopEntry.Contains($requiredText)) "The Linux desktop entry is missing required metadata: $requiredText"
}
$desktopSource = Get-Content -Raw -LiteralPath (Join-Path $root 'crates\sylvops-desktop\src\lib.rs')
Assert-ReleaseCondition ($desktopSource.Contains('settings.platform_specific.application_id = sylvops_core::LINUX_DESKTOP_ID.to_owned();')) "The Linux window identity must use the shared desktop-file identity."

$linuxMetainfoPath = Join-Path $root 'packaging\linux\com.devemit.sylvops.metainfo.xml'
Assert-ReleaseCondition (Test-Path -LiteralPath $linuxMetainfoPath -PathType Leaf) "The Linux AppStream metadata is missing."
$linuxMetainfo = Get-Content -Raw -LiteralPath $linuxMetainfoPath
foreach ($requiredText in @('<id>com.devemit.sylvops</id>', '<name>SylvOps</name>', '<project_license>MIT</project_license>', '<launchable type="desktop-id">sylvops.desktop</launchable>')) {
    Assert-ReleaseCondition ($linuxMetainfo.Contains($requiredText)) "The Linux AppStream metadata is missing required content: $requiredText"
}

$linuxPackageScriptPath = Join-Path $root 'scripts\package-linux.sh'
Assert-ReleaseCondition (Test-Path -LiteralPath $linuxPackageScriptPath -PathType Leaf) "The Linux package script is missing."
$linuxPackageScript = Get-Content -Raw -LiteralPath $linuxPackageScriptPath
foreach ($requiredText in @('run_with_timeout', 'x86_64-unknown-linux-gnu', 'cargo build --release --locked -p sylvops-cli', 'cargo packager', '--formats appimage,deb', 'sylvops-linux-x86_64.AppImage', 'sylvops-linux-x86_64.deb')) {
    Assert-ReleaseCondition ($linuxPackageScript.Contains($requiredText)) "The Linux package script is missing a release control: $requiredText"
}

$linuxSmokeScriptPath = Join-Path $root 'scripts\test-linux-packages.sh'
Assert-ReleaseCondition (Test-Path -LiteralPath $linuxSmokeScriptPath -PathType Leaf) "The Linux package smoke test is missing."
$linuxSmokeScript = Get-Content -Raw -LiteralPath $linuxSmokeScriptPath
foreach ($requiredText in @('run_with_timeout', 'run_with_timeout 10 realpath --', 'APPIMAGE_EXTRACT_AND_RUN=1', '--appimage-extract', '--previous-appimage', '--previous-deb', '--allow-downgrades', 'run_with_timeout 30 dpkg-deb --info', 'run_with_timeout 30 dpkg-deb --contents', 'run_with_timeout 300 sudo --non-interactive env DEBIAN_FRONTEND=noninteractive apt-get install', 'run_with_timeout 30 ldd /usr/bin/sylvops', 'run_with_timeout 30 dpkg-query -L sylvops', 'gtk-launch sylvops', 'desktop-file-validate', 'daemon status', 'pgrep -f', 'run_with_timeout 120 sudo --non-interactive dpkg --remove', 'package-preserve.txt', 'show-ref --verify', 'user_integration_path', 'usr/share/applications/sylvops.desktop', 'usr/share/metainfo/com.devemit.sylvops.metainfo.xml', 'apps/sylvops\.png')) {
    Assert-ReleaseCondition ($linuxSmokeScript.Contains($requiredText)) "The Linux package smoke test is missing an acceptance check: $requiredText"
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
foreach ($requiredText in @('sylvops-macos-x86_64.dmg', 'sylvops-macos-aarch64.dmg', '/Applications', 'Gatekeeper')) {
    Assert-ReleaseCondition ($readme.Contains($requiredText)) "README is missing normal macOS installation guidance: $requiredText"
}
foreach ($requiredText in @('sylvops-linux-x86_64.AppImage', 'sylvops-linux-x86_64.deb', 'AppImage', 'Debian')) {
    Assert-ReleaseCondition ($readme.Contains($requiredText)) "README is missing normal Linux installation guidance: $requiredText"
}

$releasingGuide = Get-Content -Raw -LiteralPath (Join-Path $root 'docs\development\releasing.md')
foreach ($requiredText in @(
    'cargo-packager 0.11.8',
    'scripts/package-linux.sh',
    'scripts/test-linux-packages.sh',
    'sylvops-linux-x86_64.AppImage',
    'sylvops-linux-x86_64.deb',
    'WINDOWS_SIGNING_CERTIFICATE_BASE64',
    'WINDOWS_SIGNING_CERTIFICATE_PASSWORD',
    'WINDOWS_SIGNING_TIMESTAMP_URL',
    '%LOCALAPPDATA%\Programs\SylvOps',
    '%LOCALAPPDATA%\SylvOps',
    '%APPDATA%\SylvOps',
    'MACOS_SIGNING_CERTIFICATE_BASE64',
    'MACOS_SIGNING_CERTIFICATE_PASSWORD',
    'MACOS_SIGNING_IDENTITY',
    'MACOS_NOTARY_KEY_BASE64',
    'MACOS_NOTARY_KEY_ID',
    'MACOS_NOTARY_ISSUER_ID'
)) {
    Assert-ReleaseCondition ($releasingGuide.Contains($requiredText)) "Release guide is missing protected package guidance: $requiredText"
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
    'sylvops-linux-x86_64.AppImage',
    'sylvops-linux-x86_64.deb',
    'sylvops-macos-x86_64.tar.gz',
    'sylvops-macos-aarch64.tar.gz',
    'sylvops-macos-x86_64.dmg',
    'sylvops-macos-aarch64.dmg',
    'sylvops-linux-x86_64.AppImage',
    'sylvops-linux-x86_64.deb',
    'sylvops-update-windows-x86_64-nsis.json',
    'sylvops-update-macos-x86_64-dmg.json',
    'sylvops-update-macos-aarch64-dmg.json',
    'sylvops-update-linux-x86_64-appimage.json',
    'sylvops-update-linux-x86_64-deb.json',
    'RELEASE-EVIDENCE.json'
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
Assert-ReleaseCondition ($release.Contains('scripts/test-windows-native-upgrade.ps1')) "Release packaging does not run the native Windows N-1 upgrade test."
Assert-ReleaseCondition ($release.Contains('windows-upgrade-failed-health.json')) "Release packaging does not exercise a signed failed-health rollback."
Assert-ReleaseCondition ($release.Contains('sylvops-windows-x86_64-setup.exe')) "Release packaging does not publish the Windows installer asset."
Assert-ReleaseCondition ($release.Contains('MACOS_SIGNING_CERTIFICATE_BASE64')) "Release packaging does not load the protected macOS signing certificate."
Assert-ReleaseCondition ($release.Contains('MACOS_SIGNING_CERTIFICATE_PASSWORD')) "Release packaging does not load the protected macOS signing certificate password."
Assert-ReleaseCondition ($release.Contains('MACOS_SIGNING_IDENTITY')) "Release packaging does not select the Developer ID signing identity."
Assert-ReleaseCondition ($release.Contains('MACOS_NOTARY_KEY_BASE64')) "Release packaging does not load the protected notarization key."
Assert-ReleaseCondition ($release.Contains('MACOS_NOTARY_KEY_ID')) "Release packaging does not load the notarization key ID."
Assert-ReleaseCondition ($release.Contains('MACOS_NOTARY_ISSUER_ID')) "Release packaging does not load the notarization issuer ID."
Assert-ReleaseCondition ($release.Contains('scripts/package-macos.sh')) "Release packaging does not build the signed and notarized macOS DMGs."
Assert-ReleaseCondition ($release.Contains('scripts/test-macos-package.sh')) "Release packaging does not run the native macOS package smoke test."
Assert-ReleaseCondition ($release.Contains('scripts/package-linux.sh')) "Release packaging does not build the AppImage and deb packages."
Assert-ReleaseCondition ($release.Contains('scripts/test-linux-packages.sh')) "Release packaging does not run the native Linux package smoke test."
Assert-ReleaseCondition ($release.Contains('needs: [package-windows, package-linux, package-macos]')) "Release staging is not gated on the native Linux package job."
Assert-ReleaseCondition ([regex]::Matches($release, 'chmod 755 dist/sylvops-linux-x86_64\.AppImage').Count -eq 2) "Release staging and publication do not restore the AppImage executable permission after artifact transport."
Assert-ReleaseCondition ($release.Contains('UPDATE_SIGNING_PRIVATE_KEY_BASE64')) "Release staging does not load the protected application-update signing key."
Assert-ReleaseCondition ($release.Contains('UPDATE_SIGNING_PUBLIC_KEY_BASE64')) "Release builds do not embed the application-update verification key."
Assert-ReleaseCondition ($release.Contains('--bin release-manifest -- generate')) "Release staging does not create signed update manifests."
Assert-ReleaseCondition ($release.Contains('--bin release-manifest -- verify')) "Release staging does not verify signed update manifests."
Assert-NoLegacyReleasePhaseWording -Path '.github\workflows\release.yml' -Text $release

$ciInstallerJob = Get-Content -Raw -LiteralPath (Join-Path $root '.github\workflows\ci.yml')
Assert-ReleaseCondition ($ciInstallerJob.Contains('windows-installer')) "CI is missing the clean native Windows installer job."
Assert-ReleaseCondition ($ciInstallerJob.Contains('New-SelfSignedCertificate')) "CI does not create an isolated test signing identity for installer verification."
Assert-ReleaseCondition ($ciInstallerJob.Contains('scripts/package-windows-installer.ps1')) "CI does not exercise the production Windows packaging script."
Assert-ReleaseCondition ($ciInstallerJob.Contains('scripts/test-windows-installer.ps1')) "CI does not install, launch, verify, and uninstall the Windows package."
Assert-ReleaseCondition ($ciInstallerJob.Contains('macos-package')) "CI is missing the native macOS package matrix."
Assert-ReleaseCondition ($ciInstallerJob.Contains('macos-15-intel')) "CI is missing the native Intel macOS package job."
Assert-ReleaseCondition ($ciInstallerJob.Contains('scripts/package-macos.sh')) "CI does not exercise the production macOS packaging script."
Assert-ReleaseCondition ($ciInstallerJob.Contains('scripts/test-macos-package.sh')) "CI does not install, launch, verify, and remove the macOS package."
Assert-ReleaseCondition ($ciInstallerJob.Contains('linux-package')) "CI is missing the native Linux package job."
Assert-ReleaseCondition ($ciInstallerJob.Contains('xvfb')) "CI does not provide a virtual display for the Linux desktop launch test."
Assert-ReleaseCondition ($ciInstallerJob.Contains('scripts/package-linux.sh')) "CI does not exercise the production Linux packaging script."
Assert-ReleaseCondition ($ciInstallerJob.Contains('scripts/test-linux-packages.sh')) "CI does not install, launch, verify, and remove the Linux packages."

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

foreach ($packagePath in @($MacosPackagePath | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })) {
    Assert-MacosPackage -Path (Resolve-Path -LiteralPath $packagePath).Path
}

foreach ($packagePath in @($LinuxPackagePath | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })) {
    Assert-LinuxPackage -Path (Resolve-Path -LiteralPath $packagePath).Path
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
    $macosPackages = @(
        'sylvops-macos-x86_64.dmg',
        'sylvops-macos-aarch64.dmg'
    )
    $linuxPackages = @(
        'sylvops-linux-x86_64.AppImage',
        'sylvops-linux-x86_64.deb'
    )
    $updateManifests = @{
        'sylvops-update-windows-x86_64-nsis.json' = 'sylvops-windows-x86_64-setup.exe'
        'sylvops-update-macos-x86_64-dmg.json' = 'sylvops-macos-x86_64.dmg'
        'sylvops-update-macos-aarch64-dmg.json' = 'sylvops-macos-aarch64.dmg'
        'sylvops-update-linux-x86_64-appimage.json' = 'sylvops-linux-x86_64.AppImage'
        'sylvops-update-linux-x86_64-deb.json' = 'sylvops-linux-x86_64.deb'
    }
    $evidence = 'RELEASE-EVIDENCE.json'
    $releaseAssets = @($archives + $installer + $macosPackages + $linuxPackages + @($updateManifests.Keys) + $evidence)
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
        elseif ($asset -in $macosPackages) {
            Assert-MacosPackage -Path $path
        }
        elseif ($asset -in $linuxPackages) {
            Assert-LinuxPackage -Path $path
        }
        elseif ($updateManifests.ContainsKey($asset)) {
            Assert-UpdateManifest -Path $path -AssetPath (Join-Path $dist $updateManifests[$asset]) -ExpectedVersion $version
        }
        elseif ($asset -eq $evidence) {
            Assert-ReleaseEvidence -Path $path -ExpectedVersion $version
        }
        else {
            Assert-Archive -Path $path
        }
        Write-Host "[ok] $asset sha256=$actualHash"
    }
    Write-Host "[ok] release bundle has exactly fifteen validated assets and one checksum manifest"
}
