#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$CodexPath,
    [ValidatePattern('^[0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}$')]
    [string]$ThreadId,
    [switch]$Check
)

$ErrorActionPreference = 'Stop'

if (!$CodexPath) {
    # Prefer the desktop app's running binary so newer session formats are supported.
    $desktopRoot = Join-Path $env:LOCALAPPDATA 'OpenAI/Codex/bin'
    $CodexPath = Get-Process -Name codex -ErrorAction SilentlyContinue |
        ForEach-Object { $_.Path } |
        Where-Object { $_ -and $_.StartsWith($desktopRoot + '\', [StringComparison]::OrdinalIgnoreCase) } |
        Select-Object -First 1

    if (!$CodexPath -and (Test-Path -LiteralPath $desktopRoot)) {
        $CodexPath = Get-ChildItem -LiteralPath $desktopRoot -Directory |
            Sort-Object LastWriteTime -Descending |
            ForEach-Object { Join-Path $_.FullName 'codex.exe' } |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
            Select-Object -First 1
    }

    if (!$CodexPath) {
        $command = Get-Command codex -ErrorAction SilentlyContinue
        if ($command) { $CodexPath = $command.Source }
    }
}

if (!$CodexPath -or !(Test-Path -LiteralPath $CodexPath -PathType Leaf)) {
    throw 'Codex was not found. Install Codex CLI or pass -CodexPath with its executable path.'
}

$CodexPath = (Resolve-Path -LiteralPath $CodexPath).Path
$version = & $CodexPath --version
if ($LASTEXITCODE -ne 0) { throw 'Could not read the Codex version.' }
$resumeHelp = (& $CodexPath resume --help) -join "`n"
if ($LASTEXITCODE -ne 0 -or $resumeHelp -notmatch '--all') {
    throw 'This Codex version does not support the session picker. Update Codex first.'
}

Write-Host "Codex chat test | $version"
Write-Host "Using: $CodexPath"
Write-Host 'Uses your existing local Codex profile. No credentials are copied.'
Write-Host 'These are Codex sessions, not general ChatGPT conversations.'
Write-Host 'This test runs a separate Codex server; it does not share the Desktop connection.'
Write-Host 'An idle chat can still be locked by another app. Finishing its turn is not enough.'
Write-Host 'On the lock screen: R retries after the other app releases it; F forks a separate chat.'
Write-Host 'A fork keeps the saved context but does not sync new messages with the original chat.'
Write-Host ''

if ($Check) {
    Write-Host 'Check passed. No conversation was resumed and no message was sent.'
    exit 0
}

$codexArguments = @('resume', '--all', '--no-alt-screen')
if ($resumeHelp -match '--no-daemon') {
    # Desktop bundles do not include the standalone CLI daemon package.
    $codexArguments += '--no-daemon'
}
if ($resumeHelp -match '--include-non-interactive') {
    $codexArguments += '--include-non-interactive'
}
if ($ThreadId) { $codexArguments += $ThreadId }

# The user chooses a session and sends messages in the official terminal UI.
# Do not force a model, working directory, or permission policy on an existing session.
& $CodexPath @codexArguments
exit $LASTEXITCODE
