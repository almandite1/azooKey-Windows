<#
.SYNOPSIS
Checks the learning feature's on-disk side: that the memory directory exists,
that it is locked down, and that an unelevated process cannot read it.

.DESCRIPTION
Run this TWICE, and the pair is the whole point:

  1. from an ORDINARY (unelevated) PowerShell -- the reads must be denied
  2. from an ELEVATED PowerShell (-Elevated)  -- the same reads must succeed

Every check prints PASS/FAIL/SKIP with an ASCII tag. No Japanese in the
verdict lines and no non-ASCII in any comparison: this runs under Windows
PowerShell 5.1, where console encoding is CP932 and a mis-encoded string
silently fails a match that should have passed.

Nothing here starts, stops or changes the IME. It only looks.

.PARAMETER Elevated
Assert the elevated expectations (reads succeed, ACL is ours) instead of the
unelevated ones (reads denied). The script checks its own token and refuses
if the two disagree, so this cannot be passed by mistake.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\verify-learning.ps1
  powershell -ExecutionPolicy Bypass -File scripts\verify-learning.ps1 -Elevated
#>
param(
    [switch]$Elevated
)

$ErrorActionPreference = 'Continue'

$script:Failures = 0

function Report {
    param([string]$Verdict, [string]$What, [string]$Detail = '')
    if ($Verdict -eq 'FAIL') { $script:Failures++ }
    $line = "[{0}] {1}" -f $Verdict, $What
    if ($Detail) { $line = "$line`n        $Detail" }
    Write-Output $line
}

# --- where things are -------------------------------------------------------

$memoryDir = Join-Path $env:APPDATA 'Azookey\memory'
$settings = Join-Path $env:APPDATA 'Azookey\settings.json'

Write-Output "memory directory: $memoryDir"

# --- the token this is running with ----------------------------------------

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
$isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

Write-Output ("running elevated: {0} (expected: {1})" -f $isAdmin, [bool]$Elevated)
if ($isAdmin -ne [bool]$Elevated) {
    Write-Output ''
    Report 'FAIL' 'the -Elevated switch does not match this token' `
        'Run without -Elevated from an ordinary shell, and with it from an elevated one.'
    exit 1
}
Write-Output ''

# --- 1. the directory exists ------------------------------------------------

# Test-Path on a directory needs no read access to its contents, so this
# answers the same in both shells.
if (Test-Path -LiteralPath $memoryDir -PathType Container) {
    Report 'PASS' 'the memory directory exists'
} else {
    Report 'FAIL' 'the memory directory does not exist' `
        'The server creates it at startup and on every save. Is the IME running, and is learning on?'
    Write-Output ''
    Write-Output "Failures: $script:Failures"
    exit 1
}

# --- 2. the DACL names only SYSTEM and Administrators -----------------------
#
# icacls, not Get-Acl: Get-Acl needs READ_CONTROL, which the DACL below does
# not grant an ordinary token -- so the unelevated run would fail here for the
# right reason but with the wrong message. icacls reports the same denial in a
# way we can distinguish.

$icacls = & icacls.exe $memoryDir 2>&1 | Out-String
$icaclsFailed = $LASTEXITCODE -ne 0

if ($Elevated) {
    if ($icaclsFailed) {
        Report 'FAIL' 'icacls could not read the DACL from an elevated shell' $icacls.Trim()
    } else {
        # Match on SIDs, never on localized group names: "Administrators" is
        # "Administrators" here and something else on a Japanese Windows, and
        # icacls prints whatever the machine calls them.
        $SYSTEM = 'S-1-5-18'
        $ADMINS = 'S-1-5-32-544'
        $USERS = 'S-1-5-32-545'
        $forbidden = @('S-1-1-0', 'S-1-5-11', 'S-1-5-4')  # Everyone, Authenticated Users, Interactive

        # Users is allowed on the DIRECTORY, but only for emptying and
        # removing it. Anything outside this set is a finding -- and on a
        # file, so is ReadData (see the file loop further down).
        $usersMayHave = @(
            'ListDirectory', 'ReadData',        # the same bit; .NET names it by context
            'DeleteSubdirectoriesAndFiles',
            'ReadAttributes', 'WriteAttributes',
            'Delete', 'Synchronize'
        )

        $acl = Get-Acl -LiteralPath $memoryDir -ErrorAction SilentlyContinue
        if ($null -eq $acl) {
            Report 'FAIL' 'Get-Acl returned nothing from an elevated shell' $icacls.Trim()
        } else {
            function SidOf($ace) {
                try { $ace.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value }
                catch { $ace.IdentityReference.Value }
            }

            $granted = @($acl.Access | ForEach-Object { SidOf $_ } | Sort-Object -Unique)

            foreach ($needed in @($SYSTEM, $ADMINS)) {
                if ($granted -notcontains $needed) {
                    Report 'FAIL' "the DACL does not grant $needed, which the server needs" `
                        ("present: {0}" -f ($granted -join ', '))
                }
            }
            foreach ($f in $forbidden) {
                if ($granted -contains $f) {
                    Report 'FAIL' "the DACL grants $f, which defeats the protection"
                }
            }
            $strangers = @($granted | Where-Object { $_ -ne $SYSTEM -and $_ -ne $ADMINS -and $_ -ne $USERS })
            if ($strangers.Count -gt 0) {
                Report 'FAIL' 'the DACL names a principal that has no business here' `
                    ($strangers -join ', ')
            } else {
                Report 'PASS' 'the DACL names only SYSTEM, Administrators and Users'
            }

            # Users is the interesting one: present on purpose, so what
            # matters is the mask, not the presence.
            $userAces = @($acl.Access | Where-Object { (SidOf $_) -eq $USERS })
            if ($userAces.Count -eq 0) {
                Report 'FAIL' 'Users is granted nothing, so the folder cannot be deleted' `
                    'An uninstall would leave something behind that only an admin can remove.'
            } else {
                $rights = @()
                foreach ($ace in $userAces) { $rights += ($ace.FileSystemRights.ToString() -split ',\s*') }
                $rights = @($rights | Sort-Object -Unique)
                $tooMuch = @($rights | Where-Object { $usersMayHave -notcontains $_ })
                if ($tooMuch.Count -gt 0) {
                    Report 'FAIL' 'Users is granted more than deletion on the directory' `
                        ("unexpected: {0}" -f ($tooMuch -join ', '))
                } else {
                    Report 'PASS' 'Users can empty and remove the directory, and no more' `
                        ($rights -join ', ')
                }
            }

            # Protected: no inherited ACEs from %APPDATA%. An inherited ACE is
            # exactly the user-full-control entry this exists to keep out.
            $inherited = @($acl.Access | Where-Object { $_.IsInherited })
            if ($inherited.Count -gt 0) {
                Report 'FAIL' 'the DACL still inherits from %APPDATA%' `
                    ("{0} inherited ACE(s); the DACL must be protected (D:P)" -f $inherited.Count)
            } else {
                Report 'PASS' 'the DACL is protected: nothing is inherited from %APPDATA%'
            }
        }
    }
} else {
    if ($icaclsFailed) {
        Report 'PASS' 'an ordinary shell cannot even read the DACL'
    } else {
        Report 'INFO' 'an ordinary shell can read the DACL' `
            'Not a failure by itself -- READ_CONTROL is not READ_DATA. The read test below is the one that decides.'
        Write-Output $icacls.Trim()
    }
}

# --- 3. the files inside ----------------------------------------------------

$listing = $null
$listError = $null
try {
    $listing = @(Get-ChildItem -LiteralPath $memoryDir -Force -ErrorAction Stop)
} catch {
    $listError = $_.Exception.Message
}

if ($Elevated) {
    if ($null -eq $listing) {
        Report 'FAIL' 'an elevated shell cannot list the memory directory' $listError
    } else {
        Report 'PASS' ("an elevated shell lists the directory ({0} file(s))" -f $listing.Count)
        foreach ($f in $listing) {
            Write-Output ("        {0}  {1} bytes  {2}" -f $f.Name, $f.Length, $f.LastWriteTime)
        }
        if ($listing.Count -eq 0) {
            Report 'INFO' 'the directory is empty' `
                'Nothing has been learned and committed yet. Confirm a conversion with Enter, then run again.'
        }

        # Every file must have picked the DACL up by inheritance. Users may
        # appear here too -- it is what makes the folder deletable -- but on
        # a FILE the bit .NET calls ReadData is the history itself, so that
        # one is the line.
        $fileUsersMayHave = @(
            'ReadAttributes', 'WriteAttributes', 'Delete', 'Synchronize'
        )
        foreach ($f in $listing) {
            $facl = Get-Acl -LiteralPath $f.FullName -ErrorAction SilentlyContinue
            if ($null -eq $facl) { continue }
            foreach ($ace in $facl.Access) {
                try {
                    $sid = $ace.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
                } catch {
                    $sid = $ace.IdentityReference.Value
                }
                if ($sid -eq 'S-1-5-18' -or $sid -eq 'S-1-5-32-544') { continue }
                if ($sid -ne 'S-1-5-32-545') {
                    Report 'FAIL' ("{0} grants {1}, who has no business here" -f $f.Name, $sid)
                    continue
                }
                $rights = @($ace.FileSystemRights.ToString() -split ',\s*')
                $tooMuch = @($rights | Where-Object { $fileUsersMayHave -notcontains $_ })
                if ($tooMuch.Count -gt 0) {
                    Report 'FAIL' ("{0} grants Users more than deletion" -f $f.Name) `
                        ("unexpected: {0} -- ReadData here would BE the history" -f ($tooMuch -join ', '))
                } else {
                    Report 'PASS' ("{0}: Users can delete it, not read it" -f $f.Name) `
                        ($rights -join ', ')
                }
            }
        }
    }
} else {
    if ($null -eq $listing) {
        Report 'INFO' 'an ordinary shell cannot list the memory directory' `
            ("{0}`n        Note: the folder is then undeletable from an ordinary session." -f $listError)
    } else {
        Report 'PASS' ("an ordinary shell can list the directory ({0} file(s))" -f $listing.Count) `
            'Deliberate -- a recursive delete enumerates first. Listing is not reading; the content test below is what matters.'
    }
}

# --- 3b. the delete carve-out: granted, without deleting anything -----------
#
# Opening a handle with DELETE access proves the right is there. The
# disposition is never set, so nothing is removed.

if (-not $Elevated -and $null -ne $listing -and $listing.Count -gt 0) {
    if (-not ('Win32DeleteProbe' -as [type])) {
        Add-Type -Namespace Probe -Name Win32DeleteProbe -MemberDefinition @'
[DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern System.IntPtr CreateFileW(
    string lpFileName, uint dwDesiredAccess, uint dwShareMode,
    System.IntPtr lpSecurityAttributes, uint dwCreationDisposition,
    uint dwFlagsAndAttributes, System.IntPtr hTemplateFile);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool CloseHandle(System.IntPtr hObject);
'@ -ErrorAction SilentlyContinue
    }

    $DELETE = 0x00010000
    $SHARE_ALL = 7
    $OPEN_EXISTING = 3
    $BACKUP_SEMANTICS = 0x02000000
    $INVALID = [System.IntPtr]::new(-1)

    $target = $listing[0].FullName
    $h = [Probe.Win32DeleteProbe]::CreateFileW($target, $DELETE, $SHARE_ALL, [System.IntPtr]::Zero, $OPEN_EXISTING, 0, [System.IntPtr]::Zero)
    if ($h -eq $INVALID) {
        Report 'FAIL' 'an ordinary shell cannot open a learned file for DELETE' `
            ("{0}`n        The folder cannot be removed without elevation." -f $target)
    } else {
        [void][Probe.Win32DeleteProbe]::CloseHandle($h)
        Report 'PASS' 'an ordinary shell may DELETE a learned file (nothing was deleted)' $target
    }

    $h = [Probe.Win32DeleteProbe]::CreateFileW($memoryDir, $DELETE, $SHARE_ALL, [System.IntPtr]::Zero, $OPEN_EXISTING, $BACKUP_SEMANTICS, [System.IntPtr]::Zero)
    if ($h -eq $INVALID) {
        Report 'FAIL' 'an ordinary shell cannot open the directory for DELETE' `
            'The folder would outlive an uninstall with only an admin able to remove it.'
    } else {
        [void][Probe.Win32DeleteProbe]::CloseHandle($h)
        Report 'PASS' 'an ordinary shell may DELETE the directory (nothing was deleted)'
    }
}

# --- 4. THE test: can an ordinary process read the history? -----------------
#
# Read the raw bytes rather than Get-Content: the store is binary, and
# Get-Content would additionally fail on encoding, which would look like the
# same denial for a different reason.

$probe = Join-Path $memoryDir 'memory.louds'
$probeExists = Test-Path -LiteralPath $probe -PathType Leaf

if (-not $probeExists -and $null -ne $listing -and $listing.Count -gt 0) {
    $probe = $listing[0].FullName
    $probeExists = $true
}

if (-not $probeExists) {
    Report 'SKIP' 'there is no learned file to try reading' `
        'Type something, pick a candidate, press Enter, then run this again.'
} else {
    $bytes = $null
    $readError = $null
    try {
        $stream = [System.IO.File]::Open($probe, 'Open', 'Read', 'Read')
        $buffer = New-Object byte[] 16
        $null = $stream.Read($buffer, 0, 16)
        $stream.Close()
        $bytes = $buffer
    } catch {
        $readError = $_.Exception.Message
    }

    if ($Elevated) {
        if ($null -eq $bytes) {
            Report 'FAIL' 'an elevated shell cannot read the learned file' `
                ("{0}`n        {1}" -f $probe, $readError)
        } else {
            Report 'PASS' 'an elevated shell can read the learned file' $probe
        }
    } else {
        if ($null -eq $bytes) {
            # Distinguish the denial we want from any other failure.
            if ($readError -match 'denied|Denied|UnauthorizedAccess') {
                Report 'PASS' 'an ordinary shell is DENIED the learned file' $probe
            } else {
                Report 'FAIL' 'the read failed, but not with an access denial' `
                    ("{0}`n        {1}" -f $probe, $readError)
            }
        } else {
            Report 'FAIL' 'AN ORDINARY SHELL READ THE LEARNING HISTORY' `
                ("{0}`n        This is the protection failing. Is the server running elevated?" -f $probe)
        }
    }
}

# --- 5. the path must not have leaked into settings.json --------------------

if (Test-Path -LiteralPath $settings -PathType Leaf) {
    $text = Get-Content -LiteralPath $settings -Raw -ErrorAction SilentlyContinue
    if ($null -eq $text) {
        Report 'SKIP' 'settings.json could not be read'
    } elseif ($text -match 'memory_directory') {
        Report 'FAIL' 'settings.json contains memory_directory' `
            'That key is computed per machine and must never be written to a roaming file.'
    } else {
        Report 'PASS' 'settings.json does not contain memory_directory'
        if ($text -match '"learning"') {
            $on = if ($text -match '"enable"\s*:\s*true') { 'present' } else { 'present (value unread)' }
            Report 'INFO' "settings.json has a learning section ($on)"
        } else {
            Report 'INFO' 'settings.json has no learning section yet' `
                'It is written on the next save; the defaults apply until then.'
        }
    }
} else {
    Report 'SKIP' 'settings.json does not exist yet'
}

Write-Output ''
Write-Output "Failures: $script:Failures"
if ($script:Failures -gt 0) { exit 1 } else { exit 0 }
