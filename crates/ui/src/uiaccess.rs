// obtain useraccess privileges from system applications
// cf: https://github.com/killtimer0/uiaccess/

use std::{ffi::c_void, ptr::addr_of_mut};

use anyhow::Result;
use windows::{
    Win32::{
        Foundation::{BOOL, CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        Security::{
            DuplicateTokenEx, GetTokenInformation, LookupPrivilegeValueW, PRIVILEGE_SET,
            PrivilegeCheck, SE_TCB_NAME, SecurityAnonymous, SecurityImpersonation,
            SetTokenInformation, TOKEN_ACCESS_MASK, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
            TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY, TokenImpersonation, TokenPrimary,
            TokenSessionId, TokenUIAccess,
        },
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next,
                TH32CS_SNAPPROCESS,
            },
            Environment::GetCommandLineW,
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject,
            },
            SystemServices::PRIVILEGE_SET_ALL_NECESSARY,
            Threading::{
                CREATE_SUSPENDED, CreateProcessAsUserW, ExitProcess, GetCurrentProcess,
                GetExitCodeProcess, GetStartupInfoW, INFINITE, OpenProcess, OpenProcessToken,
                PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, STARTUPINFOW,
                SetThreadToken, TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::PWSTR,
};

/// get token from current process
fn open_current_process_token() -> Result<HANDLE> {
    let mut h_token = HANDLE::default();
    unsafe {
        match OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut h_token,
        ) {
            Ok(()) => Ok(h_token),
            Err(_) => anyhow::bail!("OpenProcessToken failed"),
        }
    }
}

/// check for ui access
pub fn check_for_ui_access() -> Result<bool> {
    let mut token_ui_access: BOOL = false.into();
    let mut token_len: u32 = 0;

    unsafe {
        let h_token = open_current_process_token()?;
        let success = GetTokenInformation(
            h_token,
            TokenUIAccess,
            Some(&mut token_ui_access as *mut _ as *mut _),
            std::mem::size_of::<BOOL>() as u32,
            &mut token_len,
        );
        let _ = CloseHandle(h_token);
        if let Ok(()) = success {
            Ok(token_ui_access.as_bool())
        } else {
            anyhow::bail!("GetTokenInformation failed {success:?}");
        }
    }
}

pub fn duplicate_winlogon_token(
    session_id: u32,
    desired_access: TOKEN_ACCESS_MASK,
    h_token: &mut HANDLE,
) -> Result<()> {
    let mut privilege_set = PRIVILEGE_SET {
        PrivilegeCount: 1,
        Control: PRIVILEGE_SET_ALL_NECESSARY,
        ..Default::default()
    };

    unsafe {
        LookupPrivilegeValueW(None, SE_TCB_NAME, &mut privilege_set.Privilege[0].Luid)?;

        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        anyhow::ensure!(
            snapshot != INVALID_HANDLE_VALUE,
            "CreateToolhelp32Snapshot failed"
        );

        let mut process_entry = PROCESSENTRY32 {
            dwSize: std::mem::size_of::<PROCESSENTRY32>() as u32,
            ..Default::default()
        };

        // walk starting from the FIRST entry (the previous loop called
        // Process32First then immediately Process32Next, skipping it), close
        // the snapshot on the way out, and try_token failures per-process
        // don't abort the whole scan
        let mut result = Err(anyhow::anyhow!("no matching winlogon token found"));
        if Process32First(snapshot, &mut process_entry).is_ok() {
            loop {
                if is_winlogon(&process_entry) {
                    // bound to a local rather than matched as a temporary: a
                    // scrutinee temporary holding a token handle changes drop
                    // point between editions 2021 and 2024
                    let duplicated = try_duplicate_token(
                        &process_entry,
                        &mut privilege_set,
                        session_id,
                        desired_access,
                    );
                    match duplicated {
                        Ok(token) => {
                            *h_token = token;
                            result = Ok(());
                            // stop at the first winlogon whose session
                            // matches, instead of overwriting with the last
                            break;
                        }
                        Err(e) => {
                            eprintln!("winlogon token candidate rejected: {e:?}");
                        }
                    }
                }

                if Process32Next(snapshot, &mut process_entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
        result
    }
}

fn is_winlogon(entry: &PROCESSENTRY32) -> bool {
    let exe = entry
        .szExeFile
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&exe)
        .to_lowercase()
        .contains("winlogon")
}

/// Opens winlogon's token, verifies it has the TCB privilege and matches the
/// target session, and duplicates it. Owns and closes the intermediate
/// process/token handles; only the duplicated token escapes.
unsafe fn try_duplicate_token(
    entry: &PROCESSENTRY32,
    privilege_set: &mut PRIVILEGE_SET,
    session_id: u32,
    desired_access: TOKEN_ACCESS_MASK,
) -> Result<HANDLE> {
    unsafe {
        let process = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            entry.th32ProcessID,
        )?;

        let mut token = HANDLE::default();
        // helper so every early return still closes the handles above
        let inner = (|| {
            OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token)?;

            let mut privilege_result = false.into();
            PrivilegeCheck(token, privilege_set as *mut _, &mut privilege_result)?;

            let mut token_session_id: u32 = 0;
            let mut token_info_length: u32 = 0;
            GetTokenInformation(
                token,
                TokenSessionId,
                Some(addr_of_mut!(token_session_id) as *mut c_void),
                std::mem::size_of::<u32>() as u32,
                &mut token_info_length,
            )?;
            anyhow::ensure!(
                token_session_id == session_id,
                "TokenSessionId does not match the session_id"
            );

            let mut duplicated = HANDLE::default();
            DuplicateTokenEx(
                token,
                desired_access,
                None,
                SecurityImpersonation,
                TokenImpersonation,
                &mut duplicated,
            )?;
            Ok(duplicated)
        })();

        if !token.is_invalid() {
            let _ = CloseHandle(token);
        }
        let _ = CloseHandle(process);
        inner
    }
}

pub fn create_uiaccess_token(token_handle: &mut HANDLE) -> Result<()> {
    let mut token_self = HANDLE::default();

    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut token_self,
        )?;

        // everything below runs inside closures so that a failure still
        // reverts the thread impersonation and closes every handle: if the
        // caller falls back to running without UIAccess (see main.rs), a
        // lingering winlogon impersonation token or a leaked handle would
        // otherwise poison the rest of the process
        let result = (|| {
            let mut session_id = 0;
            let mut token_info_length = 0;

            GetTokenInformation(
                token_self,
                TokenSessionId,
                Some(addr_of_mut!(session_id) as *mut c_void),
                std::mem::size_of::<u32>() as u32,
                &mut token_info_length,
            )?;

            let mut system_token_handle = HANDLE::default();
            duplicate_winlogon_token(session_id, TOKEN_IMPERSONATE, &mut system_token_handle)?;

            // impersonate winlogon only for the duplication below, then
            // revert no matter how it goes
            let impersonated = (|| {
                SetThreadToken(None, system_token_handle)?;
                DuplicateTokenEx(
                    token_self,
                    TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
                    None,
                    SecurityAnonymous,
                    TokenPrimary,
                    token_handle,
                )?;

                let ui_access: BOOL = true.into();
                SetTokenInformation(
                    *token_handle,
                    TokenUIAccess,
                    &ui_access as *const _ as *mut _,
                    std::mem::size_of::<BOOL>() as u32,
                )?;
                Ok::<(), anyhow::Error>(())
            })();

            // revert impersonation before returning to the caller's context
            let _ = SetThreadToken(None, None);
            if !system_token_handle.is_invalid() {
                let _ = CloseHandle(system_token_handle);
            }

            impersonated
        })();

        if !token_self.is_invalid() {
            let _ = CloseHandle(token_self);
        }

        // don't leak the half-built primary token if a later step failed
        if result.is_err() && !token_handle.is_invalid() {
            let _ = CloseHandle(*token_handle);
            *token_handle = HANDLE::default();
        }

        result
    }
}

pub fn prepare_uiaccess_token() -> Result<()> {
    let ui_access = check_for_ui_access()?;
    if ui_access {
        println!("UIAccess is already enabled");
        return Ok(());
    }

    let mut token_handle = HANDLE::default();
    create_uiaccess_token(&mut token_handle)?;

    let mut startup_info = STARTUPINFOW::default();
    let mut process_info = PROCESS_INFORMATION::default();

    unsafe {
        GetStartupInfoW(&mut startup_info);
        // CREATE_SUSPENDED: the child must not run before it is tied to the
        // shim's kill-on-close job below — a child that starts first can
        // outlive the whole supervision chain and squat on the pipe name
        let created = CreateProcessAsUserW(
            token_handle,
            None,
            PWSTR(GetCommandLineW().as_ptr() as *mut u16),
            None,
            None,
            false,
            CREATE_SUSPENDED,
            None,
            None,
            &startup_info,
            &mut process_info,
        );

        // the primary token has done its job either way
        if !token_handle.is_invalid() {
            let _ = CloseHandle(token_handle);
        }
        created?;

        println!("Process created with UIAccess token");
        run_as_supervision_shim(process_info)
    }
}

/// Keeps the pre-UIAccess process alive as a supervision shim for the
/// re-executed child, mirroring the child's exit code. Never returns on the
/// success path; an error means the child could not be started and the caller
/// should fall back to running the UI itself, without UIAccess.
///
/// The launcher supervises by CHILD HANDLE: if this process just exited 0
/// (as it used to), the supervisor read that as a deliberate shutdown and
/// stopped — leaving the UIAccess child, the process actually drawing the
/// candidate window, with no hang detection and no restarts. Staying alive
/// keeps the launcher's watchdog aimed at something whose lifetime equals the
/// real UI's:
/// - child crashes → the shim exits with the same nonzero code → restart;
/// - child hangs → its health pipe goes silent → the watchdog kills the shim
///   → the job below tears the child down → restart;
/// - launcher dies → its kill-on-close job kills the shim → same teardown.
unsafe fn run_as_supervision_shim(process_info: PROCESS_INFORMATION) -> Result<()> {
    unsafe {
        // Tie the (still suspended) child to this shim's lifetime. A failure is
        // not fatal — supervision still works via the exit-code mirror; only the
        // die-with-the-shim guarantee is lost (same policy as the launcher's own
        // job setup).
        match create_kill_on_close_job() {
            // the job handle is deliberately never closed: closing the last
            // handle kills the child, so it must live exactly as long as this
            // process
            Ok(job) => {
                if let Err(e) = AssignProcessToJobObject(job, process_info.hProcess) {
                    eprintln!(
                        "UIAccess child could not be tied to the shim ({e}); it won't die with it"
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "shim job creation failed ({e}); the UIAccess child won't die with the shim"
                )
            }
        }

        if ResumeThread(process_info.hThread) == u32::MAX {
            // the child never ran; clean it up and let the caller run without
            // UIAccess instead of leaving a suspended zombie behind
            let _ = TerminateProcess(process_info.hProcess, 1);
            let _ = CloseHandle(process_info.hThread);
            let _ = CloseHandle(process_info.hProcess);
            anyhow::bail!("ResumeThread failed for the UIAccess child");
        }
        let _ = CloseHandle(process_info.hThread);

        WaitForSingleObject(process_info.hProcess, INFINITE);
        let mut code: u32 = 1;
        if GetExitCodeProcess(process_info.hProcess, &mut code).is_err() {
            // unknown outcome: report an abnormal exit so the launcher restarts
            code = 1;
        }
        let _ = CloseHandle(process_info.hProcess);
        ExitProcess(code);
    }
}

/// A job object that kills its members when the last handle closes, exactly
/// like the launcher's `CHILD_JOB`.
unsafe fn create_kill_on_close_job() -> Result<HANDLE> {
    unsafe {
        let job = CreateJobObjectW(None, windows::core::PCWSTR::null())?;

        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        if let Err(e) = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) {
            let _ = CloseHandle(job);
            return Err(e.into());
        }

        Ok(job)
    }
}
