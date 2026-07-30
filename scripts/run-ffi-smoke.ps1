<#
.SYNOPSIS
    Runs the --ignored FFI smoke tests against the freshly built engine.

.DESCRIPTION
    These tests talk to a live azookey-server over the session's named pipe,
    and that pipe is owned by whichever server is running. So the installed
    stack has to come down first, the build/ one goes up in its place, and the
    installed one goes back afterwards.

    MUST BE RUN ELEVATED. launcher.exe runs as administrator, so a
    non-elevated shell cannot stop it -- Stop-Process fails with "Access is
    denied" and the pipe stays taken.

    The IME is unavailable while this runs (a minute or two). Typing in other
    applications will fall back to whatever other input method is installed.

    Two of the tests rewrite %APPDATA%\Azookey\settings.json and restore it on
    drop; this script also takes its own copy and puts it back at the end, in
    case the test process is killed rather than finishing.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\run-ffi-smoke.ps1
#>
param(
    [string]$Repo = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "run this from an elevated PowerShell: launcher.exe runs as administrator and cannot be stopped otherwise"
}

Set-Location $Repo
$build = Join-Path $Repo 'build'
foreach ($needed in @('azookey-server.exe', 'plugin-host.exe', 'azookey-server.dll', 'llama_cpu\llama.dll')) {
    if (-not (Test-Path (Join-Path $build $needed))) {
        throw "build\$needed is missing; run ``cargo make build --release`` first"
    }
}
# the artifacts under test have to BE the ones just built, not a stale tree
$age = (Get-Date) - (Get-Item (Join-Path $build 'azookey-server.dll')).LastWriteTime
Write-Host ("engine DLL built {0:N0} minutes ago" -f $age.TotalMinutes)

$settings = Join-Path $env:APPDATA 'Azookey\settings.json'
$saved = Join-Path $env:TEMP 'azookey-settings-before-smoke.json'
if (Test-Path $settings) { Copy-Item $settings $saved -Force }

$started = @()
try {
    Write-Host '== stopping the installed stack'
    schtasks /End /TN "Azookey Startup" 2>&1 | Out-Null
    Get-Process launcher, ui, azookey-server, plugin-host -ErrorAction SilentlyContinue |
        Stop-Process -Force
    Start-Sleep -Seconds 3

    Write-Host '== starting the build/ engine and plugin host'
    # llama.dll is loaded at engine start and is not on PATH
    $env:Path = "$build\llama_cpu;$env:Path"
    $started += Start-Process (Join-Path $build 'azookey-server.exe') -WorkingDirectory $build -PassThru
    $started += Start-Process (Join-Path $build 'plugin-host.exe') -WorkingDirectory $build -PassThru
    # the engine loads the dictionary and warms the converter up before it listens
    Start-Sleep -Seconds 20

    Write-Host '== running the smoke tests'
    cargo test -p azookey-server -- --ignored --test-threads=1
    $result = $LASTEXITCODE
}
finally {
    Write-Host '== restoring'
    foreach ($p in $started) {
        if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    }
    Start-Sleep -Seconds 2
    if (Test-Path $saved) { Copy-Item $saved $settings -Force }
    schtasks /Run /TN "Azookey Startup" 2>&1 | Out-Null
    Start-Sleep -Seconds 3
    $back = @(Get-Process launcher, azookey-server -ErrorAction SilentlyContinue).Count
    Write-Host "installed stack back up: $back of 2 processes"
}

if ($result -ne 0) { throw "the smoke tests failed with exit code $result" }
Write-Host 'smoke tests passed'
