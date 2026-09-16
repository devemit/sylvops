[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$Repository = ".",

    [string]$StateDirectory
)

$ErrorActionPreference = "Stop"
$executable = Join-Path $PSScriptRoot "sylvops.exe"
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "sylvops.exe is missing from this package."
}

$arguments = @()
if (-not [string]::IsNullOrWhiteSpace($StateDirectory)) {
    New-Item -ItemType Directory -Path $StateDirectory -Force | Out-Null
    $arguments += @("--state-dir", (Resolve-Path -LiteralPath $StateDirectory).Path)
}
$arguments += @("open", $Repository)

& $executable @arguments
if ($LASTEXITCODE -ne 0) {
    throw "SylvOps failed. Run 'sylvops doctor' for redacted diagnostics."
}
