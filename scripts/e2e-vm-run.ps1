<#
.SYNOPSIS
    Runs the Tier 2 E2E scenarios in the test VM and brings the result back.

.DESCRIPTION
    One command for the pre-release smoke test: stage the payload, ship it to
    the VM over PowerShell Direct, make sure the IME is registered and the
    engine is up, run the harness IN THE VM'S INTERACTIVE SESSION, then copy
    the log back and report pass/fail.

    The interactive session is the whole reason this is not a plain
    Invoke-Command. The harness synthesises keystrokes; a command started
    through PowerShell Direct runs in session 0, where those keystrokes reach
    nothing. So the harness is launched as a scheduled task whose principal is
    the user logged on at the VM console, which puts it on that desktop.

    ASCII ONLY, deliberately. Windows PowerShell 5.1 reads a BOM-less UTF-8
    script as CP932, and a decision that hangs off a mangled non-ASCII string
    fails in a way that looks like a test failure. Every string this script
    matches on is ASCII; the harness's own Japanese output is passed through
    untouched.

.PARAMETER VMName
    Hyper-V guest to run in. Default: azookey-e2e.

.PARAMETER Credential
    An account in the VM. Prompted for when omitted.

.PARAMETER Release
    Stage and run the release build instead of debug.

.PARAMETER SkipPayload
    Reuse the existing %TEMP%\azookey-e2e.zip instead of rebuilding it.

.PARAMETER TimeoutMinutes
    How long to wait for the harness before giving up. Default: 20.

.EXAMPLE
    pwsh -File scripts/e2e-vm-run.ps1
.EXAMPLE
    pwsh -File scripts/e2e-vm-run.ps1 -Release -VMName azookey-e2e
#>
[CmdletBinding()]
param(
    [string]$VMName = 'azookey-e2e',
    [pscredential]$Credential,
    [switch]$Release,
    [switch]$SkipPayload,
    [int]$TimeoutMinutes = 20
)

$ErrorActionPreference = 'Stop'

# Everything the VM side touches lives under one directory, so a failed run
# can be inspected (and wiped) without guessing where things landed.
$VmRoot = 'C:\azookey-e2e'
$VmPayload = "$VmRoot\payload"
$VmLog = "$VmRoot\e2e.log"
$TaskName = 'azookey-e2e-run'

function Write-Step {
    param([string]$Text)
    Write-Host ''
    Write-Host "== $Text" -ForegroundColor Cyan
}

function Fail {
    param([string]$Text)
    throw $Text
}

# ---------------------------------------------------------------- host side

$repo = Split-Path -Parent $PSScriptRoot
$zip = Join-Path $env:TEMP 'azookey-e2e.zip'

if (-not $SkipPayload) {
    Write-Step 'Staging the payload (cargo make e2e_payload)'
    Push-Location $repo
    try {
        if ($Release) { cargo make e2e_payload --release } else { cargo make e2e_payload }
        if ($LASTEXITCODE -ne 0) { Fail "cargo make e2e_payload failed with exit code $LASTEXITCODE" }
    }
    finally { Pop-Location }
}

if (-not (Test-Path $zip)) {
    Fail "payload not found at $zip (drop -SkipPayload to build it)"
}
$zipMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "payload: $zip ($zipMb MB)"

Write-Step "Connecting to $VMName"
$vm = Get-VM -Name $VMName -ErrorAction SilentlyContinue
if ($null -eq $vm) { Fail "no Hyper-V guest named $VMName on this host" }
if ($vm.State -ne 'Running') { Fail "$VMName is $($vm.State); start it and log on at its console first" }

if ($null -eq $Credential) { $Credential = Get-Credential -Message "Account inside $VMName" }
$session = New-PSSession -VMName $VMName -Credential $Credential

try {
    # The precondition that cannot be worked around: somebody has to be logged
    # on at the VM console, or there is no interactive session to type into.
    $consoleUser = Invoke-Command -Session $session -ScriptBlock {
        (Get-CimInstance Win32_ComputerSystem).UserName
    }
    if ([string]::IsNullOrWhiteSpace($consoleUser)) {
        Fail "nobody is logged on at the $VMName console; the harness needs an interactive desktop"
    }
    Write-Host "console user: $consoleUser"

    Write-Step 'Copying the payload into the VM'
    Invoke-Command -Session $session -ScriptBlock {
        param($root, $payload)
        Remove-Item $payload -Recurse -Force -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force -Path $root | Out-Null
    } -ArgumentList $VmRoot, $VmPayload
    Copy-Item -Path $zip -Destination "$VmRoot\payload.zip" -ToSession $session -Force

    Write-Step 'Unpacking and preparing the IME'
    $prep = Invoke-Command -Session $session -ScriptBlock {
        param($root, $payload)
        $ErrorActionPreference = 'Stop'
        Expand-Archive -Path "$root\payload.zip" -DestinationPath $payload -Force

        # Re-register unconditionally: the DLL was just overwritten, and
        # regsvr32 on an already-registered path is a no-op that refreshes it.
        # Both bitnesses, because a 32-bit host app loads the x86 TIP.
        $reg = @()
        foreach ($dll in @("$payload\azookey_windows.dll", "$payload\x86\azookey_windows.dll")) {
            if (Test-Path $dll) {
                $p = Start-Process regsvr32.exe -ArgumentList '/s', "`"$dll`"" -Wait -PassThru
                $reg += "$dll -> $($p.ExitCode)"
            }
            else { $reg += "$dll -> MISSING" }
        }

        # One launcher, one server. The harness refuses to run next to a
        # duplicated supervisor, so clear the old one out before starting.
        Get-Process launcher, azookey-server, ui -ErrorAction SilentlyContinue |
            Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
        Start-Process "$payload\launcher.exe" -WorkingDirectory $payload
        Start-Sleep -Seconds 5

        [pscustomobject]@{
            Register = $reg
            Engine   = @(Get-Process azookey-server -ErrorAction SilentlyContinue).Count
            Ui       = @(Get-Process ui -ErrorAction SilentlyContinue).Count
        }
    } -ArgumentList $VmRoot, $VmPayload

    $prep.Register | ForEach-Object { Write-Host "regsvr32: $_" }
    Write-Host "azookey-server processes: $($prep.Engine)"
    Write-Host "ui processes: $($prep.Ui)"
    if ($prep.Engine -lt 1) { Fail 'the conversion server did not come up; check the VM console' }

    Write-Step 'Running the scenarios in the interactive session'
    Invoke-Command -Session $session -ScriptBlock {
        param($payload, $log, $taskName, $user)
        $ErrorActionPreference = 'Stop'
        Remove-Item $log -Force -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue

        # cmd.exe only as the redirection wrapper: the harness writes its
        # report to stdout, and a scheduled task has nowhere else to put it.
        $action = New-ScheduledTaskAction -Execute 'cmd.exe' `
            -Argument "/c `"`"$payload\azookey-e2e.exe`" > `"$log`" 2>&1`"" `
            -WorkingDirectory $payload
        # Interactive + Highest: session 0 cannot receive synthesised keys, and
        # the harness restarts the supervised engine.
        $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings | Out-Null
        Start-ScheduledTask -TaskName $taskName
    } -ArgumentList $VmPayload, $VmLog, $TaskName, $consoleUser

    $deadline = (Get-Date).AddMinutes($TimeoutMinutes)
    $state = 'Running'
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 10
        $state = Invoke-Command -Session $session -ScriptBlock {
            param($taskName)
            (Get-ScheduledTask -TaskName $taskName).State
        } -ArgumentList $TaskName
        Write-Host "." -NoNewline
        if ($state -ne 'Running') { break }
    }
    Write-Host ''
    if ($state -eq 'Running') {
        Fail "the harness was still running after $TimeoutMinutes minutes; look at the VM console, then read $VmLog"
    }

    $taskResult = Invoke-Command -Session $session -ScriptBlock {
        param($taskName)
        (Get-ScheduledTaskInfo -TaskName $taskName).LastTaskResult
    } -ArgumentList $TaskName

    Write-Step 'Collecting the log'
    $localLog = Join-Path $env:TEMP 'azookey-e2e.log'
    Remove-Item $localLog -Force -ErrorAction SilentlyContinue
    Copy-Item -Path $VmLog -Destination $localLog -FromSession $session -ErrorAction SilentlyContinue
    if (-not (Test-Path $localLog)) {
        Fail "the harness produced no log; it may not have started in the interactive session"
    }

    # The harness prints in Japanese; read it as UTF-8 so the pass-through is
    # legible, but decide only on its ASCII markers.
    $report = Get-Content -Path $localLog -Encoding UTF8
    Write-Host ''
    $report | ForEach-Object { Write-Host $_ }

    $verdict = $report | Where-Object { $_ -match '^RESULT: ' } | Select-Object -Last 1
    $tally = $report | Where-Object { $_ -match '^[0-9]+/[0-9]+ passed' } | Select-Object -Last 1

    Write-Host ''
    Write-Host "log: $localLog"
    Write-Host "task exit code: $taskResult"
    Write-Host "tally: $tally"

    if ($verdict -eq 'RESULT: pass' -and $taskResult -eq 0) {
        Write-Host 'E2E SMOKE: PASS' -ForegroundColor Green
        exit 0
    }
    Write-Host 'E2E SMOKE: FAIL' -ForegroundColor Red
    exit 1
}
finally {
    if ($session) {
        Invoke-Command -Session $session -ScriptBlock {
            param($taskName)
            Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
        } -ArgumentList $TaskName -ErrorAction SilentlyContinue
        Remove-PSSession $session
    }
}
