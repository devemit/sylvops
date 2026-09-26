[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string[]]$Path,
    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9A-Fa-f]{40}$')]
    [string]$CertificateThumbprint,
    [string]$TimestampUrl,
    [switch]$RequireTrustedSignature,
    [switch]$VerifyOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Find-SignTool {
    $kitsBin = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Windows Kits\10\bin'
    if (-not (Test-Path -LiteralPath $kitsBin -PathType Container)) {
        throw 'The Windows 10 SDK bin directory was not found.'
    }

    $resolvedKitsBin = (Resolve-Path -LiteralPath $kitsBin).Path.TrimEnd('\') + '\'
    $candidates = @(
        Get-ChildItem -LiteralPath $resolvedKitsBin -Filter signtool.exe -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Directory.Name -eq 'x64' } |
            Sort-Object FullName -Descending
    )
    foreach ($candidate in $candidates) {
        $resolvedCandidate = (Resolve-Path -LiteralPath $candidate.FullName).Path
        if (-not $resolvedCandidate.StartsWith($resolvedKitsBin, [StringComparison]::OrdinalIgnoreCase)) {
            continue
        }
        $signature = Get-AuthenticodeSignature -LiteralPath $resolvedCandidate
        $publisher = if ($null -eq $signature.SignerCertificate) {
            ''
        }
        else {
            $signature.SignerCertificate.GetNameInfo([Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false)
        }
        if ($signature.Status -eq 'Valid' -and $publisher -like 'Microsoft*') {
            return $resolvedCandidate
        }
    }
    throw 'No trusted Microsoft x64 signtool.exe was found under the canonical Windows 10 SDK directory.'
}

$signTool = if ($VerifyOnly) { $null } else { Find-SignTool }
$expectedThumbprint = $CertificateThumbprint.ToUpperInvariant()

foreach ($target in $Path) {
    $resolved = (Resolve-Path -LiteralPath $target).Path
    if (-not $VerifyOnly) {
        $arguments = @('sign', '/sha1', $expectedThumbprint, '/fd', 'SHA256')
        if (-not [string]::IsNullOrWhiteSpace($TimestampUrl)) {
            $arguments += @('/tr', $TimestampUrl, '/td', 'SHA256')
        }
        $arguments += $resolved

        & $signTool @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "signtool failed for $resolved."
        }
    }

    $signature = Get-AuthenticodeSignature -LiteralPath $resolved
    if ($null -eq $signature.SignerCertificate) {
        throw "Signed file has no signer certificate: $resolved"
    }
    if ($signature.SignerCertificate.Thumbprint.ToUpperInvariant() -ne $expectedThumbprint) {
        throw "Signed file uses an unexpected certificate: $resolved"
    }
    if ($signature.Status -in @('HashMismatch', 'NotSigned')) {
        throw "Authenticode validation failed for $resolved with status $($signature.Status)."
    }
    if ($RequireTrustedSignature -and $signature.Status -ne 'Valid') {
        throw "Trusted Authenticode validation failed for $resolved with status $($signature.Status): $($signature.StatusMessage)"
    }
    if (-not [string]::IsNullOrWhiteSpace($TimestampUrl) -and $null -eq $signature.TimeStamperCertificate) {
        throw "The Authenticode signature for $resolved is missing its required RFC 3161 timestamp."
    }

    Write-Host "[ok] Authenticode signature verified for $resolved"
}
