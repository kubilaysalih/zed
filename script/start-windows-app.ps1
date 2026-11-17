#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$FilePath,
    [string]$ArgumentString = '',
    [string]$WorkingDirectory,
    [ValidateSet('Normal', 'Hidden')][string]$WindowStyle = 'Normal'
)
$ErrorActionPreference = 'Stop'
$executable = (Resolve-Path -LiteralPath $FilePath).Path
if (!$WorkingDirectory) { $WorkingDirectory = Split-Path -Parent $executable }
$directory = (Resolve-Path -LiteralPath $WorkingDirectory).Path
$shell = $null
$windows = $null
$desktop = $null
$document = $null
$desktopShell = $null
try {
    $shell = New-Object -ComObject Shell.Application
    $windows = $shell.Windows()
    $location = 0
    $root = 0
    $windowHandle = 0
    $desktop = $windows.FindWindowSW([ref]$location, [ref]$root, 8, [ref]$windowHandle, 1)
    if (!$desktop) { throw 'Windows desktop shell is unavailable; independent launch cancelled.' }
    $document = $desktop.Document
    $desktopShell = $document.Application
    if (!$desktopShell) { throw 'Windows desktop launcher is unavailable.' }
    # The existing Explorer process must own the launch, so the caller's process-tree cleanup cannot kill the app.
    # https://devblogs.microsoft.com/oldnewthing/20131118-00/?p=2643
    $show = if ($WindowStyle -eq 'Hidden') { 0 } else { 1 }
    $desktopShell.ShellExecute($executable, $ArgumentString, $directory, 'open', $show)
} finally {
    foreach ($instance in @($desktopShell, $document, $desktop, $windows, $shell)) {
        if ($null -ne $instance -and [Runtime.InteropServices.Marshal]::IsComObject($instance)) {
            [void][Runtime.InteropServices.Marshal]::ReleaseComObject($instance)
        }
    }
}
