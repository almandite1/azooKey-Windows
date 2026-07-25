//! Running a sequence of steps where **every** step must be attempted.
//!
//! Teardown is where this matters. A sink left advised leaks into the host
//! application, an unreleased key reservation keeps the key, and a skipped
//! registry cleanup outlives the uninstall — so a step that failed must not
//! stop the ones after it. Four places had grown their own copy of the same
//! "log it, keep the first error, keep going" loop (`Deactivate`'s two
//! teardown halves, `unadvise_compartment_sinks`, `unpreserve_keys` and
//! `DllUnregisterServer`); this is that loop, once.

use anyhow::Result;

/// Accumulator for a best-effort sequence: [`step`](Self::step) logs and
/// swallows each failure, and [`finish`](Self::finish) reports the first one
/// so the caller still fails overall.
pub struct BestEffort {
    /// names the sequence in the log lines and in the reported error
    what: &'static str,
    first_error: Option<anyhow::Error>,
    failures: usize,
}

impl BestEffort {
    pub fn new(what: &'static str) -> Self {
        Self {
            what,
            first_error: None,
            failures: 0,
        }
    }

    /// Runs one step's result through the accumulator. Failures are logged
    /// where they happen, because only the first one survives to the caller
    /// and the later ones are just as diagnostic.
    pub fn step(&mut self, result: Result<()>) {
        if let Err(error) = result {
            tracing::warn!("{} step failed: {error:?}", self.what);
            self.failures += 1;
            if self.first_error.is_none() {
                self.first_error = Some(error);
            }
        }
    }

    /// The first failure, if any — with the total count attached, so a caller
    /// that only ever sees one error still knows how many there were.
    pub fn finish(self) -> Result<()> {
        match self.first_error {
            None => Ok(()),
            Some(error) if self.failures == 1 => Err(error),
            Some(error) => Err(error.context(format!(
                "{} had {} failing steps; this is the first",
                self.what, self.failures
            ))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_sequence_succeeds() {
        let mut steps = BestEffort::new("teardown");
        steps.step(Ok(()));
        steps.step(Ok(()));
        assert!(steps.finish().is_ok());
    }

    /// The point of the type: a failing step must not stop the caller from
    /// running the rest, and the failure must still surface at the end.
    #[test]
    fn the_first_failure_is_what_surfaces() {
        let mut steps = BestEffort::new("teardown");
        steps.step(Err(anyhow::anyhow!("first")));
        steps.step(Ok(()));
        steps.step(Err(anyhow::anyhow!("second")));

        let error = steps.finish().expect_err("a failing step must surface");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("first"), "{rendered}");
        assert!(
            rendered.contains("2 failing steps"),
            "the count of the ones that were swallowed must not be lost: {rendered}"
        );
    }

    /// A lone failure is reported as itself: no wrapping that would push the
    /// real message down a level for no reason.
    #[test]
    fn a_single_failure_is_not_wrapped() {
        let mut steps = BestEffort::new("teardown");
        steps.step(Err(anyhow::anyhow!("only")));
        let error = steps.finish().expect_err("a failing step must surface");
        assert_eq!(format!("{error:#}"), "only");
    }
}
