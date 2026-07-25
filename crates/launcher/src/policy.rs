//! The two decisions the supervisor makes, as pure state machines.
//!
//! Neither touches a process, a socket or the clock: everything they need —
//! whether the watchdog killed the child, whether it was ever healthy, how
//! long it ran, and what time it is — is passed in. That is what makes the
//! budgets, the grace period and the backoff testable at all; a real
//! crash/hang loop takes minutes per round.

use std::time::{Duration, Instant};

/// give up when a child keeps crashing this many times within RESTART_WINDOW
pub(crate) const MAX_RESTARTS_IN_WINDOW: usize = 5;
pub(crate) const RESTART_WINDOW: Duration = Duration::from_secs(60);
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(8);

// -- watchdog --
/// how often the health of a child is checked
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(10);
/// hard deadline for a single health check
pub(crate) const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// this many failures in a row (after the child was healthy once) = hung
pub(crate) const MAX_CONSECUTIVE_PING_FAILURES: u32 = 3;
/// a child that never answers a single ping gets this long before it is
/// declared hung (covers dictionary/model loading at startup)
pub(crate) const STARTUP_GRACE: Duration = Duration::from_secs(120);
/// give up when the watchdog kills a child this many times in a row
/// without the child ever becoming healthy in between
pub(crate) const MAX_CONSECUTIVE_WATCHDOG_KILLS: u32 = 5;

/// What to do after a child stopped.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RestartDecision {
    /// Wait this long, then start the child again.
    RetryAfter(Duration),
    /// The watchdog killed the child over and over and it never became
    /// healthy in between.
    GiveUpHangLoop,
    /// The child crashed too often inside the crash window.
    GiveUpCrashLoop,
}

/// Pure restart policy, separated from I/O for unit testing — the same shape
/// as `WatchdogPolicy` below.
pub(crate) struct RestartPolicy {
    /// When the child was restarted, within the crash window.
    recent_restarts: Vec<Instant>,
    backoff: Duration,
    consecutive_watchdog_kills: u32,
}

impl RestartPolicy {
    pub(crate) fn new() -> Self {
        Self {
            recent_restarts: Vec::new(),
            backoff: Duration::from_secs(1),
            consecutive_watchdog_kills: 0,
        }
    }

    pub(crate) fn on_child_stopped(
        &mut self,
        hung: bool,
        saw_healthy: bool,
        ran_for: Duration,
        now: Instant,
    ) -> RestartDecision {
        // a hang loop is slower than the 60s crash window (detection alone
        // takes ~45s), so count watchdog kills separately: reaching a
        // healthy state is what proves a restart was worthwhile
        if hung && !saw_healthy {
            self.consecutive_watchdog_kills += 1;
            if self.consecutive_watchdog_kills >= MAX_CONSECUTIVE_WATCHDOG_KILLS {
                return RestartDecision::GiveUpHangLoop;
            }
        } else if saw_healthy {
            self.consecutive_watchdog_kills = 0;
        }

        // a stable AND healthy stretch resets the backoff — "alive for a
        // minute" alone would also match a server that hangs right away
        if ran_for >= RESTART_WINDOW && saw_healthy {
            self.backoff = Duration::from_secs(1);
            self.recent_restarts.clear();
        }

        self.recent_restarts
            .retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        if self.recent_restarts.len() >= MAX_RESTARTS_IN_WINDOW {
            return RestartDecision::GiveUpCrashLoop;
        }
        self.recent_restarts.push(now);

        let backoff = self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        RestartDecision::RetryAfter(backoff)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Healthy,
    Hung,
}

/// Pure hang-detection policy, separated from I/O for unit testing.
pub(crate) struct WatchdogPolicy {
    started_at: Instant,
    ever_succeeded: bool,
    consecutive_failures: u32,
}

impl WatchdogPolicy {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            ever_succeeded: false,
            consecutive_failures: 0,
        }
    }

    pub(crate) fn on_ping_result(&mut self, ok: bool, now: Instant) -> Verdict {
        if ok {
            self.ever_succeeded = true;
            self.consecutive_failures = 0;
            return Verdict::Healthy;
        }

        if !self.ever_succeeded {
            // startup grace: the pipe does not even exist while the child
            // is loading its dictionary/model, so failures don't count —
            // but a child that NEVER comes up is itself a hang
            if now.duration_since(self.started_at) <= STARTUP_GRACE {
                return Verdict::Healthy;
            }
            return Verdict::Hung;
        }

        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_CONSECUTIVE_PING_FAILURES {
            Verdict::Hung
        } else {
            Verdict::Healthy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn failures_during_startup_grace_are_not_counted() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        for i in 1..=10 {
            assert_eq!(
                policy.on_ping_result(false, at(base, i * 10)),
                Verdict::Healthy,
                "failure at {}s should be within grace",
                i * 10
            );
        }
    }

    #[test]
    fn never_becoming_healthy_past_grace_is_hung() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        assert_eq!(policy.on_ping_result(false, at(base, 60)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 121)), Verdict::Hung);
    }

    #[test]
    fn hang_needs_consecutive_failures_after_health() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        assert_eq!(policy.on_ping_result(true, at(base, 10)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 20)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 30)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 40)), Verdict::Hung);
    }

    #[test]
    fn one_success_resets_the_failure_streak() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        assert_eq!(policy.on_ping_result(true, at(base, 10)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 20)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 30)), Verdict::Healthy);
        // recovers just in time
        assert_eq!(policy.on_ping_result(true, at(base, 40)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 50)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 60)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 70)), Verdict::Hung);
    }

    #[test]
    fn success_after_grace_still_arms_normally() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        // slow startup, first success arrives after the grace window would
        // have expired for failures
        assert_eq!(
            policy.on_ping_result(false, at(base, 100)),
            Verdict::Healthy
        );
        assert_eq!(policy.on_ping_result(true, at(base, 110)), Verdict::Healthy);
        assert_eq!(
            policy.on_ping_result(false, at(base, 200)),
            Verdict::Healthy
        );
        assert_eq!(
            policy.on_ping_result(false, at(base, 210)),
            Verdict::Healthy
        );
        assert_eq!(policy.on_ping_result(false, at(base, 220)), Verdict::Hung);
    }

    /// A crash: the child exited on its own (not killed by the watchdog) and
    /// never answered a ping.
    fn crashed(policy: &mut RestartPolicy, now: Instant) -> RestartDecision {
        policy.on_child_stopped(false, false, Duration::from_secs(1), now)
    }

    #[test]
    fn the_backoff_doubles_and_stops_at_the_cap() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        // one restart per crash window, so the crash budget never fills and
        // only the backoff is under test
        let delays: Vec<Duration> = (0..6)
            .map(|i| match crashed(&mut policy, at(base, i * 61)) {
                RestartDecision::RetryAfter(delay) => delay,
                other => panic!("expected a retry, got {other:?}"),
            })
            .collect();

        assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                MAX_BACKOFF,
                MAX_BACKOFF,
            ]
        );
        assert_eq!(MAX_BACKOFF, Duration::from_secs(8), "the cap is 8s");
    }

    /// The crash budget: the fifth crash inside the window is the one that
    /// stands the launcher down, because four restarts are already on record.
    #[test]
    fn crashing_the_budget_away_inside_the_window_gives_up() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        for i in 0..MAX_RESTARTS_IN_WINDOW {
            assert!(
                matches!(
                    crashed(&mut policy, at(base, i as u64)),
                    RestartDecision::RetryAfter(_)
                ),
                "restart {i} is still within budget"
            );
        }

        assert_eq!(
            crashed(&mut policy, at(base, MAX_RESTARTS_IN_WINDOW as u64)),
            RestartDecision::GiveUpCrashLoop
        );
    }

    /// Restarts age out of the window: a child that crashes once a minute
    /// forever is unhealthy but recoverable, and the launcher must keep
    /// restarting it.
    #[test]
    fn restarts_older_than_the_window_are_not_counted() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        for i in 0..20 {
            assert!(
                matches!(
                    crashed(&mut policy, at(base, i * 61)),
                    RestartDecision::RetryAfter(_)
                ),
                "the crash at {}s stands alone in its window",
                i * 61
            );
        }
    }

    /// A restart exactly RESTART_WINDOW old is already out (`<`), so five
    /// crashes spread over just more than the window are survivable.
    #[test]
    fn a_restart_exactly_a_window_old_has_aged_out() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        for i in 0..MAX_RESTARTS_IN_WINDOW {
            crashed(&mut policy, at(base, i as u64));
        }
        // the first restart is now exactly RESTART_WINDOW old
        assert!(matches!(
            crashed(&mut policy, base + RESTART_WINDOW),
            RestartDecision::RetryAfter(_)
        ));
    }

    /// Stable AND healthy: a child that ran out the window and answered at
    /// least one ping earned a clean slate.
    #[test]
    fn a_stable_healthy_run_resets_the_backoff_and_the_budget() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        for i in 0..3 {
            crashed(&mut policy, at(base, i));
        }

        let decision = policy.on_child_stopped(false, true, RESTART_WINDOW, at(base, 100));

        assert_eq!(
            decision,
            RestartDecision::RetryAfter(Duration::from_secs(1))
        );
        // and the budget went with it: four more crashes still fit
        for i in 0..MAX_RESTARTS_IN_WINDOW - 1 {
            assert!(matches!(
                crashed(&mut policy, at(base, 101 + i as u64)),
                RestartDecision::RetryAfter(_)
            ));
        }
    }

    /// The reason the reset needs both halves: a server that comes up and
    /// hangs immediately can stay "alive" for hours without ever serving a
    /// conversion, and resetting on uptime alone would let it restart forever.
    #[test]
    fn a_long_run_that_was_never_healthy_does_not_reset_the_backoff() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        crashed(&mut policy, at(base, 0));
        let decision = policy.on_child_stopped(false, false, RESTART_WINDOW * 10, at(base, 61));

        assert_eq!(
            decision,
            RestartDecision::RetryAfter(Duration::from_secs(2)),
            "the backoff must keep growing"
        );
    }

    /// Hang detection takes ~45s per round, so a hang loop never fills the
    /// 60s crash window — hence its own counter.
    #[test]
    fn five_watchdog_kills_without_a_healthy_run_give_up() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        for i in 0..MAX_CONSECUTIVE_WATCHDOG_KILLS - 1 {
            assert!(
                matches!(
                    policy.on_child_stopped(
                        true,
                        false,
                        Duration::from_secs(45),
                        at(base, i as u64 * 100)
                    ),
                    RestartDecision::RetryAfter(_)
                ),
                "kill {i} is still within budget"
            );
        }

        assert_eq!(
            policy.on_child_stopped(
                true,
                false,
                Duration::from_secs(45),
                at(base, MAX_CONSECUTIVE_WATCHDOG_KILLS as u64 * 100)
            ),
            RestartDecision::GiveUpHangLoop
        );
    }

    /// A kill that followed a healthy stretch is not part of a hang loop: the
    /// restart did produce a working engine, so the streak starts over.
    #[test]
    fn a_healthy_run_resets_the_watchdog_kill_streak() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        for i in 0..MAX_CONSECUTIVE_WATCHDOG_KILLS - 1 {
            policy.on_child_stopped(
                true,
                false,
                Duration::from_secs(45),
                at(base, i as u64 * 100),
            );
        }
        // this one served requests before it hung
        policy.on_child_stopped(true, true, Duration::from_secs(45), at(base, 1000));

        // so the counter starts from zero again rather than tripping here
        assert!(matches!(
            policy.on_child_stopped(true, false, Duration::from_secs(45), at(base, 1100)),
            RestartDecision::RetryAfter(_)
        ));
    }

    /// The hang counter is not touched by ordinary crashes, and the crash
    /// budget is what catches those.
    #[test]
    fn a_crash_neither_advances_nor_resets_the_watchdog_kill_streak() {
        let base = Instant::now();
        let mut policy = RestartPolicy::new();

        for i in 0..MAX_CONSECUTIVE_WATCHDOG_KILLS - 1 {
            policy.on_child_stopped(
                true,
                false,
                Duration::from_secs(45),
                at(base, i as u64 * 100),
            );
        }
        // a plain crash in between: never healthy, but not a watchdog kill
        crashed(&mut policy, at(base, 900));

        assert_eq!(
            policy.on_child_stopped(true, false, Duration::from_secs(45), at(base, 1000)),
            RestartDecision::GiveUpHangLoop,
            "the streak is unbroken"
        );
    }
}
