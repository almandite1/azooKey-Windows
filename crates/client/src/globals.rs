use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result};

use windows::{
    Win32::{
        Foundation::{FALSE, HMODULE, MAX_PATH},
        System::LibraryLoader::GetModuleFileNameW,
        UI::TextServices::{
            TF_ATTR_TARGET_CONVERTED, TF_CT_NONE, TF_DA_COLOR, TF_DA_COLOR_0, TF_DISPLAYATTRIBUTE,
            TF_LS_SOLID,
        },
    },
    core::GUID,
};

pub const CLSID_PREFIX: &str = "CLSID\\";
pub const INPROC_SUFFIX: &str = "\\InProcServer32";

pub const SERVICE_NAME: &str = "Azookey";

// ffdefe79-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_TEXT_SERVICE: GUID = GUID::from_u128(0xffdefe79_2fc2_11ef_b16b_94e70b2c378c);
// ffdefe7a-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_PROFILE: GUID = GUID::from_u128(0xffdefe7a_2fc2_11ef_b16b_94e70b2c378c);

// DisplayAttribute用のGUID
pub const GUID_DISPLAY_ATTRIBUTE: GUID = GUID::from_u128(0xffdefe7b_2fc2_11ef_b16b_94e70b2c378c);

/// Identifies our candidate list to `ITfUIElementMgr`. Must be TIP-specific
/// and stable — deliberately NOT `GUID_TEXT_SERVICE`, which identifies the
/// text service itself rather than this UI element.
// ffdefe7c-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_CANDIDATE_LIST_UI_ELEMENT: GUID =
    GUID::from_u128(0xffdefe7c_2fc2_11ef_b16b_94e70b2c378c);

// Identify the keys reserved with `ITfKeystrokeMgr::PreserveKey`. One GUID
// per *purpose* rather than per key: TSF hands the GUID back in
// `OnPreservedKey`, and separating them is what lets a log say which key the
// user actually pressed. Zenkaku/Hankaku shares one GUID across its two
// virtual keys because they are the same physical key.
// ffdefe7d-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_PRESERVEDKEY_TOGGLE_ZENHAN: GUID =
    GUID::from_u128(0xffdefe7d_2fc2_11ef_b16b_94e70b2c378c);
// ffdefe7e-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_PRESERVEDKEY_TOGGLE_KANJI: GUID =
    GUID::from_u128(0xffdefe7e_2fc2_11ef_b16b_94e70b2c378c);
// ffdefe7f-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_PRESERVEDKEY_TOGGLE_ALT_GRAVE: GUID =
    GUID::from_u128(0xffdefe7f_2fc2_11ef_b16b_94e70b2c378c);

/// `VK_DBE_SBCSCHAR` / `VK_DBE_DBCSCHAR` — the two virtual keys the
/// Zenkaku/Hankaku key produces depending on the current mode.
pub const VK_ZENKAKU_HANKAKU: [u32; 2] = [0xF3, 0xF4];

/// `VK_KANJI`. The 漢字 key of a JIS keyboard — and, with Alt held, what
/// Windows translates **Alt+`** into on a 101-key Japanese layout. Measured
/// on hardware: the key arrives as `OnKeyDown(wparam=0x19)` with the Alt flag
/// set in lparam and the scan code of the `` ` `` key (0x29). It does NOT
/// arrive as `VK_OEM_3`, so that reservation alone never fires.
pub const VK_KANJI: u32 = 0x19;

/// `VK_OEM_3` — `` ` `` on a US layout. Reserved with Alt as well, for
/// layouts and hosts where the translation above does not happen.
pub const VK_OEM_3: u32 = 0xC0;

/// Every virtual key that means "switch the IME on/off".
///
/// Read from both sides of the same decision and therefore declared once:
/// `tsf::preserved_key` asks TSF to route these through `OnPreservedKey`, and
/// `engine::user_action` recognises them when a host delivers one raw anyway.
/// They used to be spelled out as bare `0xF3 | 0xF4 | 0x19` literals on the
/// engine side.
pub const VK_IME_TOGGLE: [u32; 3] = [VK_ZENKAKU_HANKAKU[0], VK_ZENKAKU_HANKAKU[1], VK_KANJI];

pub const DISPLAY_ATTRIBUTE: TF_DISPLAYATTRIBUTE = TF_DISPLAYATTRIBUTE {
    crText: TF_DA_COLOR {
        r#type: TF_CT_NONE,
        Anonymous: TF_DA_COLOR_0 { nIndex: 0 },
    },
    crBk: TF_DA_COLOR {
        r#type: TF_CT_NONE,
        Anonymous: TF_DA_COLOR_0 { nIndex: 0 },
    },
    lsStyle: TF_LS_SOLID,
    fBoldLine: FALSE,
    crLine: TF_DA_COLOR {
        r#type: TF_CT_NONE,
        Anonymous: TF_DA_COLOR_0 { nIndex: 0 },
    },
    bAttr: TF_ATTR_TARGET_CONVERTED,
};

// You can use any value for this cookie.
pub const TEXTSERVICE_LANGBARITEMSINK_COOKIE: u32 = 0;

pub static DLL_INSTANCE: OnceLock<Mutex<DllModule>> = OnceLock::new();

/// The module handle `DllMain` was given, as a plain integer.
///
/// An atomic rather than a field of the `Mutex<DllModule>` below because
/// `DllMain` runs under the LOADER LOCK: taking a lock there can deadlock the
/// host against any other thread that holds it and is waiting on the loader.
/// A relaxed store of a pointer-sized integer cannot. Everything that needs
/// the handle reads it back through [`DllModule::hmodule`], which is likewise
/// lock-free.
///
/// Zero means "DllMain has not run", which is the case in unit tests.
static DLL_HMODULE: AtomicUsize = AtomicUsize::new(0);

/// Records the handle `DllMain` received. The only thing `DLL_PROCESS_ATTACH`
/// does besides `DisableThreadLibraryCalls`.
pub fn set_dll_hmodule(hinst: HMODULE) {
    DLL_HMODULE.store(hinst.0 as usize, Ordering::Relaxed);
}

unsafe impl Sync for DllModule {}
unsafe impl Send for DllModule {}

/// The COM lock count for `DllCanUnloadNow`. It used to carry the module
/// handle and a channel sender too; the handle moved to [`DLL_HMODULE`] (see
/// there) and the sender was never assigned by anything.
#[derive(Debug)]
pub struct DllModule {
    pub ref_count: Arc<AtomicUsize>,
}

impl DllModule {
    pub fn new() -> Self {
        Self {
            ref_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn get() -> Result<MutexGuard<'static, DllModule>> {
        // Initialized here, on first use, rather than in DllMain: building it
        // allocates, and the less that happens under the loader lock the
        // better. Every caller is a COM entry point, well outside it.
        //
        // Poisoning is recovered from: one panic while the lock was held must
        // not permanently break every later Activate/LockServer call.
        Ok(DLL_INSTANCE
            .get_or_init(|| Mutex::new(DllModule::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The module handle, or an error before `DllMain` has run (unit tests).
    pub fn hmodule() -> Result<HMODULE> {
        match DLL_HMODULE.load(Ordering::Relaxed) {
            0 => Err(anyhow::anyhow!("DllMain has not recorded a module handle")),
            handle => Ok(HMODULE(handle as *mut core::ffi::c_void)),
        }
    }

    pub fn get_path() -> anyhow::Result<String> {
        let dll_instance = Self::hmodule().context("Dll instance not found")?;

        // GetModuleFileNameW does not report the required length: it fills the
        // buffer, and if the path does not fit it truncates and returns the
        // buffer size. A single fixed MAX_PATH call therefore silently yields a
        // truncated path for deep install locations. Grow the buffer until the
        // returned length is strictly less than its size (i.e. it fit).
        let mut buffer: Vec<u16> = vec![0; MAX_PATH as usize];
        loop {
            let length = unsafe { GetModuleFileNameW(Some(dll_instance), &mut buffer) } as usize;

            if length == 0 {
                return Err(anyhow::anyhow!("GetModuleFileNameW failed: {:?}", unsafe {
                    windows::Win32::Foundation::GetLastError()
                }));
            }

            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(String::from_utf16_lossy(&buffer));
            }

            // length == buffer.len(): the path was truncated. Cap growth at the
            // Windows extended-length maximum (32767 + NUL) so a wrong result
            // cannot loop forever.
            if buffer.len() >= 0x8000 {
                return Err(anyhow::anyhow!(
                    "module path exceeds {} UTF-16 code units",
                    buffer.len()
                ));
            }
            buffer.resize(buffer.len() * 2, 0);
        }
    }

    // Both return nothing: `DllCanUnloadNow` always answers S_FALSE (the
    // count does not yet track every live COM object), so the previous value
    // has no reader and every call site was discarding it.
    pub fn add_ref(&mut self) {
        self.ref_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn release(&mut self) {
        self.ref_count.fetch_sub(1, Ordering::SeqCst);
    }
}
