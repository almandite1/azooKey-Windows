//! A fake TSF host for unit tests.
//!
//! The TIP normally only runs inside a real host (Word, Chrome, Explorer),
//! which is why the COM-facing half of this crate has had no test coverage.
//! `TextServiceFactory::start_composition` and friends are `pub fn` and
//! `TextService::context` is a plain `Option<ITfContext>` field, so a fake
//! context implementing the same interfaces can be injected and the real
//! code driven against it.
//!
//! What this buys us over a real host: the fake can be told to *misbehave*
//! on demand (deny an edit session, defer it asynchronously) and it records
//! what it was asked to do, so the tests can assert on both the arguments
//! the TIP passes and the way it handles a hostile answer.
//!
//! The fakes themselves live in [`super::fakes`], one module per host object
//! they stand in for; this file is the entry point tests import from, so
//! every existing `use crate::tsf::test_support::{..}` keeps working.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::new_ret_no_self)]
// The re-exports below are the harness's surface, not a list of what happens
// to be used today: a test that reaches for a fake this build does not
// exercise should find it here rather than have to edit this file.
#![allow(unused_imports)]

use windows::Win32::UI::TextServices::ITfContext;

pub use super::fakes::compartment::{CompartmentLog, FakeCompartment};
pub use super::fakes::context::{
    EditSessionBehavior, EditSessionRequest, FAKE_TEXT_EXT, FakeContext, FakeContextView,
    FakeDocumentMgr, TextExtBehavior, ViewLog,
};
pub use super::fakes::range::{FakeComposition, FakeProperty, FakeRange, RangeLog};
pub use super::fakes::thread_mgr::{
    FakeThreadMgr, FakeThreadMgrConfig, ThreadMgrLog, UiElementLog,
};

/// The edit cookie the fake hands to `DoEditSession`. Any non-zero value
/// works; a real host's cookie is opaque to the TIP.
pub const FAKE_COOKIE: u32 = 0x1234;

/// Serializes tests that touch process-global state (`IMEState`,
/// `DllModule`). Tests run in parallel threads within one process, so any
/// test that reads or writes those globals must hold this guard — module-
/// local locks cannot see each other across test modules.
pub fn global_state_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Casts an `ITfContext` produced by [`FakeContext::new`] back to its Rust
/// implementation so a test can read what it recorded.
///
/// # Safety
/// The context must have come from [`FakeContext::new`].
pub unsafe fn fake_context_of(context: &ITfContext) -> &FakeContext {
    unsafe {
        use windows::core::AsImpl as _;
        context.as_impl()
    }
}

/// Builds a `TextServiceFactory` wired to the given (usually fake) context,
/// the way `TextServiceFactory::create` wires a real one.
pub fn factory_with_context(
    context: ITfContext,
) -> windows::Win32::UI::TextServices::ITfTextInputProcessor {
    use windows::Win32::UI::TextServices::ITfTextInputProcessor;
    use windows::core::AsImpl as _;

    use super::factory::TextServiceFactory;

    let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
        .expect("failed to create the factory");

    {
        let factory = unsafe { tip.as_impl() };
        let mut text_service = factory.borrow_mut().expect("factory is already borrowed");
        text_service.tid = 1;
        text_service.context = Some(context);
    }

    tip
}

/// The factory behind a TIP interface, as the *outer* COM object.
///
/// `as_impl()` yields the inner `TextServiceFactory`, and since windows 0.62
/// the inner type has no route back to its outer `_Impl` box — which is where
/// the factory's own methods now live, because they QueryInterface our own
/// identity. Tests therefore go through a `ComObject`, which derefs to the
/// `_Impl` (and on through to the inner, so `borrow()` still works).
pub fn factory_of(
    tip: &windows::Win32::UI::TextServices::ITfTextInputProcessor,
) -> windows::core::ComObject<super::factory::TextServiceFactory> {
    windows::core::ComObject::<super::factory::TextServiceFactory>::cast_from(tip)
        .expect("the TIP must be our own factory")
}

/// [`factory_with_context`] over a plain [`FakeContext`] with the given
/// edit-session behavior.
pub fn factory_with_fake_context(
    behavior: EditSessionBehavior,
) -> (
    windows::Win32::UI::TextServices::ITfTextInputProcessor,
    ITfContext,
) {
    let context = FakeContext::new(behavior);
    (factory_with_context(context.clone()), context)
}
