//! Rewriting text for the ear before it is spoken.
//!
//! Everything in this file is in the **default build**: the trait, the
//! deadline, the semaphore, the circuit breakers and the orchestration.
//! Only [`http`] -- the `ureq` client -- is behind `#[cfg(feature =
//! "reword")]`, which is what lets every rule here be tested with a struct
//! holding a `Vec` and no network at all.
//!
//! # The two rules that are load-bearing
//!
//! **`tokio::time::timeout` abandons the `.await`, never the task behind
//! it.** The client is blocking, so the request runs on
//! `tokio::task::spawn_blocking` and the *only* thing that bounds that
//! thread is the client's own [`REWORD_HTTP_CEILING`]. This is
//! `NotifyEnabledWatch`'s CRITICAL 1, measured on this daemon: a 250 ms
//! timeout that abandoned an await but not its task took the process from
//! 30 to 548 blocking threads in three and a half minutes, hit tokio's
//! 512-thread cap, and left `Say` over D-Bus never returning.
//!
//! **The permit is held by the blocking job to completion**, not released
//! when the deadline fires. A permit released at the deadline would let a
//! slow provider accumulate blocking threads at the arrival rate while
//! pretending to be bounded. Held to completion, the worst case is exactly
//! [`REWORD_MAX_INFLIGHT`] blocking threads, each living at most
//! [`REWORD_HTTP_CEILING`].
//!
//! # Why a late answer cannot be spoken
//!
//! [`reword_or_original`] returns a `String` and holds no `EngineHandle`.
//! `submit` therefore happens in the caller's scope, never inside the
//! rewrite job: when the deadline fires the `.await` on the job's
//! `JoinHandle` is abandoned and the value it eventually produces has
//! nowhere to go. It is dropped because nothing is left holding the
//! receiving end, not because of a check that could be forgotten.
//!
//! What that costs, exactly: an async client would have its future
//! *cancelled* at the deadline -- the request stops, the socket closes, the
//! work stops costing anything. The blocking job instead runs to completion
//! (or to the ceiling) with its result discarded: a wasted request and an
//! occupied thread. What does not differ is correctness. Buying
//! cancellation would have cost 40 additional crates.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use sayd_core::config::RewordConfig;
use sayd_core::reword::{check, eligible, Ineligible};

#[cfg(feature = "reword")]
pub mod http;

/// The client's own ceiling on one request. Set on the `ureq` agent, so it
/// bounds the *thread*, which `tokio::time::timeout` cannot.
pub const REWORD_HTTP_CEILING: Duration = Duration::from_secs(10);

/// What the settings window's Test button waits, rather than `timeout_ms`.
///
/// A test that gave up at the configured deadline could only ever say "too
/// slow" and could never say *how much* too slow -- which is the number
/// needed to choose a better deadline, or to conclude the provider is
/// hopeless. Equal to [`REWORD_HTTP_CEILING`] because the client's own
/// ceiling is what actually ends the request; there is nothing to be gained
/// by waiting longer and nothing to be gained by giving up sooner.
// `#[allow(dead_code)]`: the settings window's Test row is a later task in
// this milestone, and it is the only caller. Produced now because it is part
// of the interface that task expects, and because its value is an argument
// about `REWORD_HTTP_CEILING` that belongs beside it.
#[allow(dead_code)]
pub const REWORD_TEST_CEILING: Duration = REWORD_HTTP_CEILING;

/// How many rewrites may be in flight at once, across every path --
/// notifications, `--reword`, and the settings window's Test button.
///
/// Two rather than eight because the per-application cooldown already caps
/// the arrival rate; more than a couple in flight means the provider is
/// slow, and when the provider is slow the right answer is the raw text
/// now. When no permit is free the text is submitted raw *immediately*
/// rather than queued behind another rewrite: backpressure degrades to the
/// same behaviour a slow provider already degrades to, so the failure mode
/// is one this design already promises.
pub const REWORD_MAX_INFLIGHT: usize = 2;

/// Consecutive transport failures before the breaker opens (§8).
const TRANSPORT_FAILURES_TO_OPEN: u32 = 3;
/// How long it stays open before one request is let through.
const TRANSPORT_BREAKER_COOLDOWN: Duration = Duration::from_secs(60);
/// Backoff after a 429 with no `Retry-After`.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);
/// The longest a `Retry-After` may hold the breaker shut.
///
/// DEPARTURE, and the reason is the boundary rule this module is built on:
/// `retry_after` is the one breaker input that comes straight out of a
/// provider's response. Unclamped it reaches `Instant::checked_add`, and a
/// header large enough to overflow that yields `None` -- which this code
/// would read as *no backoff at all*, so the one response that most wants
/// backing off from would get none. An hour is longer than any real
/// `Retry-After` this daemon will meet and short enough that a provider
/// cannot switch the feature off for the life of the process.
const RATE_LIMIT_MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Missed deadlines are counted; the first is logged, then every Nth.
const DEADLINE_LOG_EVERY: u64 = 50;
/// How much of a rejected candidate reaches the debug log.
const DEBUG_SNIPPET_CHARS: usize = 80;

/// Poison-tolerant, for the reason `settings::model::lock` gives: this state
/// is reached from GTK signal handlers (the Test row), glib calls those
/// through an `extern "C"` frame, and a panic in one of those aborts the
/// daemon outright rather than unwinding. Nothing under this lock can be
/// left half-updated -- every field is replaced whole -- so reading through
/// the poison is safe as well as necessary.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Print a diagnostic line, but only when `SAYD_DEBUG` is set and non-empty.
///
/// This daemon has no log crate and three prefixes by convention (`info:`,
/// `warning:`, `error:`). §7 asks for rejected candidates at *debug* -- the
/// string is what diagnosing a bad guard needs, and printing it on every
/// rejection would duplicate locally what is being sent remotely, which
/// helps nobody. An environment variable is what "debug level" means here.
pub fn debug(args: std::fmt::Arguments<'_>) {
    if std::env::var_os("SAYD_DEBUG").is_some_and(|v| !v.is_empty()) {
        eprintln!("debug: {args}");
    }
}

/// Why a rewrite did not happen. Every variant ends in "speak the original";
/// they are distinguished so the settings window's Test row can tell a user
/// which one they have, because from outside the daemon every one of them
/// looks identical -- and identical to the feature being switched off.
// `#[allow(dead_code)]`: in a build without the `reword` feature nothing
// constructs most of these, which is the whole point of the split -- the
// classification, the breakers that fold it in and the tests that pin their
// behaviour all live in the default build, and only the client that produces
// the variants is gated. The next task constructs them from HTTP responses.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewordError {
    /// `base_url` is empty or does not parse. Carries the reason, naming
    /// the field.
    NotConfigured(String),
    /// Built without the `reword` feature.
    Unavailable,
    /// 401 or 403. Latches the breaker until the config changes.
    Auth {
        status: u16,
        host: String,
        message: Option<String>,
    },
    /// 404, or an `error` body naming the model at any status. The single
    /// most common misconfiguration on a local server, and invisible
    /// without this.
    NoSuchModel {
        status: u16,
        model: String,
        message: Option<String>,
    },
    RateLimited {
        retry_after: Option<Duration>,
        message: Option<String>,
    },
    /// DNS, connect or TLS.
    Unreachable(String),
    /// The client's own [`REWORD_HTTP_CEILING`] was hit. Counted toward the
    /// same breaker as [`RewordError::Unreachable`]: this row exists to
    /// bound the thread, not the utterance -- the utterance was spoken
    /// 8.5 s earlier.
    Ceiling,
    /// No `choices[0].message.content`, an unparseable body, or an `error`
    /// object that classifies as nothing more specific.
    Malformed(String),
}

/// The seam. One synchronous method, defined by us, so a test double is a
/// struct with a `Vec` of canned outcomes and no runtime at all.
///
/// The signature is the boundary: an implementation may only *return* --
/// there is no `&mut` state to corrupt, no channel to speak down, and every
/// way it can fail is a [`RewordError`]. An implementation that panics
/// anyway is contained by [`attempt`], which turns the `JoinError` into
/// [`RewordError::Malformed`], so the worst a provider's response can do to
/// the announcement is cost it a rewrite.
pub trait Rewriter: Send + Sync {
    fn reword(&self, text: &str) -> Result<String, RewordError>;
}

/// What one bounded attempt produced.
#[derive(Debug)]
pub enum Attempt {
    /// The job answered inside the budget.
    Answered(Result<String, RewordError>),
    /// The budget elapsed with the job still running. Its answer is
    /// unreachable and will be dropped.
    Deadline,
    /// No permit was free. Not queued -- see [`REWORD_MAX_INFLIGHT`].
    Busy,
}

/// Run one rewrite on the blocking pool, bounded by `budget`.
///
/// Returns the outcome and how long the caller waited. The `Duration` is
/// wall-clock from the moment the permit was requested, which is what the
/// settings window reports and compares against the configured deadline.
pub async fn attempt(
    rewriter: Arc<dyn Rewriter>,
    state: Arc<RewordState>,
    text: String,
    budget: Duration,
) -> (Attempt, Duration) {
    let started = Instant::now();
    let Some(permit) = state.try_permit() else {
        return (Attempt::Busy, started.elapsed());
    };
    let job = tokio::task::spawn_blocking(move || {
        // `let _permit`, never `let _`: binding to `_` drops the permit at
        // once and is precisely the bug §2 exists to prevent. Named, it
        // lives to the end of this closure, so the permit is released when
        // the *job* finishes -- not when the deadline below fires.
        let _permit = permit;
        rewriter.reword(&text)
    });
    match tokio::time::timeout(budget, job).await {
        Ok(Ok(result)) => (Attempt::Answered(result), started.elapsed()),
        // The blocking task itself failed, which means it panicked. Not
        // reachable through the shipped client, but a panic here must not
        // take the announcement with it.
        Ok(Err(join)) => (
            Attempt::Answered(Err(RewordError::Malformed(format!(
                "the rewrite task failed: {join}"
            )))),
            started.elapsed(),
        ),
        // The `.await` on the handle is abandoned here. The job keeps
        // running (bounded by REWORD_HTTP_CEILING) and keeps its permit
        // (bounded by REWORD_MAX_INFLIGHT), and its eventual answer has
        // nowhere to go.
        Err(_elapsed) => (Attempt::Deadline, started.elapsed()),
    }
}

/// Why a request was not even attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    AuthLatched,
    TransportOpen,
    RateLimited,
}

/// The circuit breakers, the permit pool and the log-once latches.
///
/// One instance for the process, reached through [`state`]; tests build
/// their own with [`RewordState::new`] so they never touch it.
///
/// Every method that consults or moves time takes `now` as a parameter, the
/// same discipline `notify::policy` uses and for the same reason: a test
/// that had to sleep for a 60-second breaker window would not be written.
pub struct RewordState {
    permits: Arc<tokio::sync::Semaphore>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Endpoints already announced this run, keyed `base_url|model`.
    announced: HashSet<String>,
    /// The exact `RewordConfig` a 401/403 was seen under.
    ///
    /// DEPARTURE from §8, which says "until the config *generation*
    /// changes". Keying on the config value itself is strictly more precise
    /// and needs no plumbing: neither `notify::monitor` nor `dbus.rs` holds
    /// a `ConfigStore`, and a generation bumped by an unrelated setting
    /// (a voice change, a tray mute) would clear a latch nothing had fixed.
    /// §6's parenthetical -- that editing an environment-supplied key does
    /// not move the generation, so a successful Test is the only way back
    /// -- holds identically here, because an environment edit does not move
    /// the config value either.
    auth_latched_for: Option<RewordConfig>,
    transport_failures: u32,
    transport_open_until: Option<Instant>,
    rate_limited_until: Option<Instant>,
    deadlines: u64,
    /// Log-once latches, one per §8 row that says "once per run".
    not_configured_logged: bool,
    plain_http_logged: bool,
    too_long_logged: bool,
    outage_logged: bool,
    auth_logged: bool,
    model_logged: bool,
}

impl RewordState {
    pub fn new() -> Arc<RewordState> {
        Arc::new(RewordState {
            permits: Arc::new(tokio::sync::Semaphore::new(REWORD_MAX_INFLIGHT)),
            inner: Mutex::new(Inner::default()),
        })
    }

    /// How many rewrites could start right now. Not a control -- the tests
    /// that hold this module's concurrency bound in place are its readers,
    /// and they are the reason it is here.
    #[allow(dead_code)]
    pub fn available_permits(&self) -> usize {
        self.permits.available_permits()
    }

    /// Take a permit, or `None` if none is free. Synchronous, so the
    /// settings window can take one from a plain thread with no runtime in
    /// scope -- which is what keeps Test inside the same bound of 2.
    pub fn try_permit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }

    /// May a request be made at all?
    pub fn allow(&self, cfg: &RewordConfig, now: Instant) -> Result<(), Blocked> {
        let mut i = lock(&self.inner);
        if i.auth_latched_for.as_ref() == Some(cfg) {
            return Err(Blocked::AuthLatched);
        }
        if let Some(until) = i.transport_open_until {
            if now < until {
                return Err(Blocked::TransportOpen);
            }
            // "then let one through": the breaker closes, and the failure
            // count starts again from zero rather than from three.
            i.transport_open_until = None;
            i.transport_failures = 0;
        }
        if let Some(until) = i.rate_limited_until {
            if now < until {
                return Err(Blocked::RateLimited);
            }
            i.rate_limited_until = None;
        }
        Ok(())
    }

    /// Fold one outcome into the breakers, and log whatever §8 says this
    /// row owes -- once per run, or the first of a standing outage, never
    /// once per utterance.
    pub fn record(&self, cfg: &RewordConfig, outcome: &Attempt, now: Instant) {
        let mut i = lock(&self.inner);
        match outcome {
            Attempt::Busy => {}
            Attempt::Deadline => {
                i.deadlines += 1;
                if i.deadlines == 1 || i.deadlines.is_multiple_of(DEADLINE_LOG_EVERY) {
                    eprintln!(
                        "info: a rewrite did not answer within {} ms; spoke the text as \
                         written ({} so far this run)",
                        cfg.timeout_ms, i.deadlines
                    );
                }
            }
            Attempt::Answered(Ok(_)) => {
                i.transport_failures = 0;
                i.outage_logged = false;
            }
            Attempt::Answered(Err(e)) => match e {
                RewordError::Auth {
                    status,
                    host,
                    message,
                } => {
                    if !i.auth_logged {
                        eprintln!(
                            "warning: reword: {host} rejected the API key (HTTP {status}{}); \
                             speaking text as written until the configuration changes",
                            message
                                .as_deref()
                                .map(|m| format!(": {m}"))
                                .unwrap_or_default()
                        );
                        i.auth_logged = true;
                    }
                    i.auth_latched_for = Some(cfg.clone());
                }
                RewordError::NoSuchModel {
                    status,
                    model,
                    message,
                } => {
                    if !i.model_logged {
                        eprintln!(
                            "warning: reword: the provider does not have model {model:?} \
                             (HTTP {status}{}); speaking text as written",
                            message
                                .as_deref()
                                .map(|m| format!(": {m}"))
                                .unwrap_or_default()
                        );
                        i.model_logged = true;
                    }
                }
                RewordError::RateLimited { retry_after, .. } => {
                    let wait = retry_after
                        .unwrap_or(RATE_LIMIT_BACKOFF)
                        .min(RATE_LIMIT_MAX_BACKOFF);
                    i.rate_limited_until = now.checked_add(wait);
                }
                RewordError::Unreachable(detail) => {
                    if !i.outage_logged {
                        eprintln!("warning: reword: could not reach the provider: {detail}");
                        i.outage_logged = true;
                    }
                    i.fail_transport(now);
                }
                RewordError::Ceiling => {
                    if !i.outage_logged {
                        eprintln!(
                            "warning: reword: the provider did not answer within {:.0} s",
                            REWORD_HTTP_CEILING.as_secs_f64()
                        );
                        i.outage_logged = true;
                    }
                    i.fail_transport(now);
                }
                RewordError::NotConfigured(reason) => {
                    if !i.not_configured_logged {
                        eprintln!("warning: reword: {reason}; speaking text as written");
                        i.not_configured_logged = true;
                    }
                }
                RewordError::Unavailable => {}
                RewordError::Malformed(detail) => {
                    debug(format_args!("reword: unusable response: {detail}"));
                }
            },
        }
    }

    /// Forget a latched auth failure. Called only by a *successful* test in
    /// the settings window, which is the one event that proves the key
    /// works -- and the only way back when the key came from the
    /// environment.
    // `#[allow(dead_code)]`: the settings window's Test row is its only
    // caller and lands in a later task of this milestone.
    #[allow(dead_code)]
    pub fn clear_auth_latch(&self) {
        lock(&self.inner).auth_latched_for = None;
    }

    /// Announce where text is going, once per run per resolved endpoint,
    /// and warn once about cleartext to a non-loopback host. Returns
    /// whether this was the first time -- which the settings window uses to
    /// say that a first request includes connection setup.
    pub fn note_endpoint(&self, cfg: &RewordConfig) -> bool {
        let key = format!("{}|{}", cfg.base_url, cfg.model);
        let mut i = lock(&self.inner);
        if !i.announced.insert(key) {
            return false;
        }
        eprintln!(
            "info: reword: sending text to {} (model {})",
            cfg.base_url, cfg.model
        );
        if !i.plain_http_logged {
            if let Ok(endpoint) = sayd_core::reword::parse_base_url(&cfg.base_url) {
                if endpoint.scheme == "http" && !sayd_core::reword::is_loopback(&endpoint.host) {
                    // A security statement rather than a trust judgement:
                    // cleartext on the wire is a fact about the transport,
                    // not an opinion about the operator.
                    eprintln!(
                        "warning: reword: base_url is plain HTTP to a non-loopback host; \
                         text will cross the network unencrypted"
                    );
                    i.plain_http_logged = true;
                }
            }
        }
        true
    }

    // `#[allow(dead_code)]`: read by the settings window's Test row, which
    // uses it to say that a first request includes connection setup.
    #[allow(dead_code)]
    pub fn endpoint_seen(&self, cfg: &RewordConfig) -> bool {
        lock(&self.inner)
            .announced
            .contains(&format!("{}|{}", cfg.base_url, cfg.model))
    }

    /// §4's logging rule: over-long text is worth one line per run, short
    /// text is worth none at all.
    pub fn note_ineligible(&self, why: Ineligible) {
        if why != Ineligible::TooLong {
            return;
        }
        let mut i = lock(&self.inner);
        if !i.too_long_logged {
            eprintln!(
                "info: reword: some text is longer than reword.max_chars and is spoken \
                 as written (said once per run)"
            );
            i.too_long_logged = true;
        }
    }
}

impl Inner {
    /// One transport-class failure: [`RewordError::Unreachable`] and
    /// [`RewordError::Ceiling`] are the same row of §8's table and must
    /// count toward the same breaker, so they share the one body rather
    /// than two that can drift apart.
    fn fail_transport(&mut self, now: Instant) {
        self.transport_failures += 1;
        if self.transport_failures >= TRANSPORT_FAILURES_TO_OPEN {
            self.transport_open_until = now.checked_add(TRANSPORT_BREAKER_COOLDOWN);
            self.transport_failures = 0;
        }
    }
}

/// Will this text be reworded at all? Decided synchronously and cheaply,
/// *before* anything is spawned, so an ineligible submission costs one pass
/// over a short string and a mutex.
///
/// Both callers -- `notify::monitor::speak` and `dbus.rs` -- gate on this
/// and only detach or await when it says yes.
// `#[allow(dead_code)]`: those two callers are a later task in this
// milestone. Everything they will call is here and tested.
#[allow(dead_code)]
pub fn will_reword(text: &str, cfg: &RewordConfig, state: &RewordState) -> bool {
    if let Err(why) = eligible(text, cfg.max_chars) {
        state.note_ineligible(why);
        return false;
    }
    state.allow(cfg, Instant::now()).is_ok()
}

/// The text to speak: the rewrite if one arrived in time and passed the
/// guard, the original otherwise.
///
/// **Holds no `EngineHandle` and returns a `String`.** That is the whole of
/// the drop rule: the caller submits, so a late answer has nowhere to go.
/// Do not add a submit callback to this signature.
///
/// Assumes [`will_reword`] has already said yes.
// `#[allow(dead_code)]`: as `will_reword`.
#[allow(dead_code)]
pub async fn reword_or_original(
    text: String,
    cfg: &RewordConfig,
    rewriter: Arc<dyn Rewriter>,
    state: Arc<RewordState>,
) -> String {
    state.note_endpoint(cfg);
    let budget = Duration::from_millis(cfg.timeout_ms);
    let (outcome, _elapsed) = attempt(rewriter, state.clone(), text.clone(), budget).await;
    state.record(cfg, &outcome, Instant::now());
    let Attempt::Answered(Ok(candidate)) = outcome else {
        return text;
    };
    match check(&text, &candidate) {
        Ok(rewritten) => rewritten,
        Err(reason) => {
            debug(format_args!(
                "reword: rejected a candidate ({}): {:?}",
                reason.phrase(),
                sayd_core::reword::truncate_for_debug(&candidate, DEBUG_SNIPPET_CHARS)
            ));
            text
        }
    }
}

/// The process-wide breaker state and permit pool.
///
/// A `OnceLock` rather than a value threaded through `notify::monitor::run`
/// and `dbus::SaydIface`, for the reason `SETTINGS_REQUESTS` is one: three
/// unrelated call sites need the same bound, and two of them are
/// constructed in tests that must not have to build one.
pub fn state() -> Arc<RewordState> {
    static STATE: OnceLock<Arc<RewordState>> = OnceLock::new();
    STATE.get_or_init(RewordState::new).clone()
}

/// The rewriter for `cfg`, or `None` when this build cannot make one or the
/// configuration cannot be used.
///
/// Cached and rebuilt only when the config changes. The underlying `ureq`
/// agent is cached separately and outlives config changes entirely --
/// `base_url`, `model` and the key are per-request inputs, not client
/// state.
// `#[allow(dead_code)]`: as `will_reword`.
#[allow(dead_code)]
pub fn context(cfg: &RewordConfig) -> Option<(Arc<dyn Rewriter>, Arc<RewordState>)> {
    /// The client and the exact config it was built for. Named because the
    /// pair is what makes "rebuilt only when the config changes" checkable
    /// in one comparison.
    type Cache = Mutex<Option<(RewordConfig, Arc<dyn Rewriter>)>>;

    static CACHE: OnceLock<Cache> = OnceLock::new();
    let state = state();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = lock(cell);
    if let Some((cached_cfg, rewriter)) = guard.as_ref() {
        if cached_cfg == cfg {
            return Some((rewriter.clone(), state));
        }
    }
    match build_rewriter(cfg) {
        Ok(rewriter) => {
            *guard = Some((cfg.clone(), rewriter.clone()));
            Some((rewriter, state))
        }
        Err(e) => {
            drop(guard);
            state.record(cfg, &Attempt::Answered(Err(e)), Instant::now());
            None
        }
    }
}

/// Build a client for `cfg`. The one function whose body differs between
/// the two builds.
// `#[allow(dead_code)]`: `HttpRewriter` lands in the next task, which is
// where this attribute is deleted.
#[allow(dead_code)]
#[cfg(feature = "reword")]
pub fn build_rewriter(cfg: &RewordConfig) -> Result<Arc<dyn Rewriter>, RewordError> {
    http::HttpRewriter::new(cfg).map(|r| Arc::new(r) as Arc<dyn Rewriter>)
}

// `#[allow(dead_code)]`: as above.
#[allow(dead_code)]
#[cfg(not(feature = "reword"))]
pub fn build_rewriter(_cfg: &RewordConfig) -> Result<Arc<dyn Rewriter>, RewordError> {
    Err(RewordError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A test double: a queue of canned outcomes and, optionally, a sleep
    /// so a test can drive the deadline. No runtime, no futures, no
    /// associated types -- which is the whole reason the seam is a
    /// one-method synchronous trait we define ourselves.
    struct Stub {
        outcomes: Mutex<std::collections::VecDeque<Result<String, RewordError>>>,
        sleep: Duration,
        calls: AtomicUsize,
    }

    impl Stub {
        fn new(outcomes: Vec<Result<String, RewordError>>) -> Arc<Stub> {
            Arc::new(Stub {
                outcomes: Mutex::new(outcomes.into()),
                sleep: Duration::ZERO,
                calls: AtomicUsize::new(0),
            })
        }

        fn slow(sleep: Duration, outcomes: Vec<Result<String, RewordError>>) -> Arc<Stub> {
            Arc::new(Stub {
                outcomes: Mutex::new(outcomes.into()),
                sleep,
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl Rewriter for Stub {
        fn reword(&self, _text: &str) -> Result<String, RewordError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.sleep.is_zero() {
                std::thread::sleep(self.sleep);
            }
            lock(&self.outcomes)
                .pop_front()
                .unwrap_or(Err(RewordError::Malformed("stub exhausted".into())))
        }
    }

    fn cfg() -> RewordConfig {
        RewordConfig {
            enabled: true,
            timeout_ms: 100,
            ..RewordConfig::default()
        }
    }

    /// The deadline race. A rewrite that has not answered in `timeout_ms`
    /// does not get spoken; the original does, and it gets spoken exactly
    /// once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rewrite_that_misses_the_deadline_is_dropped_and_the_original_is_spoken() {
        let stub = Stub::slow(
            Duration::from_millis(400),
            vec![Ok("a much better sentence than the one that came in".into())],
        );
        let state = RewordState::new();
        let original = "Alice: where do you want to go for dinner".to_string();

        let spoken = reword_or_original(
            original.clone(),
            &cfg(),
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
        )
        .await;

        assert_eq!(
            spoken, original,
            "past the deadline the original is what gets spoken"
        );
        // And the late answer has nowhere to go: `reword_or_original` has
        // no engine handle at all, so there is no path on which a rewrite
        // is spoken after its original. This is the test that would have
        // caught a `submit` call inside the rewrite job.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            1,
            "exactly one request was made, and its answer was discarded"
        );
    }

    /// §2's load-bearing rule: the permit belongs to the blocking job, not
    /// to the deadline. Released at the deadline, a slow provider would
    /// accumulate blocking threads at the arrival rate while appearing
    /// bounded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_permit_is_held_past_the_deadline_until_the_job_finishes() {
        let stub = Stub::slow(Duration::from_millis(500), vec![Ok("better".into())]);
        let state = RewordState::new();
        assert_eq!(state.available_permits(), REWORD_MAX_INFLIGHT);

        let (outcome, _) = attempt(
            stub as Arc<dyn Rewriter>,
            state.clone(),
            "Alice: where do you want to go for dinner".into(),
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(outcome, Attempt::Deadline));
        assert_eq!(
            state.available_permits(),
            REWORD_MAX_INFLIGHT - 1,
            "the deadline has fired and the permit is still held: the job is \
             still on a blocking thread and must still be counted"
        );

        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            state.available_permits(),
            REWORD_MAX_INFLIGHT,
            "and it comes back when the job actually finishes"
        );
    }

    /// Concurrency is bounded at 2. The third is not queued behind the
    /// other two -- it is submitted raw immediately, because when the
    /// provider is slow the right answer is the raw text now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_third_concurrent_rewrite_is_refused_rather_than_queued() {
        let stub = Stub::slow(
            Duration::from_millis(400),
            vec![Ok("a".into()), Ok("b".into()), Ok("c".into())],
        );
        let state = RewordState::new();
        let text = "Alice: where do you want to go for dinner".to_string();

        let a = tokio::spawn(attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            text.clone(),
            Duration::from_millis(900),
        ));
        let b = tokio::spawn(attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            text.clone(),
            Duration::from_millis(900),
        ));
        // Long enough for both permits to be taken, far short of the 400 ms
        // the stub sleeps for.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = std::time::Instant::now();
        let (third, _) = attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            text.clone(),
            Duration::from_millis(900),
        )
        .await;
        assert!(
            matches!(third, Attempt::Busy),
            "no permit was free, so the third is refused: {third:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "and refused *immediately*, not queued behind the other two"
        );

        let _ = a.await;
        let _ = b.await;
        assert_eq!(stub.calls.load(Ordering::SeqCst), 2);
    }

    /// §8's auth latch. A bad key does not fix itself, and every retry
    /// costs a full budget of delay before the same fallback.
    #[test]
    fn a_rejected_key_stops_further_attempts_until_the_config_changes() {
        let state = RewordState::new();
        let cfg = cfg();
        let now = Instant::now();
        assert_eq!(state.allow(&cfg, now), Ok(()));

        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::Auth {
                status: 401,
                host: "api.ppq.ai".into(),
                message: None,
            })),
            now,
        );
        assert_eq!(state.allow(&cfg, now), Err(Blocked::AuthLatched));
        assert_eq!(
            state.allow(&cfg, now + Duration::from_secs(3600)),
            Err(Blocked::AuthLatched),
            "time does not fix a bad key"
        );

        // Any change to the reword config -- a new key, a new endpoint, a
        // new model -- is a fresh question worth asking.
        let mut changed = cfg.clone();
        changed.api_key = "sk-new".into();
        assert_eq!(state.allow(&changed, now), Ok(()));

        // ...and a successful test in the settings window clears it, which
        // is the only way to recover from a key supplied through the
        // environment: editing that does not change the config at all.
        assert_eq!(state.allow(&cfg, now), Err(Blocked::AuthLatched));
        state.clear_auth_latch();
        assert_eq!(state.allow(&cfg, now), Ok(()));
    }

    /// §8's transport breaker: after 3 consecutive failures, stop
    /// attempting for 60 s, then let one through. A success resets the
    /// count.
    #[test]
    fn three_transport_failures_open_the_breaker_for_a_minute() {
        let state = RewordState::new();
        let cfg = cfg();
        let t0 = Instant::now();

        for i in 0..2 {
            state.record(
                &cfg,
                &Attempt::Answered(Err(RewordError::Unreachable("connection refused".into()))),
                t0,
            );
            assert_eq!(
                state.allow(&cfg, t0),
                Ok(()),
                "still closed after {}",
                i + 1
            );
        }
        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::Unreachable("connection refused".into()))),
            t0,
        );
        assert_eq!(state.allow(&cfg, t0), Err(Blocked::TransportOpen));
        assert_eq!(
            state.allow(&cfg, t0 + Duration::from_secs(59)),
            Err(Blocked::TransportOpen)
        );
        assert_eq!(
            state.allow(&cfg, t0 + Duration::from_secs(61)),
            Ok(()),
            "after 60 s one is let through"
        );

        // The client's own 10 s ceiling counts toward the same breaker.
        for _ in 0..3 {
            state.record(&cfg, &Attempt::Answered(Err(RewordError::Ceiling)), t0);
        }
        assert_eq!(state.allow(&cfg, t0), Err(Blocked::TransportOpen));

        // A success resets the count rather than merely not incrementing it.
        let state = RewordState::new();
        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::Unreachable("x".into()))),
            t0,
        );
        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::Unreachable("x".into()))),
            t0,
        );
        state.record(&cfg, &Attempt::Answered(Ok("fine".into())), t0);
        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::Unreachable("x".into()))),
            t0,
        );
        assert_eq!(state.allow(&cfg, t0), Ok(()));
    }

    /// §8's rate-limit row: honour `Retry-After` when present, otherwise
    /// back off 60 s. Never retry the same utterance -- it is already
    /// spoken.
    #[test]
    fn a_rate_limit_backs_off_for_retry_after_or_a_minute() {
        let state = RewordState::new();
        let cfg = cfg();
        let t0 = Instant::now();

        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::RateLimited {
                retry_after: Some(Duration::from_secs(5)),
                message: None,
            })),
            t0,
        );
        assert_eq!(state.allow(&cfg, t0), Err(Blocked::RateLimited));
        assert_eq!(state.allow(&cfg, t0 + Duration::from_secs(6)), Ok(()));

        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::RateLimited {
                retry_after: None,
                message: None,
            })),
            t0,
        );
        assert_eq!(
            state.allow(&cfg, t0 + Duration::from_secs(30)),
            Err(Blocked::RateLimited)
        );
        assert_eq!(state.allow(&cfg, t0 + Duration::from_secs(61)), Ok(()));
    }

    /// The gate the callers use before they spawn anything. Short text,
    /// long text and an open breaker all mean "speak it as written", and
    /// deciding that costs one pass over a short string.
    #[test]
    fn will_reword_refuses_the_ineligible_and_the_broken() {
        let state = RewordState::new();
        let cfg = cfg();
        assert!(will_reword(
            "Alice: where do you want to go for dinner",
            &cfg,
            &state
        ));
        assert!(!will_reword("Ping", &cfg, &state));
        assert!(!will_reword(&"x".repeat(401), &cfg, &state));

        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::Auth {
                status: 401,
                host: "h".into(),
                message: None,
            })),
            Instant::now(),
        );
        assert!(!will_reword(
            "Alice: where do you want to go for dinner",
            &cfg,
            &state
        ));
    }

    /// The guard is applied to whatever comes back, and a rejection means
    /// the original.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_guard_rejection_speaks_the_original() {
        let original = "Alice: where do you want to go for dinner".to_string();
        let state = RewordState::new();

        let good = Stub::new(vec![Ok(
            "Alice is asking where you want to go for dinner".into()
        )]);
        assert_eq!(
            reword_or_original(
                original.clone(),
                &cfg(),
                good as Arc<dyn Rewriter>,
                state.clone()
            )
            .await,
            "Alice is asking where you want to go for dinner"
        );

        let chatty = Stub::new(vec![Ok("Sure!\nHere you go:\nAlice is asking.".into())]);
        assert_eq!(
            reword_or_original(
                original.clone(),
                &cfg(),
                chatty as Arc<dyn Rewriter>,
                RewordState::new()
            )
            .await,
            original,
            "a model that explained itself gets the original spoken instead"
        );

        let dead = Stub::new(vec![Err(RewordError::Unreachable("refused".into()))]);
        assert_eq!(
            reword_or_original(
                original.clone(),
                &cfg(),
                dead as Arc<dyn Rewriter>,
                RewordState::new()
            )
            .await,
            original
        );
    }

    /// An endpoint is announced once per run, not once per utterance --
    /// the same discipline as the notification discovery log, for the same
    /// reason. It is greppable in the journal, which is the point: where
    /// text goes must be discoverable without reading the config.
    #[test]
    fn an_endpoint_is_announced_once_per_run_and_a_changed_one_again() {
        let state = RewordState::new();
        let cfg = cfg();
        assert!(state.note_endpoint(&cfg), "the first send says where");
        assert!(!state.note_endpoint(&cfg), "and the thousandth does not");
        assert!(state.endpoint_seen(&cfg));

        let mut other = cfg.clone();
        other.model = "gpt-4o-mini".into();
        assert!(
            state.note_endpoint(&other),
            "a different model is a different line"
        );
    }

    /// A stub needs no runtime, no futures and no associated types. If this
    /// stops compiling, the seam has grown something.
    #[test]
    fn a_stub_is_a_struct_with_a_vec() {
        let stub: Arc<dyn Rewriter> = Stub::new(vec![Ok("hello".into())]);
        assert_eq!(stub.reword("anything").as_deref(), Ok("hello"));
    }

    /// A stub that watches itself: how many calls are inside `reword` at
    /// once, and the largest that number ever reached.
    struct Counted {
        sleep: Duration,
        inflight: AtomicUsize,
        peak: AtomicUsize,
        calls: AtomicUsize,
    }

    impl Counted {
        fn new(sleep: Duration) -> Arc<Counted> {
            Arc::new(Counted {
                sleep,
                inflight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl Rewriter for Counted {
        fn reword(&self, _text: &str) -> Result<String, RewordError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let n = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(self.sleep);
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            Ok("Alice is asking where you want to go for dinner".into())
        }
    }

    /// The bound observed from *inside* the rewriter rather than from the
    /// permit count: across three waves of four arrivals, two requests are
    /// live at once and never three, and permits come back for the next
    /// wave rather than being spent once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_rewrites_run_at_once_and_never_three() {
        let stub = Counted::new(Duration::from_millis(150));
        let state = RewordState::new();
        let text = "Alice: where do you want to go for dinner".to_string();

        for wave in 0..3 {
            let mut spawned = Vec::new();
            for _ in 0..4 {
                spawned.push(tokio::spawn(attempt(
                    stub.clone() as Arc<dyn Rewriter>,
                    state.clone(),
                    text.clone(),
                    Duration::from_secs(5),
                )));
            }
            let mut answered = 0;
            let mut busy = 0;
            for handle in spawned {
                match handle.await.expect("the attempt task itself must not fail") {
                    (Attempt::Answered(Ok(_)), _) => answered += 1,
                    (Attempt::Busy, _) => busy += 1,
                    (other, _) => panic!("unexpected outcome in wave {wave}: {other:?}"),
                }
            }
            assert_eq!(
                (answered, busy),
                (REWORD_MAX_INFLIGHT, 4 - REWORD_MAX_INFLIGHT),
                "wave {wave}: two got in, the rest were refused rather than queued"
            );
            assert_eq!(
                state.available_permits(),
                REWORD_MAX_INFLIGHT,
                "wave {wave}: both permits came back"
            );
        }

        assert_eq!(
            stub.peak.load(Ordering::SeqCst),
            REWORD_MAX_INFLIGHT,
            "two requests were live at once, and never a third"
        );
        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            3 * REWORD_MAX_INFLIGHT,
            "and every wave got its two, so permits are reusable"
        );
    }

    /// The one this module exists for, in the shape it was measured in:
    /// arrivals that all miss their deadline against a provider that is
    /// stuck. Counted in threads, because threads are the resource that ran
    /// out -- 30 to 548 in three and a half minutes, then tokio's 512-thread
    /// cap, then `Say` over D-Bus never returning.
    ///
    /// With the permit released at the deadline instead of held, every one
    /// of these 60 arrivals would get a permit and a fresh blocking thread
    /// while the previous ones were still parked, and this test would count
    /// dozens rather than two.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stuck_rewriter_does_not_leak_blocking_threads() {
        /// This process's live threads, which is what ran out.
        fn threads() -> usize {
            std::fs::read_dir("/proc/self/task")
                .map(|d| d.count())
                .unwrap_or(0)
        }

        let stub = Stub::slow(Duration::from_millis(500), Vec::new());
        let state = RewordState::new();
        // One attempt first, so the blocking pool has grown its first thread
        // and the baseline is not credited with it.
        let _ = attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            "Alice: where do you want to go for dinner".into(),
            Duration::from_millis(10),
        )
        .await;
        let baseline = threads();
        assert!(baseline > 0, "/proc/self/task must be readable here");

        let mut peak = baseline;
        for _ in 0..60 {
            let _ = attempt(
                stub.clone() as Arc<dyn Rewriter>,
                state.clone(),
                "Alice: where do you want to go for dinner".into(),
                Duration::from_millis(10),
            )
            .await;
            peak = peak.max(threads());
        }

        assert!(
            peak <= baseline + REWORD_MAX_INFLIGHT,
            "60 arrivals against a stuck provider grew the process from {baseline} \
             threads to {peak}; the bound is {REWORD_MAX_INFLIGHT} blocking threads \
             no matter how many arrive"
        );
        assert!(
            stub.calls.load(Ordering::SeqCst) <= REWORD_MAX_INFLIGHT,
            "and the arrivals that found no permit made no request at all"
        );
    }

    /// A rewriter that answers late answers *fully* -- the job is not
    /// cancelled, it is abandoned -- and what it produced still never
    /// reaches the caller. The value exists; there is simply nothing holding
    /// the other end of it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_late_answer_is_produced_in_full_and_still_goes_nowhere() {
        struct Late {
            produced: Arc<Mutex<Vec<String>>>,
        }
        impl Rewriter for Late {
            fn reword(&self, _text: &str) -> Result<String, RewordError> {
                std::thread::sleep(Duration::from_millis(400));
                let answer = "Alice is asking where you want to go for dinner".to_string();
                lock(&self.produced).push(answer.clone());
                Ok(answer)
            }
        }

        let produced = Arc::new(Mutex::new(Vec::new()));
        let rewriter = Arc::new(Late {
            produced: produced.clone(),
        });
        let original = "Alice: where do you want to go for dinner".to_string();

        let spoken = reword_or_original(
            original.clone(),
            &cfg(),
            rewriter as Arc<dyn Rewriter>,
            RewordState::new(),
        )
        .await;
        assert_eq!(spoken, original);

        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            lock(&produced).len(),
            1,
            "the job ran to completion: it was abandoned, not cancelled"
        );
        assert_ne!(
            spoken,
            lock(&produced)[0],
            "and the thing it produced is not the thing that got spoken"
        );
    }

    /// The trait is the boundary, so an implementation that panics -- which
    /// is what a client that unwraps a provider's response does -- costs a
    /// rewrite and nothing else. Note especially that the permit comes back:
    /// a panic that leaked one would shrink the pool by one per occurrence
    /// until no rewrite could ever start again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_rewriter_speaks_the_original_and_returns_its_permit() {
        struct Panicky;
        impl Rewriter for Panicky {
            fn reword(&self, _text: &str) -> Result<String, RewordError> {
                panic!("a client that unwrapped something a provider sent");
            }
        }

        let state = RewordState::new();
        let original = "Alice: where do you want to go for dinner".to_string();
        let spoken = reword_or_original(
            original.clone(),
            &cfg(),
            Arc::new(Panicky) as Arc<dyn Rewriter>,
            state.clone(),
        )
        .await;
        assert_eq!(spoken, original);
        assert_eq!(
            state.available_permits(),
            REWORD_MAX_INFLIGHT,
            "the permit was released by the unwind, not leaked"
        );
        // A panic is classified as `Malformed`, which is the row that does
        // not touch the transport breaker: the provider answered, the client
        // could not cope with the answer, and the next notification should
        // still get its chance.
        assert_eq!(state.allow(&cfg(), Instant::now()), Ok(()));
    }

    /// The breaker windows are decided by the `Instant` handed in and by
    /// nothing else, which is what makes a 60-second window testable in
    /// microseconds. Same discipline as `notify::policy::Limiter`.
    #[test]
    fn the_breaker_windows_move_on_the_injected_clock_and_not_on_wall_time() {
        let state = RewordState::new();
        let cfg = cfg();
        let t0 = Instant::now();
        let text = "Alice: where do you want to go for dinner";

        for _ in 0..TRANSPORT_FAILURES_TO_OPEN {
            state.record(
                &cfg,
                &Attempt::Answered(Err(RewordError::Unreachable("connection refused".into()))),
                t0,
            );
        }
        // `will_reword` reads the real clock, and by it barely any time has
        // passed: the window is open.
        assert!(!will_reword(text, &cfg, &state));
        // The injected clock is a minute later, so the same state lets one
        // through -- no wall-clock second has elapsed to make that true.
        assert_eq!(
            state.allow(
                &cfg,
                t0 + TRANSPORT_BREAKER_COOLDOWN + Duration::from_secs(1)
            ),
            Ok(())
        );
        assert!(
            will_reword(text, &cfg, &state),
            "and the breaker it closed stays closed for the real clock too"
        );

        // The other direction: a window dated into the future by the
        // injected clock blocks a caller reading the real one.
        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::RateLimited {
                retry_after: Some(Duration::from_secs(1800)),
                message: None,
            })),
            t0,
        );
        assert!(!will_reword(text, &cfg, &state));
        assert_eq!(
            state.allow(&cfg, t0 + Duration::from_secs(1799)),
            Err(Blocked::RateLimited)
        );
        assert_eq!(state.allow(&cfg, t0 + Duration::from_secs(1801)), Ok(()));
    }

    /// `retry_after` is the one breaker input a provider controls directly.
    /// Unclamped it reaches `Instant::checked_add`, where a value big enough
    /// to overflow comes back as `None` -- and a `None` deadline is *no*
    /// backoff, so the single most hostile header would be the one that
    /// switched the backoff off.
    #[test]
    fn an_absurd_retry_after_backs_off_rather_than_not_at_all() {
        let state = RewordState::new();
        let cfg = cfg();
        let t0 = Instant::now();

        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::RateLimited {
                retry_after: Some(Duration::MAX),
                message: None,
            })),
            t0,
        );
        assert_eq!(
            state.allow(&cfg, t0),
            Err(Blocked::RateLimited),
            "a header out of every sane range must not read as no backoff at all"
        );
        assert_eq!(
            state.allow(&cfg, t0 + RATE_LIMIT_MAX_BACKOFF + Duration::from_secs(1)),
            Ok(()),
            "and it must not switch the feature off for the life of the process"
        );
    }

    /// One permit pool and one set of breakers for the process, so the
    /// notification path, `--reword` and the settings window's Test button
    /// share the bound instead of having one each.
    #[test]
    fn the_process_wide_state_is_a_single_instance() {
        assert!(Arc::ptr_eq(&state(), &state()));
    }

    /// Without the `reword` feature there is no client to build, and the
    /// orchestration says so in a variant rather than by being absent.
    #[cfg(not(feature = "reword"))]
    #[test]
    fn a_default_build_has_no_client_to_build() {
        assert!(matches!(
            build_rewriter(&cfg()),
            Err(RewordError::Unavailable)
        ));
    }
}
