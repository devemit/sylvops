[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9A-Fa-f]{40}$')]
    [string]$CertificateThumbprint,
    [string]$TimestampUrl,
    [switch]$RequireTrustedSignature,
    [switch]$SkipBuild,
    [string]$InputBinaryPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $IsWindows) {
    throw 'The SylvOps Windows installer must be built on a native Windows host.'
}

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$packagerVersion = (Get-Content -Raw -LiteralPath (Join-Path $repositoryRoot 'packaging\cargo-packager.version')).Trim()
$packagerVersionOutput = (& cargo packager --version 2>&1 | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $packagerVersionOutput -notmatch "(?<![0-9])$([regex]::Escape($packagerVersion))(?![0-9])") {
    throw "cargo-packager $packagerVersion is required; found '$packagerVersionOutput'."
}

if ($SkipBuild) {
    if ([string]::IsNullOrWhiteSpace($InputBinaryPath)) {
        $InputBinaryPath = Join-Path $repositoryRoot 'target\release\sylvops.exe'
    }
}
else {
    if (-not [string]::IsNullOrWhiteSpace($InputBinaryPath)) {
        throw 'InputBinaryPath can only be supplied together with SkipBuild.'
    }
    $installerBuildDirectory = Join-Path $repositoryRoot 'target\installer-build'
    & cargo build --release --locked -p sylvops-cli --target-dir $installerBuildDirectory
    if ($LASTEXITCODE -ne 0) {
        throw 'Failed to build the SylvOps release executable.'
    }
    $InputBinaryPath = Join-Path $installerBuildDirectory 'release\sylvops.exe'
}

$packagerInputDirectory = Join-Path $repositoryRoot 'target\installer-input'
if (Test-Path -LiteralPath $packagerInputDirectory) {
    Remove-Item -LiteralPath $packagerInputDirectory -Recurse -Force
}
New-Item -ItemType Directory -Path $packagerInputDirectory -Force | Out-Null
Copy-Item -LiteralPath (Resolve-Path -LiteralPath $InputBinaryPath).Path -Destination (Join-Path $packagerInputDirectory 'sylvops.exe')

$outputDirectory = Join-Path $repositoryRoot 'dist\windows-installer'
$expectedOutputRoot = Join-Path $repositoryRoot 'dist'
$resolvedOutputParent = (Resolve-Path -LiteralPath (Split-Path -Parent $outputDirectory)).Path
if ($resolvedOutputParent -ne $expectedOutputRoot) {
    throw "Refusing to clean unexpected installer output directory: $outputDirectory"
}
if (Test-Path -LiteralPath $outputDirectory) {
    Remove-Item -LiteralPath $outputDirectory -Recurse -Force
}
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null

$signedConfigPath = Join-Path $repositoryRoot ".Packager.signed.$PID.toml"
$signedConfig = Get-Content -Raw -LiteralPath (Join-Path $repositoryRoot 'Packager.toml')
$signedConfig += "`r`ncertificate-thumbprint = `"$($CertificateThumbprint.ToUpperInvariant())`"`r`n"
if (-not [string]::IsNullOrWhiteSpace($TimestampUrl)) {
    if ($TimestampUrl -match '[\r\n"]') {
        throw 'TimestampUrl contains characters that cannot be written safely to the packager configuration.'
    }
    $timestampUri = $null
    if (-not [Uri]::TryCreate($TimestampUrl, [UriKind]::Absolute, [ref]$timestampUri) -or $timestampUri.Scheme -ne 'https') {
        throw 'TimestampUrl must be an absolute HTTPS RFC 3161 endpoint.'
    }
    $signedConfig += "timestamp-url = `"$TimestampUrl`"`r`ntsp = true`r`n"
}
[IO.File]::WriteAllText($signedConfigPath, $signedConfig, [Text.UTF8Encoding]::new($false))

Push-Location $repositoryRoot
try {
    & cargo packager --release --formats nsis --config $signedConfigPath
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo-packager failed to produce the NSIS installer.'
    }
}
finally {
    Pop-Location
    Remove-Item -LiteralPath $signedConfigPath -Force -ErrorAction SilentlyContinue
}

$generatedInstallers = @(Get-ChildItem -LiteralPath $outputDirectory -Filter '*.exe' -File)
if ($generatedInstallers.Count -ne 1) {
    throw "Expected exactly one generated installer, found $($generatedInstallers.Count)."
}

$installerPath = Join-Path $repositoryRoot 'dist\sylvops-windows-x86_64-setup.exe'
if (Test-Path -LiteralPath $installerPath) {
    Remove-Item -LiteralPath $installerPath -Force
}
Move-Item -LiteralPath $generatedInstallers[0].FullName -Destination $installerPath

& (Join-Path $PSScriptRoot 'sign-windows.ps1') `
    -Path (Join-Path $packagerInputDirectory 'sylvops.exe'), $installerPath `
    -CertificateThumbprint $CertificateThumbprint `
    -TimestampUrl $TimestampUrl `
    -RequireTrustedSignature:$RequireTrustedSignature `
    -VerifyOnly

& (Join-Path $PSScriptRoot 'validate-release.ps1') -InstallerPath $installerPath
if ($LASTEXITCODE -ne 0) {
    throw 'The generated Windows installer failed release validation.'
}

Write-Output $installerPath
