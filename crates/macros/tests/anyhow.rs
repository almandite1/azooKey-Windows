//! Regression guards for `#[macros::anyhow]`.
//!
//! Both properties tested here were silently absent (issue #1): the macro
//! rebuilt the function from its name, inputs and body alone, so anything
//! else the author wrote — attributes, visibility — was dropped, and every
//! error was reported to the host as `E_FAIL`.

use anyhow::Result;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::{E_FAIL, E_NOINTERFACE, E_UNEXPECTED};

#[macros::anyhow]
fn returns_ok() -> Result<u32> {
    Ok(7)
}

#[macros::anyhow]
fn returns_plain_anyhow_error() -> Result<u32> {
    anyhow::bail!("no HRESULT anywhere in here")
}

#[macros::anyhow]
fn returns_windows_error() -> Result<u32> {
    Err(windows::core::Error::from(E_NOINTERFACE).into())
}

#[macros::anyhow]
fn returns_windows_error_with_context() -> Result<u32> {
    use anyhow::Context;
    Err::<u32, _>(windows::core::Error::from(E_UNEXPECTED))
        .context("while doing the thing the host asked for")
}

#[macros::anyhow]
fn panics() -> Result<u32> {
    panic!("COM callbacks must not unwind into the host")
}

#[test]
fn success_passes_the_value_through() {
    assert_eq!(returns_ok(), Ok(7));
}

/// An error that never carried an HRESULT still has to become one.
#[test]
fn a_plain_error_falls_back_to_e_fail() {
    assert_eq!(returns_plain_anyhow_error().unwrap_err().code(), E_FAIL);
}

/// The reason this matters: `IClassFactory::CreateInstance` answering an
/// unknown `riid` owes the caller `E_NOINTERFACE`, and a host that branches
/// on the HRESULT cannot act on `E_FAIL`.
#[test]
fn an_hresult_chosen_by_the_callee_survives() {
    assert_eq!(returns_windows_error().unwrap_err().code(), E_NOINTERFACE);
}

/// `.context(...)` wraps the error; anyhow still finds the windows error
/// underneath, so adding context must not cost the HRESULT.
#[test]
fn context_does_not_swallow_the_hresult() {
    assert_eq!(
        returns_windows_error_with_context().unwrap_err().code(),
        E_UNEXPECTED
    );
}

/// The catch_unwind safety net is the whole reason this macro exists.
#[test]
fn a_panic_becomes_e_fail_instead_of_unwinding() {
    assert_eq!(panics().unwrap_err().code(), E_FAIL);
}

static SPANS: AtomicUsize = AtomicUsize::new(0);

/// Counts creations of the span named after the instrumented function.
struct SpanCounter;

impl tracing::Subscriber for SpanCounter {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::Id {
        if attrs.metadata().name() == "instrumented" {
            SPANS.fetch_add(1, Ordering::SeqCst);
        }
        tracing::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::Id, _: &tracing::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::Id) {}
    fn exit(&self, _: &tracing::Id) {}
}

/// The attribute is written *below* `#[macros::anyhow]`, exactly as the
/// TSF callbacks in crates/client do it. Before the fix it was dropped, so
/// no span was ever created — the diagnostics trace.rs is built around were
/// simply missing on the key-event path.
#[macros::anyhow]
#[tracing::instrument]
fn instrumented() -> Result<u32> {
    Ok(1)
}

#[test]
fn attributes_below_the_macro_are_preserved() {
    SPANS.store(0, Ordering::SeqCst);

    tracing::subscriber::with_default(SpanCounter, || {
        assert_eq!(instrumented(), Ok(1));
        assert_eq!(instrumented(), Ok(1));
    });

    assert_eq!(
        SPANS.load(Ordering::SeqCst),
        2,
        "#[tracing::instrument] under #[macros::anyhow] was dropped, so its \
         spans never fire"
    );
}

/// Visibility is re-emitted too; this module only compiles if `pub` survived.
mod visibility {
    use super::*;

    #[macros::anyhow]
    pub fn public_helper() -> Result<u32> {
        Ok(3)
    }
}

#[test]
fn visibility_is_preserved() {
    assert_eq!(visibility::public_helper(), Ok(3));
}
