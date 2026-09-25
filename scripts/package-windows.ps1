[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$releaseDirectory = Join-Path $repositoryRoot "target\release"
$packageDirectory = Join-Path $repositoryRoot "dist\sylvops-windows-x86_64"
$archivePath = Join-Path $repositoryRoot "dist\sylvops-windows-x86_64.zip"

& cargo build --release --locked -p sylvops-cli
if ($LASTEXITCODE -ne 0) {
    throw "Failed to compile SylvOps with the native MSVC toolchain."
}

if (Test-Path -LiteralPath $packageDirectory) {
    Remove-Item -LiteralPath $packageDirectory -Recurse -Force
}
New-Item -ItemType Directory -Path $packageDirectory -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $releaseDirectory "sylvops.exe") -Destination $packageDirectory -Force
Copy-Item -LiteralPath (Join-Path $repositoryRoot "packaging\windows\Start-SylvOps.ps1") -Destination $packageDirectory -Force
Copy-Item -LiteralPath (Join-Path $repositoryRoot "packaging\windows\Start SylvOps.cmd") -Destination $packageDirectory -Force
Copy-Item -LiteralPath (Join-Path $repositoryRoot "packaging\windows\QUICKSTART.txt") -Destination $packageDirectory -Force
Copy-Item -LiteralPath (Join-Path $repositoryRoot "README.md") -Destination $packageDirectory -Force
Copy-Item -LiteralPath (Join-Path $repositoryRoot "LICENSE") -Destination $packageDirectory -Force
Compress-Archive -Path (Join-Path $packageDirectory "*") -DestinationPath $archivePath -Force

& (Join-Path $PSScriptRoot "validate-release.ps1") -ArchivePath $archivePath

Write-Host "Created $archivePath"
