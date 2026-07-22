use std::{
    cell::{Ref, RefCell, RefMut},
    ffi::c_void,
};

use windows::{
    core::{implement, IUnknown, Interface, GUID},
    Win32::{
        Foundation::{BOOL, CLASS_E_NOAGGREGATION, E_NOINTERFACE},
        System::Com::{IClassFactory, IClassFactory_Impl},
        UI::TextServices::{
            ITfCandidateListUIElement, ITfCompartmentEventSink, ITfCompositionSink,
            ITfDisplayAttributeProvider, ITfKeyEventSink, ITfLangBarItem, ITfLangBarItemButton,
            ITfSource, ITfTextInputProcessor, ITfTextInputProcessorEx, ITfTextLayoutSink,
            ITfThreadMgrEventSink,
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
    ITfSource,
    ITfCompartmentEventSink,
    ITfCandidateListUIElement
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

        // We do not support aggregation, and COM has an HRESULT that says
        // exactly that; E_NOINTERFACE told the caller the wrong thing (it
        // reads as "no such interface", not "no aggregation").
        if punkouter.is_some() {
            return Err(anyhow::Error::new(windows::core::Error::from_hresult(
                CLASS_E_NOAGGREGATION,
            )));
        }

        unsafe {
            *ppvobject = match riid {
                // The COM contract requires a class factory to support at
                // least IID_IUnknown: a caller that creates via IUnknown and
                // then queries — diagnostic tools, mostly — is entitled to
                // succeed. TSF itself asks for ITfTextInputProcessor.
                IUnknown::IID => {
                    std::mem::transmute::<IUnknown, *mut c_void>(TextServiceFactory::create::<
                        IUnknown,
                    >()?)
                }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use windows::Win32::UI::TextServices::ITfLangBarItem;

    fn class_factory() -> IClassFactory {
        TextServiceFactory::default().into()
    }

    /// The COM contract requires a class factory to support at least
    /// IID_IUnknown (issue #27). TSF asks for ITfTextInputProcessor, so
    /// nothing about normal operation covers this path — a host or
    /// diagnostic tool creating via IUnknown used to be turned away.
    #[test]
    fn create_instance_supports_iunknown() {
        let factory = class_factory();

        let unknown: IUnknown = unsafe { factory.CreateInstance(None) }
            .expect("CreateInstance must support IID_IUnknown");

        // and the object handed back is really ours
        unknown
            .cast::<ITfTextInputProcessor>()
            .expect("the IUnknown must be the TIP's own identity");
    }

    #[test]
    fn create_instance_supports_the_tsf_interfaces() {
        for result in [
            unsafe { class_factory().CreateInstance::<_, ITfTextInputProcessor>(None) }.map(|_| ()),
            unsafe { class_factory().CreateInstance::<_, ITfTextInputProcessorEx>(None) }
                .map(|_| ()),
        ] {
            result.expect("the TSF entry points must still be creatable");
        }
    }

    /// An interface we deliberately do not hand out from the factory. The
    /// HRESULT matters: a host that branches on it can tell "wrong
    /// interface" from "something broke", which E_FAIL denied it before
    /// the macro carried HRESULTs through (issue #1).
    #[test]
    fn an_unsupported_interface_is_refused_with_e_nointerface() {
        let err = unsafe { class_factory().CreateInstance::<_, ITfLangBarItem>(None) }
            .expect_err("the factory only creates the TIP entry points");

        assert_eq!(err.code(), E_NOINTERFACE);
    }

    /// Aggregation is not supported, and COM has an HRESULT that says so.
    #[test]
    fn aggregation_is_refused_with_class_e_noaggregation() {
        let outer: IUnknown = TextServiceFactory::default().into();

        let err = unsafe { class_factory().CreateInstance::<_, IUnknown>(&outer) }
            .expect_err("the factory does not support aggregation");

        assert_eq!(err.code(), CLASS_E_NOAGGREGATION);
    }
}
