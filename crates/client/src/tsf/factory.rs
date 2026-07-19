use std::{
    cell::{Ref, RefCell, RefMut},
    ffi::c_void,
};

use windows::{
    core::{implement, IUnknown, Interface, GUID},
    Win32::{
        Foundation::{BOOL, E_NOINTERFACE},
        System::Com::{IClassFactory, IClassFactory_Impl},
        UI::TextServices::{
            ITfCompositionSink, ITfDisplayAttributeProvider, ITfKeyEventSink, ITfLangBarItem,
            ITfLangBarItemButton, ITfSource, ITfTextInputProcessor, ITfTextInputProcessorEx,
            ITfTextLayoutSink, ITfThreadMgrEventSink,
        },
    },
};

use anyhow::Result;

use crate::globals::DllModule;

use super::text_service::TextService;

#[derive(Default)]
#[implement(
    IClassFactory,
    ITfTextInputProcessor,
    ITfTextInputProcessorEx,
    ITfKeyEventSink,
    ITfThreadMgrEventSink,
    ITfTextLayoutSink,
    ITfCompositionSink,
    ITfDisplayAttributeProvider,
    ITfLangBarItem,
    ITfLangBarItemButton,
    ITfSource
)]
#[derive(Debug)]
pub struct TextServiceFactory {
    text_service: RefCell<TextService>,
}

impl IClassFactory_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn CreateInstance(
        &self,
        punkouter: Option<&IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        let riid = unsafe { *riid };
        let ppvobject = unsafe { &mut *ppvobject };

        *ppvobject = std::ptr::null_mut();

        if punkouter.is_some() {
            return Err(anyhow::Error::new(windows::core::Error::from_hresult(
                E_NOINTERFACE,
            )));
        }

        unsafe {
            *ppvobject = match riid {
                ITfTextInputProcessor::IID => {
                    std::mem::transmute::<ITfTextInputProcessor, *mut c_void>(
                        TextServiceFactory::create::<ITfTextInputProcessor>()?,
                    )
                }
                ITfTextInputProcessorEx::IID => {
                    std::mem::transmute::<ITfTextInputProcessorEx, *mut c_void>(
                        TextServiceFactory::create::<ITfTextInputProcessorEx>()?,
                    )
                }
                _ => {
                    return Err(anyhow::Error::new(windows::core::Error::from_hresult(
                        E_NOINTERFACE,
                    )))
                }
            };
        }

        Ok(())
    }

    #[macros::anyhow]
    fn LockServer(&self, flock: BOOL) -> Result<()> {
        let mut dll_instance = DllModule::get()?;
        if flock.into() {
            dll_instance.add_ref();
        } else {
            dll_instance.release();
        }

        Ok(())
    }
}

impl TextServiceFactory {
    pub fn create<I: Interface>() -> Result<I> {
        let factory = Self {
            text_service: RefCell::new(TextService::default()),
        };

        // Moving into the interface places the factory in its COM heap
        // allocation; no stored self-reference (see `this()` below).
        let this = ITfTextInputProcessor::from(factory);
        this.cast::<I>().map_err(anyhow::Error::new)
    }

    /// QueryInterface on the containing COM object. Replaces the old stored
    /// `TextService.this` self-reference, which kept the refcount above zero
    /// forever and leaked one TextService per profile switch (B15). Both
    /// paths are a QI on the same COM identity, so the sink pointers handed
    /// to TSF are unchanged.
    ///
    /// SAFETY invariant: every `TextServiceFactory` is moved into its COM
    /// allocation (via `ITfTextInputProcessor::from` / `IUnknown::from` in
    /// `create()` and `CreateInstance`) before any method is called on it;
    /// no bare stack instance ever calls `this()`.
    pub fn this<I: Interface>(&self) -> Result<I> {
        unsafe { self.cast::<I>().map_err(anyhow::Error::new) }
    }

    pub fn borrow_mut(&self) -> Result<RefMut<'_, TextService>> {
        Ok(self.text_service.try_borrow_mut()?)
    }

    pub fn borrow(&self) -> Result<Ref<'_, TextService>> {
        Ok(self.text_service.try_borrow()?)
    }

    /// Advises this TIP as a sink of type `S` on `source` and remembers the
    /// cookie under `S::IID` in the per-instance cookie map (B14). Every
    /// advise/unadvise pair in the TIP goes through these two, so a new
    /// sink cannot re-invent the bookkeeping.
    pub fn advise_sink<S: Interface>(
        &self,
        source: &ITfSource,
        text_service: &mut TextService,
    ) -> Result<()> {
        let sink: IUnknown = self.this::<S>()?.cast()?;
        let cookie = unsafe { source.AdviseSink(&S::IID, &sink)? };
        text_service.cookies.insert(S::IID, cookie);
        Ok(())
    }

    /// Unadvises the sink of type `S` using the cookie remembered by
    /// `advise_sink`; a no-op if it was never advised.
    pub fn unadvise_sink<S: Interface>(
        &self,
        source: &ITfSource,
        text_service: &mut TextService,
    ) -> Result<()> {
        if let Some(cookie) = text_service.cookies.remove(&S::IID) {
            unsafe { source.UnadviseSink(cookie)? };
        }
        Ok(())
    }
}
