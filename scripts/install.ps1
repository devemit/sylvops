[CmdletBinding()]
param(
    [string]$Version = "0.1.0",
    [string]$InstallDirectory = "$env:LOCALAPPDATA\SylvOps\bin"
)

$ErrorActionPreference = "Stop"
$asset = "sylvops-windows-x86_64.zip"
$base = "https://github.com/devemit/sylvops/releases/download/v$Version"
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("sylvops-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    Invoke-WebRequest -Uri "$base/$asset" -OutFile (Join-Path $temporary $asset)
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile (Join-Path $temporary "SHA256SUMS")
    $expectedLine = Get-Content -LiteralPath (Join-Path $temporary "SHA256SUMS") |
        Where-Object { $_ -match [regex]::Escape($asset) } |
        Select-Object -First 1
    if ($null -eq $expectedLine) { throw "Release checksum does not include $asset." }
    $expected = ($expectedLine -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $temporary $asset)).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { throw "SylvOps archive checksum mismatch." }
    $expanded = Join-Path $temporary "expanded"
    Expand-Archive -LiteralPath (Join-Path $temporary $asset) -DestinationPath $expanded
    New-Item -ItemType Directory -Path $InstallDirectory -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $expanded "sylvops.exe") -Destination $InstallDirectory -Force
    Write-Host "Installed SylvOps to $InstallDirectory"
    Write-Host "Add this directory to PATH yourself, then run: sylvops open ."
} finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
}
