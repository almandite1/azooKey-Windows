mod engine;
mod extension;
mod globals;
mod macros;
mod register;
mod trace;
mod tsf;

use std::ffi::c_void;

use globals::{DllModule, GUID_TEXT_SERVICE};
use register::{CLSIDMgr, CategoryMgr, ProfileMgr};
use tsf::factory::TextServiceFactory;
use windows::{
    Win32::{
        Foundation::{
            CLASS_E_CLASSNOTAVAILABLE, E_INVALIDARG, E_NOINTERFACE, HMODULE, S_FALSE, S_OK,
        },
        System::{
            Com::IClassFactory, LibraryLoader::DisableThreadLibraryCalls, Ole::SELFREG_E_CLASS,
            SystemServices::DLL_PROCESS_ATTACH,
        },
    },
    core::{GUID, HRESULT, IUnknown, Interface as _},
};

// Logger setup is deferred out of DllMain: spawning threads or doing real
// work under the loader lock can deadlock the host process. First called
// from DllGetClassObject, which runs outside the loader lock.
static LOGGER_INIT: std::sync::Once = std::sync::Once::new();

fn ensure_logger() {
    LOGGER_INIT.call_once(|| {
        // best effort: logging is optional, never fail the caller
        let _ = trace::setup_logger();
    });
}
// -- Dll Export Functions --
// The IME DLL needs to implement the following four functions to operate as a COM server.

/// Deliberately almost empty.
///
/// This runs under the LOADER LOCK, inside every application that has ever
/// used the IME. Anything that takes a lock, allocates, spawns a thread or
/// calls back into the loader can deadlock the host — and a TIP is loaded
/// into Explorer, so "the host" includes the desktop. Logger setup is already
/// deferred to `DllGetClassObject` (see `ensure_logger`); this leaves only a
/// relaxed atomic store and one API call that is allowed to fail.
///
/// `DLL_PROCESS_DETACH` in particular does NOTHING. It used to take the
/// `DllModule` mutex and send on a channel, both under that lock. There is
/// nothing to release: `DllCanUnloadNow` always answers `S_FALSE`, so the DLL
/// only ever goes away with the process, and the process takes its memory,
/// handles and threads with it.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    hinst: HMODULE,
    fdw_reason: u32,
    _lpv_reserved: *mut c_void,
) -> bool {
    if fdw_reason == DLL_PROCESS_ATTACH {
        globals::set_dll_hmodule(hinst);

        // Best effort, and the return value is deliberately ignored: it fails
        // when the DLL has static TLS, in which case the notifications simply
        // keep coming. We ignore DLL_THREAD_ATTACH/DETACH either way, so this
        // only saves the host the call.
        let _ = unsafe { DisableThreadLibraryCalls(hinst) };
    }

    true
}

#[unsafe(no_mangle)]
/// # Safety
/// This function uses raw pointers
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    // Return a class factory to obtain the tsf TextService
    // This function will be called only once when applications request the TextService
    // So, You have to reopen the application to apply the changes in the TextService
    // https://zenn.dev/link/comments/d918e46723da80
    ensure_logger();
    tracing::debug!("DllGetClassObject");

    // COM contract: on failure return a failure HRESULT (never S_FALSE — a
    // host checking SUCCEEDED(hr) would then read an invalid *ppv) and leave
    // *ppv null.
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_INVALIDARG;
    }

    unsafe { *ppv = std::ptr::null_mut() };

    let rclsid = unsafe { *rclsid };
    let riid = unsafe { *riid };

    if rclsid != GUID_TEXT_SERVICE {
        return CLASS_E_CLASSNOTAVAILABLE;
    }

    let instance = match riid {
        IUnknown::IID => unsafe {
            std::mem::transmute::<IUnknown, *mut c_void>(IUnknown::from(
                TextServiceFactory::default(),
            ))
        },
        IClassFactory::IID => unsafe {
            std::mem::transmute::<IClassFactory, *mut c_void>(IClassFactory::from(
                TextServiceFactory::default(),
            ))
        },
        _ => return E_NOINTERFACE,
    };

    unsafe { *ppv = instance };
    S_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    // Register the CLSID of the TextService
    // Called when the DLL is registered using regsvr32
    tracing::debug!("DllRegisterServer");

    let result: anyhow::Result<()> = (|| {
        let dll_path = DllModule::get_path()?;

        ProfileMgr::register(&dll_path)?;
        CLSIDMgr::register(&dll_path)?;
        CategoryMgr::register()?;

        Ok(())
    })();

    // to show the error, SELFREG_E_CLASS is needed
    check_err!(result, SELFREG_E_CLASS)
}

#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    // Unregister the CLSID of the TextService
    // Called when the DLL is unregistered using regsvr32
    tracing::debug!("DllUnregisterServer");

    // Best-effort: attempt all three unregistrations even if one fails, so a
    // single failure (e.g. a key already gone) doesn't leave the others
    // behind — the old short-circuit skipped CategoryMgr cleanup entirely.
    let mut errors: Vec<String> = Vec::new();
    if let Err(e) = ProfileMgr::unregister() {
        errors.push(format!("profile: {e:#}"));
    }
    if let Err(e) = CLSIDMgr::unregister() {
        errors.push(format!("clsid: {e:#}"));
    }
    if let Err(e) = CategoryMgr::unregister() {
        errors.push(format!("category: {e:#}"));
    }

    let result: anyhow::Result<()> = if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(errors.join("; ")))
    };

    check_err!(result, SELFREG_E_CLASS)
}

#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    // Always refuse to unload: DllModule's ref count does not yet track every
    // live COM object, so claiming S_OK could let the host unload the DLL
    // while a TSF sink is still alive.
    S_FALSE
}
