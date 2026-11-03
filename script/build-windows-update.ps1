#Requires -Version 7.0
param(
    [ValidateRange(1, 64)]
    [int]$Jobs = 4,
    [switch]$Offline
)
$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$previousTarget = $env:CARGO_TARGET_DIR
$previousPath = $env:PATH
$previousIncremental = $env:CARGO_INCREMENTAL
Push-Location $repository
try {
    # Keep the same cache across installer versions and working directories.
    $env:CARGO_TARGET_DIR = Join-Path $repository 'target'
    $env:CARGO_INCREMENTAL = '1'
    $cmake = 'C:/Program Files/Microsoft Visual Studio/2022/Community/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin'
    if (Test-Path -LiteralPath $cmake) { $env:PATH = "$cmake;$env:PATH" }
    $arguments = @('build', '--profile', 'release-fast', '-p', 'zed', '--bin', 'zed', '-p', 'cli', '--bin', 'cli', '--locked', '--timings', '-j', "$Jobs")
    if ($Offline) { $arguments += '--offline' }
    & cargo @arguments
    if ($LASTEXITCODE -ne 0) { throw "Release build failed with exit code $LASTEXITCODE" }
} finally {
    $env:CARGO_TARGET_DIR = $previousTarget
    $env:CARGO_INCREMENTAL = $previousIncremental
    $env:PATH = $previousPath
    Pop-Location
}
