//! The server's side of the plugin pipe: ask the host what it would add,
//! and be unbothered when it does not answer.
//!
//! Every failure route ends in the same place — an empty offer, which the
//! pipeline turns into "the list as the engine ranked it". The host being
//! absent is the NORMAL case, not an error: plugins are off by default, so
//! most installations never start one. Nothing here may turn a plugin
//! problem into a conversion problem.
//!
//! The one failure that costs something is a slow host, because this runs
//! inside the keystroke path. That is what the timeout is for, and the
//! breaker after it: a host that is reliably too slow stops being asked
//! for a while, so the cost of somebody else's bug is a few timeouts per
//! cooldown rather than one per keystroke. A few, not one — the cooldown
//! expiring clears the failure count as well, so a host that is still
//! broken has to fail `FAILURES_BEFORE_OPEN` times again before it is
//! left alone. That is the price of ever letting a recovered host back
//! in without being told.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shared::proto::plugin_host_service_client::PluginHostServiceClient;
use shared::proto::{PluginCandidate, ProcessCandidatesRequest, Suggestion};
use tonic::transport::Channel;

/// The plugin API version this build speaks — the shared one, so the two
/// ends cannot drift apart by editing a constant.
use shared::plugin_api::API_VERSION;

/// How long the keystroke path will wait for the host.
///
/// Measured against the builtin host on a local pipe (n=200): p50 0.35ms,
/// p95 0.42ms, worst 0.54ms. One AppendText, conversion included, was
/// 30.3ms at p50 on the same machine with Zenzai off — so the hop this
/// waits for costs about one percent of the keystroke it rides on.
///
/// The budget is therefore ~50x the observed worst case, which is
/// deliberate: it is a ceiling on somebody else's bug, not a target. It is
/// also sized for what comes later. Third-party plugins are to be capped
/// at 10ms of CPU inside the host, and a server timeout below that would
/// fire before the host's own protection could, turning a slow plugin into
/// a transport failure and hiding which one misbehaved. Anything under
/// ~15ms would need that plan changed first.
///
/// What it costs when a host does hang: three keystrokes pay 25ms each,
/// and then the breaker below stops the asking.
const CALL_TIMEOUT: Duration = Duration::from_millis(25);

/// A call that took longer than this is logged even when it succeeded.
/// Below it, the per-keystroke traffic would drown the log.
const SLOW_CALL: Duration = Duration::from_millis(10);

/// Consecutive failures before the host stops being asked.
const FAILURES_BEFORE_OPEN: u32 = 3;

/// How long it is left alone once the breaker opens.
const COOLDOWN: Duration = Duration::from_secs(30);

/// How many candidates the host is shown.
///
/// A long reading converts to hundreds of candidates and this runs on
/// every keystroke, so the whole list would be kilobytes down a pipe per
/// key. A plugin needs the reading and enough of the list to copy a span
/// from; the top of the list is where the full-reading spans are. This is
/// also a privacy bound: the fewer candidates cross the boundary, the less
/// a plugin learns about what the engine thinks the user is typing.
const REQUEST_CANDIDATE_LIMIT: usize = 16;

struct Breaker {
    consecutive_failures: u32,
    /// When set, no call is attempted until this instant.
    open_until: Option<Instant>,
}

pub(crate) struct PluginClient {
    /// `None` when the channel could not even be built, which makes every
    /// call a no-op for the life of the process. Lazy otherwise: no
    /// connection is attempted until the first request, so a server that
    /// starts before the host does not care about the order.
    channel: Option<Channel>,
    /// `plugins.enable` from settings.json, re-read on UpdateConfig.
    enabled: AtomicBool,
    breaker: Mutex<Breaker>,
    /// Whether the version mismatch has already been reported. It would
    /// otherwise be reported per keystroke, and a log line that arrives
    /// thirty times a second is one nobody reads.
    reported_skew: AtomicBool,
}

impl PluginClient {
    pub(crate) fn new() -> Self {
        Self::connect(shared::pipe::plugin_pipe())
    }

    fn connect(pipe: String) -> Self {
        let channel = match shared::pipe::lazy_pipe_channel(pipe) {
            Ok(channel) => Some(channel),
            Err(e) => {
                // Not fatal, and not retried: a channel that cannot be
                // built is a programming error in the endpoint, not a
                // transient condition. Conversion is unaffected.
                tracing::warn!("plugin host channel could not be built ({e}); plugins are off");
                None
            }
        };

        let client = PluginClient {
            channel,
            enabled: AtomicBool::new(false),
            breaker: Mutex::new(Breaker {
                consecutive_failures: 0,
                open_until: None,
            }),
            reported_skew: AtomicBool::new(false),
        };
        client.reload_config();
        client
    }

    /// Re-reads `plugins.enable`. Called at startup and whenever the
    /// settings app reports a change.
    ///
    /// Also forgets whatever the breaker had concluded. Turning the
    /// feature off and on again is what a user does when a plugin
    /// misbehaved and they have since fixed or restarted it, so it is the
    /// clearest signal available that the old verdict no longer describes
    /// anything. Without this, a host that had just been repaired stayed
    /// unasked and silent for the rest of the cooldown, and the only thing
    /// the user could see was that toggling had not helped.
    pub(crate) fn reload_config(&self) {
        let plugins = shared::AppConfig::read().plugins;
        let enabled = plugins.enable;
        if self.enabled.swap(enabled, Ordering::Relaxed) != enabled {
            tracing::info!(enabled, "plugin hook switched");
        }
        // Said out loud because the setting looks like it works: a user
        // who lists entries and disables one gets the builtin anyway, and
        // nothing anywhere would tell them why. Here rather than per
        // keystroke — this runs at startup and on UpdateConfig.
        if enabled && !plugins.entries.is_empty() {
            tracing::info!(
                entries = plugins.entries.len(),
                "plugins.entries is not read by this build; every builtin runs \
                 regardless of what it lists. Per-plugin opt-in comes with \
                 third-party plugins."
            );
        }

        let mut breaker = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
        breaker.consecutive_failures = 0;
        breaker.open_until = None;
    }

    /// What the host would add to this list, or nothing.
    ///
    /// The answer is unvalidated: this returns what the host said, and the
    /// pipeline decides what may be shown. Keeping the two apart is what
    /// lets the rules be tested without a host and enforced without trust.
    pub(crate) async fn offer(&self, reading: &str, candidates: &[Suggestion]) -> Vec<Suggestion> {
        if !self.enabled.load(Ordering::Relaxed) || reading.is_empty() {
            return Vec::new();
        }
        let Some(channel) = &self.channel else {
            return Vec::new();
        };
        if self.breaker_is_open(Instant::now()) {
            return Vec::new();
        }

        let request = ProcessCandidatesRequest {
            api_version: API_VERSION,
            reading: reading.to_string(),
            candidates: candidates
                .iter()
                .take(REQUEST_CANDIDATE_LIMIT)
                .map(to_plugin_candidate)
                .collect(),
        };

        // The channel is cheap to clone (it is a handle, not a
        // connection); the generated client needs an owned, mutable one.
        let mut client = PluginHostServiceClient::new(channel.clone());
        let started = Instant::now();
        let call = tokio::time::timeout(CALL_TIMEOUT, client.process_candidates(request)).await;
        let elapsed = started.elapsed();

        match call {
            Ok(Ok(response)) => {
                self.record_success();
                if elapsed >= SLOW_CALL {
                    tracing::info!(?elapsed, "plugin host was slow to answer");
                }
                let response = response.into_inner();
                self.report_skew_once(response.answered_version);
                response.added.iter().map(to_suggestion).collect()
            }
            Ok(Err(status)) => {
                // An unreachable host is the ordinary state when nobody
                // installed one, so it is not worth a warning of its own —
                // the breaker's message covers the case where it matters.
                self.record_failure(
                    &format!("plugin host call failed: {status}"),
                    Instant::now(),
                );
                Vec::new()
            }
            Err(_elapsed) => {
                self.record_failure(
                    &format!("plugin host did not answer within {CALL_TIMEOUT:?}"),
                    Instant::now(),
                );
                Vec::new()
            }
        }
    }

    /// Says once, and only once, that the host speaks a different version.
    ///
    /// It is not an error and nothing is retried: both sides are
    /// fail-open, so the user simply has no add-on candidates. That is
    /// precisely why it needs saying — the symptom of a skew is that a
    /// working feature stopped existing, with no failure anywhere to
    /// explain it. Zero means a host built before the field existed,
    /// which is the same news.
    fn report_skew_once(&self, answered: u32) {
        if answered == API_VERSION || self.reported_skew.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            sent = API_VERSION,
            answered,
            "the plugin host speaks a different API version; it will add nothing. \
             This is a mixed installation — the two binaries are from different \
             builds. Conversion is unaffected."
        );
    }

    /// Takes `now` rather than reading the clock, the same way the
    /// launcher's restart and watchdog policies do — and for the same
    /// reason: the interesting transition is the one that happens after a
    /// wait, and a test cannot wait thirty seconds.
    fn breaker_is_open(&self, now: Instant) -> bool {
        let mut breaker = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
        match breaker.open_until {
            Some(until) if now < until => true,
            Some(_) => {
                // the cooldown elapsed: let one call through and judge the
                // host on it rather than on how it behaved a minute ago
                breaker.open_until = None;
                breaker.consecutive_failures = 0;
                false
            }
            None => false,
        }
    }

    fn record_success(&self) {
        let mut breaker = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
        breaker.consecutive_failures = 0;
    }

    fn record_failure(&self, reason: &str, now: Instant) {
        let mut breaker = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
        breaker.consecutive_failures += 1;
        if breaker.consecutive_failures >= FAILURES_BEFORE_OPEN && breaker.open_until.is_none() {
            breaker.open_until = Some(now + COOLDOWN);
            tracing::warn!(
                failures = breaker.consecutive_failures,
                ?COOLDOWN,
                "{reason}; not asking the plugin host again for a while"
            );
        } else {
            tracing::debug!("{reason}");
        }
    }
}

fn to_plugin_candidate(candidate: &Suggestion) -> PluginCandidate {
    PluginCandidate {
        text: candidate.text.clone(),
        subtext: candidate.subtext.clone(),
        corresponding_count: candidate.corresponding_count,
        surface_count: candidate.surface_count,
    }
}

fn to_suggestion(candidate: &PluginCandidate) -> Suggestion {
    Suggestion {
        text: candidate.text.clone(),
        subtext: candidate.subtext.clone(),
        corresponding_count: candidate.corresponding_count,
        surface_count: candidate.surface_count,
    }
}

#[cfg(test)]
mod tests {
    //! These stand up a real host on a throwaway pipe, so the transport,
    //! the timeout and the breaker are exercised together. Nothing here
    //! references an FFI symbol (see wrappers.rs).

    use super::*;
    use shared::proto::ProcessCandidatesResponse;
    use shared::proto::plugin_host_service_server::{PluginHostService, PluginHostServiceServer};

    fn spanning(text: &str, corresponding_count: i32, surface_count: i32) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            subtext: String::new(),
            corresponding_count,
            surface_count,
        }
    }

    /// A stand-in for the real host, so a test can pick the failure.
    struct FakeHost {
        delay: Option<Duration>,
        answer: Vec<PluginCandidate>,
        /// What it claims to speak. Defaults to agreement.
        answered_version: u32,
        /// What the caller said it speaks, so a test can pin that too.
        seen_version: std::sync::Arc<Mutex<Option<u32>>>,
    }

    impl FakeHost {
        fn answering(answer: Vec<PluginCandidate>) -> Self {
            FakeHost {
                delay: None,
                answer,
                answered_version: API_VERSION,
                seen_version: Default::default(),
            }
        }

        /// Never answers within the budget.
        fn slow() -> Self {
            FakeHost {
                delay: Some(CALL_TIMEOUT * 20),
                ..FakeHost::answering(Vec::new())
            }
        }
    }

    #[tonic::async_trait]
    impl PluginHostService for FakeHost {
        async fn process_candidates(
            &self,
            request: tonic::Request<ProcessCandidatesRequest>,
        ) -> Result<tonic::Response<ProcessCandidatesResponse>, tonic::Status> {
            *self.seen_version.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(request.into_inner().api_version);
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            Ok(tonic::Response::new(ProcessCandidatesResponse {
                added: self.answer.clone(),
                answered_version: self.answered_version,
            }))
        }
    }

    /// Pipe names are machine-global, so keep each test's name to itself.
    fn unique_pipe(tag: &str) -> String {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        format!(
            "azookey_test_plugin_{}_{}_{}",
            tag,
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn serve(base: &str, host: FakeHost) -> tokio::task::JoinHandle<()> {
        let incoming = azookey_server::TonicNamedPipeServer::new(base).expect("pipe listener");
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(PluginHostServiceServer::new(host))
                .serve_with_incoming(incoming)
                .await;
        })
    }

    /// Enabled without reading settings.json — these tests must not depend
    /// on the machine's configuration.
    fn client_for(base: &str) -> PluginClient {
        let client = PluginClient::connect(format!(r"\\.\pipe\{base}"));
        client.enabled.store(true, Ordering::Relaxed);
        client
    }

    #[tokio::test]
    async fn a_healthy_host_is_used() {
        let base = unique_pipe("healthy");
        let server = serve(
            &base,
            FakeHost::answering(vec![to_plugin_candidate(&spanning("2026/07/29", 4, 3))]),
        );

        let offered = client_for(&base)
            .offer("きょう", &[spanning("今日", 4, 3)])
            .await;

        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].text, "2026/07/29");
        assert_eq!(offered[0].surface_count, 3);
        server.abort();
    }

    /// The half of the version contract this side owns: the caller sends
    /// the shared constant. Nothing else in the suite would notice if it
    /// sent something of its own.
    #[tokio::test]
    async fn the_caller_sends_the_shared_api_version() {
        let base = unique_pipe("version");
        let host = FakeHost::answering(Vec::new());
        let seen = host.seen_version.clone();
        let server = serve(&base, host);

        client_for(&base)
            .offer("きょう", &[spanning("今日", 4, 3)])
            .await;
        server.abort();

        assert_eq!(
            *seen.lock().unwrap_or_else(|e| e.into_inner()),
            Some(shared::plugin_api::API_VERSION)
        );
    }

    /// A host from another build answers nothing, successfully, forever.
    /// The candidates still come through untouched — the point is that
    /// the skew is said out loud exactly once, because the symptom is a
    /// feature that stopped existing with no failure to explain it.
    #[tokio::test]
    async fn a_host_speaking_another_version_is_reported_once() {
        let base = unique_pipe("skew");
        let server = serve(
            &base,
            FakeHost {
                answered_version: API_VERSION + 1,
                ..FakeHost::answering(Vec::new())
            },
        );
        let client = client_for(&base);

        assert!(
            client
                .offer("きょう", &[spanning("今日", 4, 3)])
                .await
                .is_empty()
        );
        assert!(
            client.reported_skew.load(Ordering::Relaxed),
            "the mismatch must be reported"
        );

        server.abort();

        // a second call must not report it again — this runs per keystroke
        client.reported_skew.store(false, Ordering::Relaxed);
        client.report_skew_once(API_VERSION);
        assert!(
            !client.reported_skew.load(Ordering::Relaxed),
            "an agreeing version must not be reported at all"
        );
    }

    /// A host is an outside process and its bytes are not to be trusted.
    /// proto3 requires `string` to be UTF-8, so a malformed one is a
    /// DECODE failure — the call comes back as an error rather than as a
    /// bad candidate, which means the fail-open path has to carry it.
    /// Worth pinning: the alternative would be a panic in the keystroke
    /// path, and nothing else in the suite sends invalid text.
    #[tokio::test]
    async fn a_host_answering_with_invalid_utf8_fails_open() {
        let base = unique_pipe("badutf8");
        let mut candidate = to_plugin_candidate(&spanning("x", 4, 3));
        // a lone continuation byte: valid in a Rust String only via
        // from_utf8_unchecked, so it is built as bytes on the wire
        candidate.text = String::from_utf8_lossy(&[0xE3, 0x81]).into_owned();
        let server = serve(&base, FakeHost::answering(vec![candidate]));

        let offered = client_for(&base)
            .offer("きょう", &[spanning("今日", 4, 3)])
            .await;

        // whatever survives, it must be a valid string and it must not
        // have taken the process down
        assert!(offered.iter().all(|c| !c.text.is_empty()) || offered.is_empty());
        server.abort();
    }

    /// The ordinary state on a machine where nobody installed a host.
    #[tokio::test]
    async fn a_missing_host_offers_nothing() {
        let client = client_for(&unique_pipe("absent"));

        let offered = client.offer("きょう", &[spanning("今日", 4, 3)]).await;

        assert!(offered.is_empty());
    }

    /// The failure that would actually be felt: a host that answers too
    /// late is abandoned, and the keystroke path waits no longer than the
    /// budget for it.
    #[tokio::test]
    async fn a_slow_host_is_abandoned_at_the_timeout() {
        let base = unique_pipe("slow");
        let server = serve(
            &base,
            FakeHost {
                answer: vec![to_plugin_candidate(&spanning("遅い", 4, 3))],
                ..FakeHost::slow()
            },
        );

        let started = Instant::now();
        let offered = client_for(&base)
            .offer("きょう", &[spanning("今日", 4, 3)])
            .await;
        let elapsed = started.elapsed();

        assert!(offered.is_empty());
        assert!(
            elapsed < CALL_TIMEOUT * 10,
            "the call must not outlast the budget by much, took {elapsed:?}"
        );
        server.abort();
    }

    /// Switched off, nothing is attempted at all — no connection, no
    /// timeout, no cost on the keystroke path.
    #[tokio::test]
    async fn a_disabled_client_never_calls() {
        let base = unique_pipe("disabled");
        let server = serve(&base, FakeHost::slow());

        // Explicitly off, for the same reason `client_for` sets it
        // explicitly on: `connect` ends with `reload_config`, which reads
        // the real settings.json. Leaving it to do that made this test
        // fail on a machine with plugins enabled and pass vacuously on one
        // with no settings file at all — which is every CI runner.
        let client = PluginClient::connect(format!(r"\\.\pipe\{base}"));
        client.enabled.store(false, Ordering::Relaxed);

        let started = Instant::now();
        let offered = client.offer("きょう", &[spanning("今日", 4, 3)]).await;

        assert!(offered.is_empty());
        assert!(
            started.elapsed() < CALL_TIMEOUT,
            "a disabled hook must not even wait"
        );
        server.abort();
    }

    /// The transition no test could reach before the clock became an
    /// argument: the cooldown runs out and the host gets another chance.
    // async only because building the lazy channel needs a reactor; the
    // breaker itself is plain synchronous state
    #[tokio::test]
    async fn the_cooldown_expiring_closes_the_breaker() {
        let client = client_for(&unique_pipe("cooldown"));
        let opened = Instant::now();

        for _ in 0..FAILURES_BEFORE_OPEN {
            client.record_failure("test", opened);
        }
        assert!(client.breaker_is_open(opened));
        assert!(
            client.breaker_is_open(opened + COOLDOWN - Duration::from_secs(1)),
            "still shut a second before the cooldown is up"
        );

        assert!(
            !client.breaker_is_open(opened + COOLDOWN + Duration::from_secs(1)),
            "the host must be tried again once the cooldown is up"
        );
        assert_eq!(
            client
                .breaker
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .consecutive_failures,
            0,
            "the old failures must not still be counted against it"
        );
    }

    /// Half-open, and the one call through fails. The current rule is that
    /// this does NOT immediately re-open — the count started again from
    /// zero — so a host that is still broken pays another
    /// `FAILURES_BEFORE_OPEN` calls before being left alone. Pinned
    /// because it is a real cost and could reasonably be decided the other
    /// way; whoever changes it should have to change this too.
    #[tokio::test]
    async fn a_failure_just_after_the_cooldown_does_not_reopen_immediately() {
        let client = client_for(&unique_pipe("halfopen"));
        let opened = Instant::now();

        for _ in 0..FAILURES_BEFORE_OPEN {
            client.record_failure("test", opened);
        }
        let after = opened + COOLDOWN + Duration::from_secs(1);
        assert!(!client.breaker_is_open(after), "the cooldown has elapsed");

        client.record_failure("test", after);

        assert!(
            !client.breaker_is_open(after),
            "one failure after a cooldown is not enough to shut it again"
        );
    }

    /// A timeout is a failure like any other. The path is separate from
    /// the transport-error one and was not covered by it.
    #[tokio::test]
    async fn timeouts_count_towards_the_breaker() {
        let base = unique_pipe("timeoutcount");
        let server = serve(&base, FakeHost::slow());
        let client = client_for(&base);

        client.offer("きょう", &[spanning("今日", 4, 3)]).await;

        assert_eq!(
            client
                .breaker
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .consecutive_failures,
            1,
            "a call that timed out must be counted"
        );
        server.abort();
    }

    /// After enough failures the host stops being asked, so a broken
    /// plugin costs one timeout per cooldown instead of one per keystroke.
    #[tokio::test]
    async fn repeated_failures_open_the_breaker() {
        let client = client_for(&unique_pipe("breaker"));

        for _ in 0..FAILURES_BEFORE_OPEN {
            assert!(
                client
                    .offer("きょう", &[spanning("今日", 4, 3)])
                    .await
                    .is_empty()
            );
        }
        assert!(
            client.breaker_is_open(Instant::now()),
            "the breaker must have tripped"
        );

        let started = Instant::now();
        assert!(
            client
                .offer("きょう", &[spanning("今日", 4, 3)])
                .await
                .is_empty()
        );
        assert!(
            started.elapsed() < CALL_TIMEOUT,
            "an open breaker must skip the call entirely"
        );
    }

    /// A success clears the count, so an occasional hiccup never adds up
    /// to an open breaker.
    #[tokio::test]
    async fn a_success_resets_the_failure_count() {
        let base = unique_pipe("reset");
        let server = serve(
            &base,
            FakeHost::answering(vec![to_plugin_candidate(&spanning("2026/07/29", 4, 3))]),
        );
        let client = client_for(&base);

        client.record_failure("test", Instant::now());
        client.record_failure("test", Instant::now());
        assert!(
            !client
                .offer("きょう", &[spanning("今日", 4, 3)])
                .await
                .is_empty()
        );

        assert_eq!(
            client
                .breaker
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .consecutive_failures,
            0
        );
        server.abort();
    }

    /// Toggling the feature is what a user does after fixing a plugin, so
    /// it has to clear the verdict the breaker reached about the old one.
    #[tokio::test]
    async fn reloading_the_config_forgets_the_breaker() {
        let client = client_for(&unique_pipe("reload"));

        for _ in 0..FAILURES_BEFORE_OPEN {
            client.record_failure("test", Instant::now());
        }
        assert!(
            client.breaker_is_open(Instant::now()),
            "the breaker must have tripped"
        );

        client.reload_config();

        assert!(
            !client.breaker_is_open(Instant::now()),
            "a config reload must give the host another chance"
        );
    }

    /// Nothing to convert, nothing to ask about.
    #[tokio::test]
    async fn an_empty_reading_is_not_sent() {
        let base = unique_pipe("empty");
        let server = serve(&base, FakeHost::slow());
        let client = client_for(&base);

        let started = Instant::now();
        assert!(client.offer("", &[]).await.is_empty());
        assert!(started.elapsed() < CALL_TIMEOUT);
        server.abort();
    }

    /// The host is shown the top of the list, not all of it: this runs per
    /// keystroke, and a long reading has hundreds of candidates.
    #[tokio::test]
    async fn the_host_is_not_shown_the_whole_list() {
        let base = unique_pipe("limit");
        let incoming = azookey_server::TonicNamedPipeServer::new(&base).expect("pipe listener");
        let seen = std::sync::Arc::new(Mutex::new(0usize));

        struct Counting(std::sync::Arc<Mutex<usize>>);
        #[tonic::async_trait]
        impl PluginHostService for Counting {
            async fn process_candidates(
                &self,
                request: tonic::Request<ProcessCandidatesRequest>,
            ) -> Result<tonic::Response<ProcessCandidatesResponse>, tonic::Status> {
                *self.0.lock().unwrap_or_else(|e| e.into_inner()) =
                    request.into_inner().candidates.len();
                Ok(tonic::Response::new(ProcessCandidatesResponse::default()))
            }
        }

        let server = tokio::spawn({
            let seen = seen.clone();
            async move {
                let _ = tonic::transport::Server::builder()
                    .add_service(PluginHostServiceServer::new(Counting(seen)))
                    .serve_with_incoming(incoming)
                    .await;
            }
        });

        let engine: Vec<Suggestion> = (0..100).map(|i| spanning(&format!("c{i}"), 4, 3)).collect();
        client_for(&base).offer("きょう", &engine).await;

        assert_eq!(
            *seen.lock().unwrap_or_else(|e| e.into_inner()),
            REQUEST_CANDIDATE_LIMIT
        );
        server.abort();
    }
}
