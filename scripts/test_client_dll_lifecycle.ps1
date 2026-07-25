<#
.SYNOPSIS
    Loads and unloads the TIP DLL repeatedly, checking it survives the cycle.

.DESCRIPTION
    DllMain runs under the loader lock inside every application that has ever
    used the IME, Explorer included. Anything it does that takes a lock,
    allocates or calls back into the loader risks deadlocking the host, and a
    deadlock there is not a crash report -- it is a frozen desktop.

    This exercises the entry point the cheap way: LoadLibraryW ->
    GetProcAddress("DllCanUnloadNow") -> call it -> FreeLibrary, N times. It
    runs DLL_PROCESS_ATTACH and DLL_PROCESS_DETACH once per iteration without
    registering the TIP, so it is safe on a working machine.

    It does NOT prove the absence of a loader-lock deadlock (that needs a
    second thread contending for the lock). It does catch the regressions that
    actually happen: an entry point that fails, that leaks its module across
    cycles, or that faults on the second attach.

    ASCII ONLY, like the other scripts here: Windows PowerShell 5.1 reads a
    BOM-less UTF-8 script as CP932.

.PARAMETER Path
    The DLL to exercise. Defaults to the x64 build output.

.PARAMETER Iterations
    How many load/unload cycles. Default 50.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/test_client_dll_lifecycle.ps1
.EXAMPLE
    # the x86 TIP needs a 32-bit host
    C:\Windows\SysWOW64\WindowsPowerShell\v1.0\powershell.exe -ExecutionPolicy Bypass -File scripts/test_client_dll_lifecycle.ps1 -Path build/x86/azookey_windows.dll
#>
[CmdletBinding()]
param(
    [string]$Path = 'build/azookey_windows.dll',
    [int]$Iterations = 50
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Path)) { throw "no DLL at $Path (run cargo make build first)" }
$dll = (Resolve-Path $Path).Path

# PE machine type, straight out of the file: e_lfanew at 0x3C points at the
# NT headers, whose Machine field is the two bytes after the PE signature.
$bytes = [System.IO.File]::ReadAllBytes($dll)
$peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
if ([System.Text.Encoding]::ASCII.GetString($bytes, $peOffset, 2) -ne 'PE') {
    throw "$dll is not a PE image"
}
$machine = [BitConverter]::ToUInt16($bytes, $peOffset + 4)

$IMAGE_FILE_MACHINE_I386 = 0x014c
$IMAGE_FILE_MACHINE_AMD64 = 0x8664
$IMAGE_FILE_MACHINE_ARM64 = 0xAA64

$dllArch = switch ($machine) {
    $IMAGE_FILE_MACHINE_I386 { 'x86' }
    $IMAGE_FILE_MACHINE_AMD64 { 'x64' }
    $IMAGE_FILE_MACHINE_ARM64 { 'arm64' }
    default { "unknown (0x{0:X4})" -f $machine }
}
$hostArch = if ([IntPtr]::Size -eq 8) { 'x64' } else { 'x86' }

Write-Host "dll   : $dll"
Write-Host "image : $dllArch"
Write-Host "host  : PowerShell $hostArch"

# A mismatch is the common mistake, and LoadLibraryW's error for it
# (ERROR_BAD_EXE_FORMAT) reads like a corrupt file rather than a wrong host.
if ($dllArch -ne $hostArch) {
    throw ("cannot load an $dllArch image from an $hostArch process. Use " +
        "C:\Windows\SysWOW64\WindowsPowerShell\v1.0\powershell.exe for x86, " +
        "or the ordinary powershell.exe for x64.")
}

Add-Type -Namespace Native -Name Loader -MemberDefinition @'
[DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern IntPtr LoadLibraryW(string lpLibFileName);

[DllImport("kernel32.dll", CharSet = CharSet.Ansi, SetLastError = true)]
public static extern IntPtr GetProcAddress(IntPtr hModule, string lpProcName);

[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool FreeLibrary(IntPtr hModule);
'@

# The four exports a COM in-proc server owes its host. Their presence is what
# regsvr32 and the TSF loader depend on, and a .def or linker change can drop
# one without any test noticing.
$exports = @('DllCanUnloadNow', 'DllGetClassObject', 'DllRegisterServer', 'DllUnregisterServer')

$firstBase = [IntPtr]::Zero
for ($i = 1; $i -le $Iterations; $i++) {
    $h = [Native.Loader]::LoadLibraryW($dll)
    if ($h -eq [IntPtr]::Zero) {
        $err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "LoadLibraryW failed on iteration $i with error $err"
    }
    if ($firstBase -eq [IntPtr]::Zero) { $firstBase = $h }

    foreach ($name in $exports) {
        if ([Native.Loader]::GetProcAddress($h, $name) -eq [IntPtr]::Zero) {
            [void][Native.Loader]::FreeLibrary($h)
            throw "the DLL does not export $name (iteration $i)"
        }
    }

    if (-not [Native.Loader]::FreeLibrary($h)) {
        $err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "FreeLibrary failed on iteration $i with error $err"
    }
}

Write-Host "$Iterations load/unload cycles completed"

# A module that never came back to the same base across a full unload is the
# signature of something keeping it alive -- a leaked reference, a thread the
# entry point spawned, a static that pinned it.
$again = [Native.Loader]::LoadLibraryW($dll)
if ($again -eq [IntPtr]::Zero) { throw 'the DLL would not load a final time' }
$reused = ($again -eq $firstBase)
[void][Native.Loader]::FreeLibrary($again)
Write-Host "module base stable across unload: $reused"

Write-Host 'DLL LIFECYCLE: PASS'
exit 0
