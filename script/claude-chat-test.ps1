#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ClaudePath,
    [ValidatePattern('^[0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}$')]
    [string]$SessionId,
    [switch]$ForkSession,
    [switch]$Check
)

$ErrorActionPreference = 'Stop'

if (!$ClaudePath) {
    $command = Get-Command claude -ErrorAction SilentlyContinue
    if ($command) { $ClaudePath = $command.Source }
}
if (!$ClaudePath -or !(Test-Path -LiteralPath $ClaudePath -PathType Leaf)) {
    throw 'Claude Code was not found. Install it or pass -ClaudePath with its executable path.'
}

$ClaudePath = (Resolve-Path -LiteralPath $ClaudePath).Path
$version = & $ClaudePath --version
if ($LASTEXITCODE -ne 0) { throw 'Could not read the Claude Code version.' }
$helpText = (& $ClaudePath --help) -join "`n"
if ($LASTEXITCODE -ne 0 -or $helpText -notmatch '--resume') {
    throw 'This Claude Code version does not support the session picker. Update it first.'
}
if ($ForkSession -and $helpText -notmatch '--fork-session') {
    throw 'This Claude Code version does not support session forks.'
}

Write-Host "Claude chat test | $version"
Write-Host "Using: $ClaudePath"
Write-Host 'Uses the official Claude Code terminal client and your existing login.'
Write-Host 'The picker shows local Claude Code sessions, not general Claude web chats.'
Write-Host 'Press Ctrl+A in the picker to show sessions from all projects.'
if ($ForkSession) {
    Write-Host 'Fork mode: the selected history becomes a separate chat; later messages do not sync back.'
} else {
    Write-Host 'Resume mode: continue a saved session. Avoid sending concurrent turns from separate clients.'
}
Write-Host ''

if ($Check) {
    $authText = & $ClaudePath auth status --json
    $authExitCode = $LASTEXITCODE
    if ($authExitCode -ne 0) {
        Write-Host 'Claude Code is not signed in. Run claude auth login before testing.'
        exit $authExitCode
    }
    $authStatus = ($authText -join "`n") | ConvertFrom-Json
    if (!$authStatus.loggedIn) { throw 'Claude Code reports that it is not signed in.' }
    Write-Host 'Check passed: Claude Code is signed in. No chat was opened and no message was sent.'
    exit 0
}

$claudeArguments = @('--resume')
if ($SessionId) { $claudeArguments += $SessionId }
if ($ForkSession) { $claudeArguments += '--fork-session' }

# Leave permission prompts and session selection to the official client.
& $ClaudePath @claudeArguments
exit $LASTEXITCODE
