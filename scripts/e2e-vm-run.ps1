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

.PARAMETER CredentialPath
    A credential saved with Export-Clixml, used instead of prompting. Windows
    encrypts it with DPAPI under the account that saved it, so the file is
    useless to anyone else and the password never appears in it in the clear.
    Create it once with:

        Get-Credential | Export-Clixml $env:USERPROFILE\azookey-e2e.cred

    This is what makes an unattended run possible (a scheduled release check,
    or an agent driving the script), since Get-Credential needs a console.

.PARAMETER Release
    Stage and run the release build instead of debug.

.PARAMETER SkipPayload
    Reuse the existing %TEMP%\azookey-e2e.zip instead of rebuilding it.

.PARAMETER Watchdog
    Run the watchdog scenario instead of the other nine.

    The harness splits the suite on purpose: watchdog_restarts_hung_server
    needs azookey-server.exe started with AZOOKEY_TEST_HANG_AFTER_SECS, which
    launcher only passes on if it inherited it, and an armed server would hang
    partway through every other scenario. So a full smoke is TWO runs of this
    script: the default one (9 scenarios) and -Watchdog (1).

.PARAMETER HangAfterSeconds
    With -Watchdog, how long after each server start the hang fires. Default:
    20. The scenario then waits out the watchdog's own detection budget (90s)
    on top of this, so leave -TimeoutMinutes room for both.

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
    [string]$CredentialPath,
    [switch]$Release,
    [switch]$SkipPayload,
    [switch]$Watchdog,
    [int]$HangAfterSeconds = 20,
    [int]$TimeoutMinutes = 20
)

$ErrorActionPreference = 'Stop'

# Everything the VM side touches lives under one directory, so a failed run
# can be inspected (and wiped) without guessing where things landed.
$VmRoot = 'C:\azookey-e2e'

# A FRESH directory per run, never a fixed one that gets wiped and refilled.
# The TIP DLL is registered out of this directory, and a registered TIP is
# loaded into every process that uses the IME, explorer.exe included. Those
# processes pin the file for as long as they live, and explorer is not
# something a test run gets to kill. Deleting the directory therefore fails
# for reasons that have nothing to do with anything being wrong.
#
# Unpacking beside the old copy sidesteps the lock entirely: re-registering
# points the CLSID at the new path, so every process the harness starts loads
# the new DLL. Old directories are pruned best-effort at the end; whichever
# ones are still pinned simply wait for the next reboot.
$VmPayload = "$VmRoot\payload-" + (Get-Date -Format 'yyyyMMdd-HHmmss')
$VmLog = "$VmRoot\e2e.log"
$TaskName = 'azookey-e2e-run'
$LauncherTaskName = 'azookey-e2e-launcher'

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

if ($null -eq $Credential) {
    if ($CredentialPath) {
        if (-not (Test-Path $CredentialPath)) { Fail "no credential file at $CredentialPath" }
        $Credential = Import-Clixml $CredentialPath
        Write-Host "credential: $CredentialPath ($($Credential.UserName))"
    }
    else {
        # needs a console; pass -CredentialPath for an unattended run
        $Credential = Get-Credential -Message "Account inside $VMName"
    }
}
$session = New-PSSession -VMName $VMName -Credential $Credential

try {
    # The precondition that cannot be worked around: somebody has to be logged
    # on to an interactive desktop, or there is nothing to type into.
    #
    # Found through explorer.exe rather than Win32_ComputerSystem.UserName,
    # which reports the CONSOLE session only. A VMConnect enhanced-session
    # logon is an RDP session, so the console reads as unattended and the
    # obvious check calls a perfectly good desktop "nobody logged on".
    # explorer.exe runs once per interactive desktop either way, and its owner
    # is the user whose session a scheduled task will land in.
    $desktops = @(Invoke-Command -Session $session -ScriptBlock {
            $found = @()
            foreach ($p in Get-CimInstance Win32_Process -Filter "Name = 'explorer.exe'") {
                $o = Invoke-CimMethod -InputObject $p -MethodName GetOwner -ErrorAction SilentlyContinue
                if ($o -and $o.User) {
                    $found += [pscustomobject]@{
                        User      = "$($o.Domain)\$($o.User)"
                        SessionId = $p.SessionId
                    }
                }
            }
            $found | Sort-Object User, SessionId -Unique
        })

    if ($desktops.Count -eq 0) {
        Fail "no interactive desktop in $VMName; log on at its console or with VMConnect (enhanced session), then retry"
    }
    $desktopUser = $desktops[0].User
    foreach ($d in $desktops) { Write-Host "desktop: $($d.User) (session $($d.SessionId))" }
    if ($desktops.Count -gt 1) {
        Write-Host "more than one desktop; using $desktopUser" -ForegroundColor Yellow
    }

    # BEFORE the payload directory is touched. The previous run's engine holds
    # DLLs inside it, so deleting first left a half-removed tree that
    # Expand-Archive -Force then tripped over on a file it expected to find.
    Write-Step 'Stopping anything left from a previous run'
    $stopped = Invoke-Command -Session $session -ScriptBlock {
        # A harness left over from an interrupted run is the dangerous one:
        # two of them synthesise keystrokes into the same desktop at once and
        # every scenario reads the other's typing.
        foreach ($t in @('azookey-e2e-run', 'azookey-e2e-launcher')) {
            Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
        }
        # notepad is in the list because the scenarios drive it as a host: a
        # leftover one still has the TIP DLL loaded and locks the file being
        # replaced. Killing it is safe here and nowhere else, which is what
        # the harness's own VM guard is for.
        $killed = @()
        foreach ($n in @('azookey-e2e', 'azookey-e2e-host', 'launcher', 'azookey-server', 'ui', 'notepad')) {
            $procs = @(Get-Process $n -ErrorAction SilentlyContinue)
            if ($procs.Count -gt 0) {
                $killed += "$n x$($procs.Count)"
                $procs | Stop-Process -Force -ErrorAction SilentlyContinue
            }
        }
        Start-Sleep -Seconds 3
        $killed
    }
    if ($stopped) { Write-Host "stopped: $($stopped -join ', ')" }
    else { Write-Host 'nothing was left running' }

    # Set it machine-wide so BOTH scheduled tasks inherit it: the harness
    # selects its scenario from this variable, and launcher has to pass it on
    # to azookey-server.exe for the hang to arm at all.
    #
    # Written on EVERY run, cleared included. A value left behind by an
    # interrupted watchdog run would silently reduce the next normal run to
    # that one scenario, and "9/9 passed" vs "1/1 passed" is easy to skim past.
    $armed = Invoke-Command -Session $session -ScriptBlock {
        param($secs)
        $name = 'AZOOKEY_TEST_HANG_AFTER_SECS'
        if ($secs -gt 0) {
            [Environment]::SetEnvironmentVariable($name, "$secs", 'Machine')
        }
        else {
            [Environment]::SetEnvironmentVariable($name, $null, 'Machine')
        }
        [Environment]::GetEnvironmentVariable($name, 'Machine')
    } -ArgumentList $(if ($Watchdog) { $HangAfterSeconds } else { 0 })

    if ($Watchdog) {
        Write-Host "mode: WATCHDOG (hang hook armed at $armed s; 1 scenario)" -ForegroundColor Yellow
        if ("$armed" -ne "$HangAfterSeconds") { Fail "could not arm the hang hook (read back '$armed')" }
    }
    else {
        Write-Host 'mode: standard (9 scenarios; run -Watchdog separately for the 10th)'
        if ($armed) { Fail "the hang hook is still armed ('$armed'); the run would cover one scenario only" }
    }

    Write-Step "Copying the payload into the VM ($VmPayload)"
    Invoke-Command -Session $session -ScriptBlock {
        param($root, $payload)
        $ErrorActionPreference = 'Stop'
        New-Item -ItemType Directory -Force -Path $root | Out-Null
        # A directory named for this minute: it cannot already exist with
        # anything in it, so there is nothing to clear and nothing to lock.
        New-Item -ItemType Directory -Force -Path $payload | Out-Null
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

        # The launcher is deliberately NOT started here: this runs over
        # PowerShell Direct, i.e. in session 0, and ui.exe draws a WebView2
        # window, which has nowhere to go on session 0's invisible desktop.
        # It goes through the same interactive scheduled task as the harness.
        [pscustomobject]@{
            Register  = $reg
            ExeFound  = (Test-Path "$payload\azookey-e2e.exe")
            HostFound = (Test-Path "$payload\azookey-e2e-host.exe")
            Launcher  = (Test-Path "$payload\launcher.exe")
        }
    } -ArgumentList $VmRoot, $VmPayload

    $prep.Register | ForEach-Object { Write-Host "regsvr32: $_" }
    Write-Host "azookey-e2e.exe present: $($prep.ExeFound)"
    Write-Host "azookey-e2e-host.exe present: $($prep.HostFound)"
    Write-Host "launcher.exe present: $($prep.Launcher)"
    if (-not $prep.ExeFound) { Fail "the harness is missing from $VmPayload; the payload did not unpack" }
    if (-not $prep.Launcher) { Fail "launcher.exe is missing from $VmPayload" }

    Write-Step 'Starting the engine in the interactive session'
    $engineUp = Invoke-Command -Session $session -ScriptBlock {
        param($payload, $taskName, $user)
        $ErrorActionPreference = 'Stop'
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue

        $action = New-ScheduledTaskAction -Execute "$payload\launcher.exe" -WorkingDirectory $payload
        # Highest: the launcher supervises the engine and needs to be able to
        # kill and restart it, which is also what scenarios 5-7 exercise.
        $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings | Out-Null
        Start-ScheduledTask -TaskName $taskName

        # ui.exe stands up a WebView2 stack, far slower than the server, so
        # poll for both rather than sleeping a fixed amount.
        $engine = 0
        $ui = 0
        for ($i = 0; $i -lt 40; $i++) {
            Start-Sleep -Seconds 1
            $engine = @(Get-Process azookey-server -ErrorAction SilentlyContinue).Count
            $ui = @(Get-Process ui -ErrorAction SilentlyContinue).Count
            if ($engine -ge 1 -and $ui -ge 1) { break }
        }
        $info = Get-ScheduledTaskInfo -TaskName $taskName
        [pscustomobject]@{
            Engine     = $engine
            Ui         = $ui
            LastResult = $info.LastTaskResult
            # ToString() here, not on the host: State is an enum, and
            # PowerShell remoting deserializes it to a bare integer. Comparing
            # that to 'Running' on the host silently misreports the state.
            State      = (Get-ScheduledTask -TaskName $taskName).State.ToString()
        }
    } -ArgumentList $VmPayload, $LauncherTaskName, $desktopUser

    Write-Host "launcher task: state $($engineUp.State), last result $($engineUp.LastResult)"
    Write-Host "azookey-server processes: $($engineUp.Engine)"
    Write-Host "ui processes: $($engineUp.Ui)"
    if ($engineUp.Engine -lt 1) {
        Fail "the conversion server did not come up (launcher task result $($engineUp.LastResult)); check the VM console"
    }
    if ($engineUp.Ui -lt 1) {
        Write-Host 'ui.exe is not running; candidate-window scenarios will fail' -ForegroundColor Yellow
    }

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
        Start-Sleep -Seconds 2
        $info = Get-ScheduledTaskInfo -TaskName $taskName
        [pscustomobject]@{
            State       = (Get-ScheduledTask -TaskName $taskName).State.ToString()
            LastResult  = $info.LastTaskResult
            LastRunTime = $info.LastRunTime
        }
    } -ArgumentList $VmPayload, $VmLog, $TaskName, $desktopUser | ForEach-Object {
        Write-Host "task state: $($_.State), last result: $($_.LastResult), last run: $($_.LastRunTime)"
    }

    $deadline = (Get-Date).AddMinutes($TimeoutMinutes)
    $state = 'Running'
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 5
        # ToString() inside the session: the State enum comes back from
        # remoting as a bare integer, and `4 -ne 'Running'` is true (the
        # string will not convert to a number), so the wait fell straight
        # through on a task that was running perfectly well.
        $state = Invoke-Command -Session $session -ScriptBlock {
            param($taskName)
            (Get-ScheduledTask -TaskName $taskName).State.ToString()
        } -ArgumentList $TaskName
        Write-Host "." -NoNewline
        if ($state -ne 'Running') { break }
    }
    Write-Host ''
    if ($state -eq 'Running') {
        Fail "the harness was still running after $TimeoutMinutes minutes; look at the VM console, then read $VmLog"
    }

    # Gathered BEFORE anything can fail: when the harness leaves no log, this
    # is the only evidence of why, and an earlier version threw it away by
    # failing on the missing log first.
    $diag = Invoke-Command -Session $session -ScriptBlock {
        param($taskName, $root, $log)
        $info = Get-ScheduledTaskInfo -TaskName $taskName
        $task = Get-ScheduledTask -TaskName $taskName
        [pscustomobject]@{
            LastResult  = $info.LastTaskResult
            LastRunTime = $info.LastRunTime
            NumMissed   = $info.NumberOfMissedRuns
            Principal   = "$($task.Principal.UserId) / $($task.Principal.LogonType) / $($task.Principal.RunLevel)"
            Action      = "$($task.Actions[0].Execute) $($task.Actions[0].Arguments)"
            LogExists   = (Test-Path $log)
            LogBytes    = if (Test-Path $log) { (Get-Item $log).Length } else { -1 }
            Listing     = @(Get-ChildItem $root -ErrorAction SilentlyContinue |
                    ForEach-Object { "$($_.Name) ($($_.Length))" })
        }
    } -ArgumentList $TaskName, $VmRoot, $VmLog

    Write-Step 'Task outcome'
    Write-Host "last result : $($diag.LastResult) (0 = ok)"
    Write-Host "last run    : $($diag.LastRunTime)"
    Write-Host "missed runs : $($diag.NumMissed)"
    Write-Host "principal   : $($diag.Principal)"
    Write-Host "action      : $($diag.Action)"
    Write-Host "log exists  : $($diag.LogExists) ($($diag.LogBytes) bytes)"
    Write-Host "in $VmRoot  : $($diag.Listing -join ', ')"
    $taskResult = $diag.LastResult

    Write-Step 'Collecting the log'
    $localLog = Join-Path $env:TEMP 'azookey-e2e.log'
    Remove-Item $localLog -Force -ErrorAction SilentlyContinue
    if ($diag.LogExists) {
        # The error is reported, not swallowed: a copy that fails because the
        # harness still holds the file open looks exactly like "no log" from
        # the outside, and that misread cost a debugging round.
        $copyError = $null
        Copy-Item -Path $VmLog -Destination $localLog -FromSession $session `
            -ErrorAction SilentlyContinue -ErrorVariable copyError
        if ($copyError) { Write-Host "copy failed: $($copyError[0].Exception.Message)" -ForegroundColor Yellow }
    }
    if (-not (Test-Path $localLog)) {
        Write-Host ''
        Write-Host "The task ran but wrote no log. Read the values above:" -ForegroundColor Yellow
        Write-Host "  last result 267011 = never ran; 2147942402 = file not found;" -ForegroundColor Yellow
        Write-Host "  267009 = still running; 0 with no log = cmd could not create it." -ForegroundColor Yellow
        Write-Host "The task '$TaskName' was left registered in the VM so you can" -ForegroundColor Yellow
        Write-Host "run it by hand from Task Scheduler and watch what happens." -ForegroundColor Yellow
        Fail "the harness produced no log"
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
    # The task is deliberately LEFT REGISTERED: when a run fails it is the
    # evidence, and it can be started by hand from Task Scheduler on the VM
    # desktop to watch the harness live. Each run unregisters it before
    # registering again, so nothing accumulates.
    #
    # The hang hook is NOT left behind, though. A machine-wide variable that
    # says "hang the engine 20s after it starts" turns any later launch in
    # the VM into a puzzling failure. The next run would clear it anyway;
    # this just means nobody has to run one first.
    if ($session) {
        # Best effort, never fatal: a directory whose DLL some surviving
        # process still holds simply stays until the VM reboots. Keeping the
        # current one means a failed run can still be inspected.
        $freed = Invoke-Command -Session $session -ScriptBlock {
            param($root, $keep)
            $removed = 0
            foreach ($dir in Get-ChildItem $root -Directory -Filter 'payload-*' -ErrorAction SilentlyContinue) {
                if ($dir.FullName -eq $keep) { continue }
                Remove-Item $dir.FullName -Recurse -Force -ErrorAction SilentlyContinue
                if (-not (Test-Path $dir.FullName)) { $removed++ }
            }
            $removed
        } -ArgumentList $VmRoot, $VmPayload -ErrorAction SilentlyContinue
        if ($freed) { Write-Host "pruned $freed old payload director(ies)" }

        if ($Watchdog) {
            Invoke-Command -Session $session -ScriptBlock {
                [Environment]::SetEnvironmentVariable('AZOOKEY_TEST_HANG_AFTER_SECS', $null, 'Machine')
            } -ErrorAction SilentlyContinue
            Write-Host 'hang hook disarmed'
        }
        Remove-PSSession $session
    }
}
