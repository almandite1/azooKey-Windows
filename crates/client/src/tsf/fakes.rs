//! The fake TSF host, one module per host object it stands in for.
//!
//! Split out of `test_support.rs`, which had grown to 1600 lines holding all
//! four. `test_support` is still the entry point every test imports from and
//! re-exports everything here; these modules are where the fakes live.
//!
//! `range::RangeLog` is the only state shared between them — a composition
//! (range.rs) and a context (context.rs) hand out ranges that report into the
//! same log.

pub(super) mod compartment;
pub(super) mod context;
pub(super) mod range;
pub(super) mod thread_mgr;
