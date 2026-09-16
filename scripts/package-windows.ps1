[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$builderImage = "sylvops-windows-builder:bookworm"
$targetDirectory = Join-Path $repositoryRoot "target\windows-gnu-container"
$releaseDirectory = Join-Path $targetDirectory "x86_64-pc-windows-gnu\release"
$packageDirectory = Join-Path $repositoryRoot "dist\sylvops-windows-x64"
$archivePath = Join-Path $repositoryRoot "dist\sylvops-windows-x64.zip"

$dockerBuildArguments = @(
    "build",
    "--file", (Join-Path $repositoryRoot "packaging\windows\Dockerfile"),
    "--tag", $builderImage,
    $repositoryRoot
)
& docker @dockerBuildArguments
if ($LASTEXITCODE -ne 0) {
    throw "Failed to build the Windows cross-compilation image."
}

$dockerRunArguments = @(
    "run", "--rm",
    "--volume", "${repositoryRoot}:/workspace",
    "--volume", "${targetDirectory}:/target",
    "--volume", "${env:USERPROFILE}\.cargo\registry:/usr/local/cargo/registry",
    $builderImage
)
& docker @dockerRunArguments
if ($LASTEXITCODE -ne 0) {
    throw "Failed to cross-compile SylvOps for Windows."
}

New-Item -ItemType Directory -Path $packageDirectory -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $releaseDirectory "sylvops.exe") -Destination $packageDirectory -Force
Copy-Item -LiteralPath (Join-Path $repositoryRoot "packaging\windows\Start-SylvOps.ps1") -Destination $packageDirectory -Force
Copy-Item -LiteralPath (Join-Path $repositoryRoot "packaging\windows\Start SylvOps.cmd") -Destination $packageDirectory -Force
Copy-Item -LiteralPath (Join-Path $repositoryRoot "packaging\windows\QUICKSTART.txt") -Destination $packageDirectory -Force
Compress-Archive -Path (Join-Path $packageDirectory "*") -DestinationPath $archivePath -Force

Write-Host "Created $archivePath"
