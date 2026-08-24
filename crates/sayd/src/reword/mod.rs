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
//! thread is the client's own [`http_ceiling`]. This is
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
//! [`http_ceiling`] -- the configured deadline plus [`REWORD_HTTP_GRACE`],
//! so the worst case grows with the deadline the user asked for and with
//! nothing else.
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
//!
//! # The answer is dropped; the outcome is not
//!
//! Those two rules together used to make the transport breaker inert
//! against the one provider failure it most exists for. The client's
//! ceiling is [`REWORD_HTTP_GRACE`] past `timeout_ms` and the caller gives
//! up at `timeout_ms`, so a provider that
//! accepts a connection and then never answers *always* produces
//! [`Attempt::Deadline`]: the [`RewordError::Ceiling`] the client eventually
//! returns died with the abandoned `JoinHandle` and never reached
//! [`RewordState::record`]. Measured: ten consecutive ceiling-class
//! failures, and `allow` still said `Ok(())`. Every eligible notification
//! went on paying the full `timeout_ms`, both permits stayed occupied so
//! most arrivals degraded to [`Attempt::Busy`], and §8's "after 3
//! consecutive transport failures, stop attempting for 60 s" could never
//! fire.
//!
//! The fix is the distinction the spec is actually drawing: **the blocking
//! job folds its own outcome into `record` before it drops the permit**.
//! §2's rule is about the *answer* -- the `JoinHandle`'s value is still
//! discarded, `record` takes no `EngineHandle` and returns nothing, so
//! there is still no path on which a late rewrite is spoken. §8's rule is
//! about the *outcome*, and an outcome is not a thing that can be spoken.
//! [`attempt`] is therefore the only place that records: the job records
//! what it got, the caller records the deadline it saw, and neither records
//! the other's.
//!
//! The alternative -- a second breaker counting consecutive
//! [`Attempt::Deadline`]s -- was rejected: it opens on a provider that is
//! slow but working perfectly, which is a different failure with a
//! different right answer (`timeout_ms` is too low), and it would double
//! count the stuck provider that this one already catches.
//!
//! # What leaves the machine is cleaned first
//!
//! §2's pipeline is `compose -> clean -> [reword] -> submit -> clean`, and
//! the *first* clean is [`RewordPlan::admit_with`]'s, on the way in. It was
//! missing until the final review of this milestone measured what a fake
//! provider actually received from one notification:
//!
//! ````text
//! Signal: Alice sent a link. reset here https://example.com/reset?token=SECRET123
//! ```\nexport AWS_SECRET=hunter2\n``` **bold** _em_ ESC[31mRED ESC[0m
//! ````
//!
//! -- a reset token, a fenced shell secret, markdown and an ANSI escape,
//! verbatim, to whatever `base_url` names. The README told the user the
//! opposite in as many words. Cleaning here is what makes that sentence
//! true: a code fence is dropped whole, a URL is already the word `link`,
//! and the string the guard measures is the string that gets spoken.
//!
//! Three consequences, all deliberate. `sayd_core::reword::check` rejects a
//! candidate containing a code fence, and the model can no longer be shown
//! one, so that rejection stops firing on text this daemon handed it.
//! §4's eligibility ceiling now measures the cleaned string, which is
//! shorter -- so a notification whose raw form was over `reword.max_chars`
//! but whose spoken form is not becomes eligible, which is what §2 asked
//! for. And the cleaned copy stays *inside* the plan: it is what the guard
//! measured and what goes on the wire, and it is never what this module
//! hands back. Everything handed back is the original, because
//! `Engine::submit` cleans whatever it is given and `clean` is not
//! idempotent -- see `RewordPlan::original` and
//! `sayd_core::cleanup::tests::clean_is_not_idempotent_and_callers_must_not_assume_it_is`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use sayd_core::cleanup::clean;
use sayd_core::config::{CleanupConfig, Provider, RewordConfig};
use sayd_core::reword::{check, eligible, Ineligible};

#[cfg(feature = "reword")]
pub mod http;

/// How long the client keeps trying *past* the configured deadline before
/// it gives up on its own.
///
/// The client's ceiling has to bound the blocking thread, which
/// `tokio::time::timeout` cannot (see the module doc). It used to be this
/// number flat, and 10 s was comfortably longer than the 2 s
/// `reword.timeout_ms` could ever be. With that ceiling gone the flat value
/// became a lie in the one direction that matters: a configured deadline of
/// 30 s against a 10 s transport ceiling is not a 30 s deadline, it is a
/// 10 s one wearing a larger number, and the setting would not do what it
/// says.
///
/// So the ceiling is [`http_ceiling`], the deadline plus this, and this is
/// the *grace* -- what it buys is the settings window's Test row, which
/// deliberately outlives the deadline so it can report how much too slow a
/// provider is rather than only that it was too slow. Ten seconds of grace
/// is what that row had when the deadline could not exceed two.
pub const REWORD_HTTP_GRACE: Duration = Duration::from_secs(10);

/// The client's own ceiling on one request, for the config it will run
/// under. Set per request on the `ureq` call, so it bounds the *thread*.
///
/// **The transport must never be what ends a rewrite first.** It is the one
/// bound in this path the user did not choose and cannot see: a request the
/// client cut off at its own ceiling is indistinguishable, from outside,
/// from a provider that never answered. Strictly longer than the deadline
/// (by [`REWORD_HTTP_GRACE`]) rather than equal to it, so the ordering does
/// not rest on which of two clocks started first -- `attempt` starts its
/// budget before the permit and the `spawn_blocking` hop, and `ureq` starts
/// this one inside the job, but nothing enforces that gap.
///
/// Finite, still: `Runtime::drop` waits for blocking tasks that have
/// started, and the daemon's worst case is [`REWORD_MAX_INFLIGHT`] threads
/// each living at most this long. A user who sets a five-minute deadline has
/// chosen a five-minute worst case, which is the difference between a bound
/// derived from a setting and a bound with no setting behind it.
pub fn http_ceiling(cfg: &RewordConfig) -> Duration {
    Duration::from_millis(cfg.timeout_ms).saturating_add(REWORD_HTTP_GRACE)
}

/// What the settings window's Test button waits, rather than `timeout_ms`.
///
/// A test that gave up at the configured deadline could only ever say "too
/// slow" and could never say *how much* too slow -- which is the number
/// needed to choose a better deadline, or to conclude the provider is
/// hopeless. Equal to [`http_ceiling`] because the client's own ceiling is
/// what actually ends the request; there is nothing to be gained by waiting
/// longer and nothing to be gained by giving up sooner. That is also why the
/// grace has to survive an unbounded deadline: at a 30 s deadline a test
/// that stopped at 30 s would be back to saying only "too slow".
///
/// Read by `settings::model::outcome_for_error`, which is what puts the
/// number in front of a user: "No answer after 11.5 s", beside the deadline
/// that was not waited.
pub fn test_ceiling(cfg: &RewordConfig) -> Duration {
    http_ceiling(cfg)
}

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
/// How long a half-open probe may be outstanding before the next arrival
/// may take one instead.
///
/// §8's "then let one through" is a *token*, and a token that is taken and
/// never resolved is a breaker stuck half-open for the life of the process:
/// nothing would ever be attempted again. Every way of taking one without
/// resolving it is a caller that passed [`RewordState::allow`] and then did
/// not attempt -- no permit was free, or `context` could not build a
/// client. [`http_ceiling`] is the bound because it is the longest a real
/// attempt can take, so a probe older than this cannot still be in flight;
/// the cost of the safety net is at worst one extra request per ceiling's
/// worth of an open breaker, against a bug whose cost is the feature never
/// running again.
///
/// A function of the config rather than the constant it was, for the same
/// reason [`http_ceiling`] is: with no ceiling on `timeout_ms` there is no
/// longest attempt to write down. Taken from the `cfg` the caller is asking
/// about, which is the config that attempt would have run under.
fn transport_probe_ttl(cfg: &RewordConfig) -> Duration {
    http_ceiling(cfg)
}
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

/// A breaker window: the instant it lifts, or "not on any timeline this
/// clock will produce".
///
/// `Instant::checked_add` returns `None` on overflow, and a `None` deadline
/// stored in an `Option<Instant>` reads as *no window at all* -- the
/// opposite of what the arithmetic that produced it was trying to say. This
/// type makes the failure direction impossible to get wrong: overflow
/// becomes [`Window::Forever`], which is shut for every `now`, so the
/// degradation is "speak the text as written", which is what every other
/// row of §8 degrades to as well. `notify::policy::is_expired` makes
/// exactly this call for exactly this arithmetic (`None` => not expired)
/// and says why in the same words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    Until(Instant),
    Forever,
}

impl Window {
    /// A window of `d` opening at `now`, failing closed if that instant
    /// cannot be represented.
    fn opening(now: Instant, d: Duration) -> Window {
        match now.checked_add(d) {
            Some(t) => Window::Until(t),
            None => Window::Forever,
        }
    }

    /// Is the window still shut at `now`?
    fn shut_at(self, now: Instant) -> bool {
        match self {
            Window::Until(t) => now < t,
            Window::Forever => true,
        }
    }

    /// The later of two windows.
    ///
    /// Two rewrites may be in flight, so two 429s in a row is ordinary
    /// rather than exotic -- and assigning rather than extending means a
    /// second, shorter `Retry-After` silently cancels a longer one that is
    /// still in force. Measured: `record(3600 s)` then `record(1 s)` and
    /// the next attempt went out two seconds later.
    fn later(self, other: Window) -> Window {
        match (self, other) {
            (Window::Forever, _) | (_, Window::Forever) => Window::Forever,
            (Window::Until(a), Window::Until(b)) => Window::Until(a.max(b)),
        }
    }
}

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
// the variants is gated. `http::parse_response` is what constructs them, and
// it exists only under `--features reword`.
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
    /// The client's own [`http_ceiling`] was hit. Counted toward the
    /// same breaker as [`RewordError::Unreachable`]: this row exists to
    /// bound the thread, not the utterance -- the utterance was spoken
    /// 8.5 s earlier.
    Ceiling,
    /// No `choices[0].message.content`, an unparseable body, or an `error`
    /// object that classifies as nothing more specific.
    Malformed(String),
    /// The generation stopped because it ran out of tokens, not because it
    /// was finished. `reasoning` records whether the budget went on a
    /// reasoning block, which is the difference between "your model thinks
    /// and this provider was not told not to" and "your text needs a longer
    /// answer than the cap allows".
    ///
    /// Deliberately not transport-class. The provider answered, promptly and
    /// completely; there is nothing for a breaker built out of
    /// [`RewordError::Unreachable`] and [`RewordError::Ceiling`] to protect
    /// against, and opening it would take the feature offline for a minute
    /// over a model that is working exactly as configured.
    Truncated { reasoning: bool },
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
///
/// **This is the only function that calls [`RewordState::record`] for an
/// attempt**, and it calls it exactly once per outcome: the job records the
/// answer it got (from the blocking thread, before it drops its permit, so
/// a provider that outlives the caller's deadline still teaches the
/// breaker), and the caller records the deadline or the panic it saw. A
/// caller that recorded as well would count one failure twice.
///
/// **Private.** It was `pub`, and a probe measured what that bought: a
/// caller in `dbus.rs` calling `context(cfg)` and then this compiled clean
/// and skipped the eligibility rule, the auth latch, the transport breaker
/// and the rate limiter -- the bypass [`RewordPlan`]'s own doc calls
/// structurally impossible. [`reword_or_original`] is the only caller and
/// [`RewordPlan::resolve`] is the only caller of *that*, so the claim now
/// holds. The settings window's Test row is the one deliberate exception and
/// does not come through here at all -- see `settings::model::run_reword_test`,
/// whose doc says so and why.
async fn attempt(
    rewriter: Arc<dyn Rewriter>,
    state: Arc<RewordState>,
    cfg: &RewordConfig,
    text: String,
    budget: Duration,
) -> (Attempt, Duration) {
    let started = Instant::now();
    let Some(permit) = state.try_permit() else {
        state.record(cfg, &Attempt::Busy, Instant::now());
        return (Attempt::Busy, started.elapsed());
    };
    // §7's privacy line, behind the permit rather than in front of it: it
    // is what a user greps to learn where their text goes, so it must not
    // announce a send for an utterance that then finds no permit and makes
    // no request. Past this point a request is certain -- the closure below
    // always calls `reword`.
    state.note_endpoint(cfg);
    let job_state = state.clone();
    let job_cfg = cfg.clone();
    let job = tokio::task::spawn_blocking(move || {
        // `let _permit`, never `let _`: binding to `_` drops the permit at
        // once and is precisely the bug §2 exists to prevent. Named, it
        // lives to the end of this closure, so the permit is released when
        // the *job* finishes -- not when the deadline below fires.
        let _permit = permit;
        let outcome = Attempt::Answered(rewriter.reword(&text));
        // Folded in here rather than by the caller, and this is the whole
        // of the Ceiling fix: past the deadline the caller is gone and this
        // outcome would otherwise die with the abandoned `JoinHandle`. The
        // answer still goes nowhere -- `record` takes no engine handle and
        // returns nothing -- but the breaker learns what happened. Before
        // the permit is released, so a breaker that opens here is open for
        // the next arrival rather than one request later.
        job_state.record(&job_cfg, &outcome, Instant::now());
        outcome
    });
    match tokio::time::timeout(budget, job).await {
        Ok(Ok(outcome)) => (outcome, started.elapsed()),
        // The blocking task itself failed, which means it panicked. Not
        // reachable through the shipped client, but a panic here must not
        // take the announcement with it -- and the unwind skipped the
        // job's own `record`, so this one is the caller's to make.
        Ok(Err(join)) => {
            let outcome = Attempt::Answered(Err(RewordError::Malformed(format!(
                "the rewrite task failed: {join}"
            ))));
            state.record(cfg, &outcome, Instant::now());
            (outcome, started.elapsed())
        }
        // The `.await` on the handle is abandoned here. The job keeps
        // running (bounded by the client's own `http_ceiling`) and keeps its permit
        // (bounded by REWORD_MAX_INFLIGHT), and its eventual answer has
        // nowhere to go -- but its *outcome* still reaches `record` from
        // the job itself, which is what stops a permanently stuck provider
        // from being invisible to §8's breaker.
        Err(_elapsed) => {
            state.record(cfg, &Attempt::Deadline, Instant::now());
            (Attempt::Deadline, started.elapsed())
        }
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
    /// How long [`RewordState::emit`] stalls before it prints, standing in
    /// for an `eprintln!` that blocks in `write(2)`. Zero everywhere but the
    /// one test that measures the rule below, and absent entirely from the
    /// shipped binary.
    #[cfg(test)]
    emit_stall: Duration,
}

/// A line this module owes the journal, built while [`Inner`] is held and
/// printed after the guard is dropped.
///
/// The rule it exists to enforce: **nothing under `RewordState::inner` may
/// perform I/O.** `notify::monitor`'s `tokio::select!` arm reaches
/// [`RewordState::allow`] on every notification and that takes this same
/// mutex, while [`RewordState::record`] runs on a *blocking-pool* thread
/// inside the rewrite job. An `eprintln!` into a pipe nobody is draining -- a
/// stalled journald, a terminal whose reader has stopped -- blocks in
/// `write(2)`, and held across that lock it holds the monitor's arm with it:
/// the `MessageStream` stops being polled and `ticker.tick()` stops firing
/// for as long as the write is stuck. Measured on the previous shape, with a
/// 2000 ms stall standing in for the blocked write: the arm was held for
/// 1.70 s. This daemon has already shipped two bugs of that shape.
///
/// So every branch that has something to say *builds* it under the lock,
/// where the counters it names are consistent, and says it outside.
enum Journal {
    /// An `info:` or `warning:` line, printed unconditionally.
    Line(String),
    /// A `debug:` line, printed only when `SAYD_DEBUG` is set -- see
    /// [`debug`].
    Debug(String),
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
    transport_open_until: Option<Window>,
    /// The half-open probe: when the token minted by an expired cooldown
    /// stops being honoured, or `None` when no probe is out.
    ///
    /// §8 says "stop attempting for 60 s, **then let one through**", and
    /// *one* is the whole of it. Clearing the window on the first `allow`
    /// past it -- which is what this used to do -- opens the gate for
    /// everyone until three fresh failures accumulate: measured, 100 of 100
    /// arrivals at the same instant past the cooldown were admitted. The
    /// token is minted by `allow`, spent by the one caller it admits, and
    /// resolved by that attempt's `record`: an answer closes the breaker, a
    /// transport failure re-arms another full cooldown. See
    /// [`transport_probe_ttl`] for what happens to a token nobody resolves.
    transport_probe: Option<Window>,
    rate_limited_until: Option<Window>,
    deadlines: u64,
    /// Log-once latches, one per §8 row that says "once per run".
    not_configured_logged: bool,
    plain_http_logged: bool,
    too_long_logged: bool,
    outage_logged: bool,
    /// MINOR 2's row. §8 gives every failure a line, and a 429 was the one
    /// that had none: the window was set, the feature stopped rewriting for
    /// up to an hour, and the journal said nothing at all -- while the
    /// provider's own `error.message`, which the client had already parsed
    /// and sanitised, was dropped on the floor. Latched like
    /// [`Inner::outage_logged`] and cleared by the same success, so a
    /// standing rate limit is one line rather than one per notification.
    rate_limit_logged: bool,
    model_logged: bool,
    /// MINOR 5's row: an explicit `--reword` against a build that has no
    /// client in it. Its own latch rather than one of the others because it
    /// is the one line whose cause is the *binary*, not the configuration --
    /// nothing a user edits will clear it, so saying it twice would be
    /// saying it twice for the life of the process.
    unavailable_logged: bool,
    /// A1: every other row in [`Inner::fold`]'s match gates its line behind
    /// a latch like this one; `Truncated` did not. Because `Truncated`
    /// deliberately does not open the transport breaker (§8: it is not
    /// transport-class), nothing else throttles the attempts either -- a
    /// reasoning model behind `provider = "generic"` truncates on nearly
    /// every request, and an ungated line here was one `warning:` per
    /// notification, forever. Cleared on success alongside
    /// [`Inner::outage_logged`] and [`Inner::rate_limit_logged`], so a
    /// standing truncation is one line rather than one per rewrite.
    truncated_logged: bool,
}

impl RewordState {
    pub fn new() -> Arc<RewordState> {
        Arc::new(RewordState {
            permits: Arc::new(tokio::sync::Semaphore::new(REWORD_MAX_INFLIGHT)),
            inner: Mutex::new(Inner::default()),
            #[cfg(test)]
            emit_stall: Duration::ZERO,
        })
    }

    /// A state whose journal writes take `stall` to complete, standing in for
    /// an `eprintln!` blocked in `write(2)` on a pipe nobody is reading.
    ///
    /// Per instance rather than a global switch, so the test that measures
    /// [`Journal`]'s rule cannot slow any other test down: this suite runs in
    /// one process and every other test builds its own state.
    #[cfg(test)]
    fn with_stalled_journal(stall: Duration) -> Arc<RewordState> {
        Arc::new(RewordState {
            permits: Arc::new(tokio::sync::Semaphore::new(REWORD_MAX_INFLIGHT)),
            inner: Mutex::new(Inner::default()),
            emit_stall: stall,
        })
    }

    /// Print what the lock was released for. See [`Journal`]: every caller
    /// must have dropped its `MutexGuard` before it gets here.
    fn emit(&self, journal: impl IntoIterator<Item = Journal>) {
        for entry in journal {
            #[cfg(test)]
            if !self.emit_stall.is_zero() {
                std::thread::sleep(self.emit_stall);
            }
            match entry {
                Journal::Line(line) => eprintln!("{line}"),
                Journal::Debug(line) => debug(format_args!("{line}")),
            }
        }
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
    ///
    /// Not a pure predicate: past an expired transport cooldown this is the
    /// transition that mints and spends §8's one probe, so a caller that
    /// asks and then does not attempt costs the run a probe (bounded by
    /// [`transport_probe_ttl`]) rather than, as it used to, the whole
    /// breaker.
    pub fn allow(&self, cfg: &RewordConfig, now: Instant) -> Result<(), Blocked> {
        let mut i = lock(&self.inner);
        if i.auth_latched_for.as_ref() == Some(cfg) {
            return Err(Blocked::AuthLatched);
        }
        // "then let one through", and *one* is the load-bearing word: the
        // window is not cleared here. It stays until a probe comes back
        // with an answer, so an expired cooldown admits one caller and
        // refuses everyone behind it.
        let probing = match i.transport_open_until {
            Some(w) if w.shut_at(now) => return Err(Blocked::TransportOpen),
            Some(_) => match i.transport_probe {
                // Somebody else's probe is still out; theirs is the one
                // that decides.
                Some(p) if p.shut_at(now) => return Err(Blocked::TransportOpen),
                _ => true,
            },
            None => false,
        };
        if let Some(w) = i.rate_limited_until {
            if w.shut_at(now) {
                return Err(Blocked::RateLimited);
            }
            i.rate_limited_until = None;
        }
        // Last, so a caller turned away by a later row does not spend it.
        if probing {
            i.transport_probe = Some(Window::opening(now, transport_probe_ttl(cfg)));
        }
        Ok(())
    }

    /// Fold one outcome into the breakers, and log whatever §8 says this
    /// row owes -- once per run, or the first of a standing outage, never
    /// once per utterance.
    ///
    /// Takes no engine handle and returns nothing, which is why it is safe
    /// to call from inside the rewrite job: §2's rule is that a late
    /// *answer* is dropped unread, and an outcome is not a thing that can
    /// be spoken. See [`attempt`], the only caller for an attempt.
    ///
    /// Every line it owes is built under the lock and printed after it, for
    /// the reason [`Journal`] gives: this method runs on a blocking-pool
    /// thread and the mutex it takes is the one the notification monitor's
    /// `select!` arm takes on every arrival.
    pub fn record(&self, cfg: &RewordConfig, outcome: &Attempt, now: Instant) {
        let mut i = lock(&self.inner);
        let journal = i.fold(cfg, outcome, now);
        drop(i);
        self.emit(journal);
    }

    /// Forget a latched auth failure. Called only by a *successful* test in
    /// the settings window, which is the one event that proves the key
    /// works -- and the only way back when the key came from the
    /// environment.
    ///
    /// `settings::model::run_reword_test` is its only caller, and it calls
    /// it on any answer at all -- including one §3's guard then throws away,
    /// which still proves the key works.
    ///
    /// Takes the config the answer came back under, and clears the latch
    /// only if that is the config it was set for. Clearing unconditionally
    /// would let a successful test of *some other* endpoint re-arm a config
    /// nothing had fixed -- and it buys nothing, because [`allow`] already
    /// compares the latched config against the one being asked about, so a
    /// latch held for a config nobody is running blocks nothing.
    ///
    /// [`allow`]: RewordState::allow
    pub fn clear_auth_latch(&self, cfg: &RewordConfig) {
        let mut i = lock(&self.inner);
        if i.auth_latched_for.as_ref() == Some(cfg) {
            i.auth_latched_for = None;
        }
    }

    /// Announce where text is going, once per run per resolved endpoint,
    /// and warn once about cleartext to a non-loopback host. Returns
    /// whether this was the first time -- which the settings window uses to
    /// say that a first request includes connection setup.
    ///
    /// Called from [`attempt`] *after* a permit is in hand, never before:
    /// this line is what a user greps to find out where their text goes, so
    /// a run in which nothing left the machine must not contain it.
    pub fn note_endpoint(&self, cfg: &RewordConfig) -> bool {
        let key = format!("{}|{}", cfg.base_url, cfg.model);
        let mut i = lock(&self.inner);
        if !i.announced.insert(key) {
            return false;
        }
        // Built here, printed below the `drop` -- see [`Journal`]. Two lines
        // rather than one, so a `Vec` rather than an `Option`.
        let mut journal = vec![Journal::Line(format!(
            "info: reword: sending text to {} (model {})",
            cfg.base_url, cfg.model
        ))];
        if !i.plain_http_logged {
            if let Ok(endpoint) = sayd_core::reword::parse_base_url(&cfg.base_url) {
                if endpoint.scheme == "http" && !sayd_core::reword::is_loopback(&endpoint.host) {
                    // A security statement rather than a trust judgement:
                    // cleartext on the wire is a fact about the transport,
                    // not an opinion about the operator.
                    journal.push(Journal::Line(
                        "warning: reword: base_url is plain HTTP to a non-loopback host; \
                         text will cross the network unencrypted"
                            .to_string(),
                    ));
                    i.plain_http_logged = true;
                }
            }
        }
        drop(i);
        self.emit(journal);
        true
    }

    /// Has this endpoint been announced this run?
    ///
    /// Read only by `dbus`'s own tests, which use it to check that a rewrite
    /// really reached the provider before timing it. Production code that
    /// wants this asks [`RewordState::note_endpoint`] instead, whose return
    /// value is the same fact taken from the call that establishes it --
    /// see `settings::model::run_reword_test`.
    // `#[allow(dead_code)]`: that caller is behind `--features reword`.
    #[allow(dead_code)]
    pub fn endpoint_seen(&self, cfg: &RewordConfig) -> bool {
        lock(&self.inner)
            .announced
            .contains(&format!("{}|{}", cfg.base_url, cfg.model))
    }

    /// Has [`RewordState::note_unavailable`]'s line already been said this
    /// run? Test-only: reading the latch is how that rule is checked without
    /// capturing stderr.
    #[cfg(test)]
    fn unavailable_logged(&self) -> bool {
        lock(&self.inner).unavailable_logged
    }

    /// MINOR 5: say once, per run, that this build cannot rewrite anything.
    ///
    /// [`RewordState::record`] swallows [`RewordError::Unavailable`], and
    /// that is right for the path it was written for: `[reword] enabled` in
    /// a feature-off build would otherwise owe a line per notification for a
    /// rewrite nobody asked for. The explicit path *did* ask, and `dbus.rs`
    /// promises it "the diagnosis is in the log, once per run" -- so
    /// [`RewordPlan::requested`] says it here instead, where the once-per-run
    /// latch already lives and where the line is built under the lock and
    /// printed outside it like every other one ([`Journal`]).
    ///
    /// Returns whether it printed, which is what makes the latch testable
    /// without capturing stderr.
    pub fn note_unavailable(&self) -> bool {
        let mut i = lock(&self.inner);
        if i.unavailable_logged {
            return false;
        }
        i.unavailable_logged = true;
        drop(i);
        self.emit([Journal::Line(
            "warning: reword: this build of sayd has no rewriting client, so an \
             explicit --reword speaks the text as written; rebuild with \
             `--features reword` (said once per run)"
                .to_string(),
        )]);
        true
    }

    /// §4's logging rule: over-long text is worth one line per run, short
    /// text is worth none at all.
    pub fn note_ineligible(&self, why: Ineligible) {
        if why != Ineligible::TooLong {
            return;
        }
        let mut i = lock(&self.inner);
        if i.too_long_logged {
            return;
        }
        i.too_long_logged = true;
        drop(i);
        // Outside the lock, for the reason [`Journal`] gives -- and this one
        // is reached straight from `will_reword`, which the notification
        // monitor's `select!` arm calls on every arrival.
        self.emit([Journal::Line(
            "info: reword: some text is longer than reword.max_chars and is spoken \
             as written (said once per run)"
                .to_string(),
        )]);
    }
}

impl Inner {
    /// The whole of [`RewordState::record`]'s decision: fold `outcome` into
    /// the breakers and return the line §8 says this row owes, if any.
    ///
    /// Split out of `record` so both halves are reachable from a test. The
    /// lines are the part of §8 that has no other observable: a latch that
    /// is set and a line that is printed are two different promises, and
    /// MINOR 2 and MINOR 3 were both a row keeping one of them and not the
    /// other. Building the line here and printing it in the caller is also
    /// [`Journal`]'s rule -- nothing under this mutex may perform I/O.
    fn fold(&mut self, cfg: &RewordConfig, outcome: &Attempt, now: Instant) -> Option<Journal> {
        // Bound rather than spelled `self` throughout: this body moved here
        // whole from `record`, where the guard it read through was `i`, and
        // a rename would have made the diff of a fix look like a rewrite.
        let i = self;
        let mut journal = None;
        match outcome {
            // Nothing was sent, so nothing was learned -- and if this
            // caller was holding §8's probe, it hands it back unspent
            // rather than sitting on it for the TTL.
            Attempt::Busy => i.transport_probe = None,
            Attempt::Deadline => {
                i.deadlines += 1;
                if i.deadlines == 1 || i.deadlines.is_multiple_of(DEADLINE_LOG_EVERY) {
                    journal = Some(Journal::Line(format!(
                        "info: a rewrite did not answer within {} ms; spoke the text as \
                         written ({} so far this run)",
                        cfg.timeout_ms, i.deadlines
                    )));
                }
            }
            Attempt::Answered(Ok(_)) => {
                i.transport_answered();
                i.outage_logged = false;
                i.rate_limit_logged = false;
                i.truncated_logged = false;
            }
            Attempt::Answered(Err(e)) => match e {
                RewordError::Auth {
                    status,
                    host,
                    message,
                } => {
                    // MINOR 3: the *latch* is what decides whether this
                    // line is owed, rather than a `bool` of its own. They
                    // used to disagree -- the latch is per-config and the
                    // log latch was global and per-run -- so a 403 from a
                    // second endpoint (a settings-window edit, an
                    // `api_key_env` pointing somewhere else) latched
                    // silently and the user was left with a feature that
                    // had stopped and no line saying why. One condition
                    // cannot drift from itself: the line is said exactly
                    // when a config that was not latched becomes latched,
                    // and `clear_auth_latch` therefore also restores the
                    // right to say it again, which is right -- a key that
                    // tested good and then failed is news.
                    if i.auth_latched_for.as_ref() != Some(cfg) {
                        journal = Some(Journal::Line(format!(
                            "warning: reword: {host} rejected the API key (HTTP {status}{}); \
                             speaking text as written until the configuration changes",
                            message
                                .as_deref()
                                .map(|m| format!(": {m}"))
                                .unwrap_or_default()
                        )));
                    }
                    i.auth_latched_for = Some(cfg.clone());
                    i.transport_answered();
                }
                RewordError::NoSuchModel {
                    status,
                    model,
                    message,
                } => {
                    if !i.model_logged {
                        journal = Some(Journal::Line(format!(
                            "warning: reword: the provider does not have model {model:?} \
                             (HTTP {status}{}); speaking text as written",
                            message
                                .as_deref()
                                .map(|m| format!(": {m}"))
                                .unwrap_or_default()
                        )));
                        i.model_logged = true;
                    }
                    i.transport_answered();
                }
                RewordError::RateLimited {
                    retry_after,
                    message,
                } => {
                    let wait = retry_after
                        .unwrap_or(RATE_LIMIT_BACKOFF)
                        .min(RATE_LIMIT_MAX_BACKOFF);
                    let opening = Window::opening(now, wait);
                    // Extended, never replaced: see `Window::later`.
                    i.rate_limited_until = Some(match i.rate_limited_until {
                        Some(standing) => standing.later(opening),
                        None => opening,
                    });
                    // MINOR 2: this row owed a line and had none. The
                    // feature stops rewriting for `wait` -- up to an hour
                    // with a `Retry-After` -- and a user watching
                    // rewriting quietly stop deserves the reason, not a
                    // silence indistinguishable from the feature being
                    // switched off. The provider's own words are already
                    // parsed and bounded by the client; dropping them here
                    // threw away the only part that says *which* limit.
                    if !i.rate_limit_logged {
                        journal = Some(Journal::Line(format!(
                            "warning: reword: the provider is rate limiting (HTTP 429{}); \
                             not attempting another rewrite for {:.0} s",
                            message
                                .as_deref()
                                .map(|m| format!(": {m}"))
                                .unwrap_or_default(),
                            wait.as_secs_f64()
                        )));
                        i.rate_limit_logged = true;
                    }
                    i.transport_answered();
                }
                RewordError::Unreachable(detail) => {
                    if !i.outage_logged {
                        journal = Some(Journal::Line(format!(
                            "warning: reword: could not reach the provider: {detail}"
                        )));
                        i.outage_logged = true;
                    }
                    i.fail_transport(now);
                }
                RewordError::Ceiling => {
                    if !i.outage_logged {
                        journal = Some(Journal::Line(format!(
                            "warning: reword: the provider did not answer within {:.0} s",
                            http_ceiling(cfg).as_secs_f64()
                        )));
                        i.outage_logged = true;
                    }
                    i.fail_transport(now);
                }
                RewordError::NotConfigured(reason) => {
                    if !i.not_configured_logged {
                        journal = Some(Journal::Line(format!(
                            "warning: reword: {reason}; speaking text as written"
                        )));
                        i.not_configured_logged = true;
                    }
                }
                RewordError::Unavailable => {}
                RewordError::Malformed(detail) => {
                    journal = Some(Journal::Debug(format!(
                        "reword: unusable response: {detail}"
                    )));
                    // The transport did its job -- something came back and
                    // the client could not use it -- so this resolves a
                    // probe as an answer, and the next notification still
                    // gets its chance.
                    i.transport_answered();
                }
                RewordError::Truncated { reasoning } => {
                    // A1: gated like every other row, for the reason
                    // `Inner::truncated_logged`'s doc comment gives.
                    if !i.truncated_logged {
                        journal = Some(Journal::Line(format!(
                            "warning: reword: the model reached its {cap}-token cap \
                             without finishing{cause}; speaking text as written",
                            cap = cfg.max_tokens(),
                            // A2: `HttpRewriter::new` refuses any config whose
                            // provider does not resolve before a request ever
                            // leaves the machine, so a config that reached
                            // this arm is already `generic` or `llama-cpp` --
                            // never unset. The two need different advice:
                            // `generic` sends nothing to suppress reasoning,
                            // so naming `llama-cpp` is real advice; a config
                            // already on `llama-cpp` sent
                            // `chat_template_kwargs` and the server reasoned
                            // anyway, so repeating the same suggestion would
                            // be a circle -- the honest statement is that the
                            // server did not honour the request.
                            cause = match reasoning {
                                false => String::new(),
                                true if cfg.resolved_provider() == Some(Provider::LlamaCpp) => {
                                    " (it spent the budget reasoning even though \
                                     reword.provider is already \"llama-cpp\"; the \
                                     server did not honour the request -- try a \
                                     model that does not reason)"
                                        .to_string()
                                }
                                true => " (it spent the budget reasoning -- set \
                                    reword.provider = \"llama-cpp\" so it can be \
                                    told not to)"
                                    .to_string(),
                            },
                        )));
                        i.truncated_logged = true;
                    }
                    // The transport did its job, so this resolves a probe as
                    // an answer, exactly as `Malformed` does.
                    i.transport_answered();
                }
            },
        }
        journal
    }

    /// One transport-class failure: [`RewordError::Unreachable`] and
    /// [`RewordError::Ceiling`] are the same row of §8's table and must
    /// count toward the same breaker, so they share the one body rather
    /// than two that can drift apart.
    fn fail_transport(&mut self, now: Instant) {
        if self.transport_probe.take().is_some() {
            // The one §8 let through failed as well. Another full cooldown,
            // not a fresh count of three: the evidence that the provider is
            // still down is the probe, and it is complete on its own.
            self.transport_open_until = Some(Window::opening(now, TRANSPORT_BREAKER_COOLDOWN));
            self.transport_failures = 0;
            return;
        }
        self.transport_failures += 1;
        if self.transport_failures >= TRANSPORT_FAILURES_TO_OPEN {
            self.transport_open_until = Some(Window::opening(now, TRANSPORT_BREAKER_COOLDOWN));
            self.transport_failures = 0;
        }
    }

    /// The provider answered -- with a rewrite, or with a 401, a 429 or a
    /// body the client could not parse. Any of those is proof the transport
    /// works, which is the only question this breaker asks, so the window
    /// closes, a probe resolves as a success, and the consecutive-failure
    /// count starts again from zero.
    fn transport_answered(&mut self) {
        self.transport_probe = None;
        self.transport_open_until = None;
        self.transport_failures = 0;
    }
}

/// Will this text be reworded at all? Decided synchronously and cheaply,
/// *before* anything is spawned, so an ineligible submission costs one pass
/// over a short string and a mutex.
///
/// **Call it once per arrival, then attempt.** It is not a pure predicate:
/// past an expired transport cooldown [`RewordState::allow`] mints and
/// spends §8's one half-open probe, so a caller that asks twice and attempts
/// once costs the run a probe until [`transport_probe_ttl`] expires. That is
/// why this is private and [`RewordPlan::admit`] is its only caller: one
/// constructor, one ask, one attempt, and no way for a call site to hold the
/// answer and then decide something else with it.
fn will_reword(text: &str, cfg: &RewordConfig, state: &RewordState) -> bool {
    if let Err(why) = eligible(text, cfg.max_chars) {
        state.note_ineligible(why);
        return false;
    }
    state.allow(cfg, Instant::now()).is_ok()
}

/// The text to speak, and what to speak instead if the engine will not take
/// it.
///
/// CRITICAL 2. A rewrite is admitted by `sayd_core::reword::check` at up to
/// `original * 3 / 2 + 32` characters, and `Engine::submit` refuses anything
/// over `max_chars` -- two ceilings that are not related to each other, so a
/// 1000-character announcement under a 1000-character `max_chars` can come
/// back as a perfectly valid 1200-character rewrite and be refused. Measured
/// end to end on a private bus: `text is 1200 characters, limit is 1000`, the
/// announcement lost, and the daemon in global `State::Error` without ever
/// having entered `speaking`. Both halves of that are now fixed -- the engine
/// no longer sets *global* error state for a `Source::Notification` rejection
/// -- but the announcement is only saved by this: a submission that is
/// refused is retried with the text the rewrite replaced, which is the text
/// that would have been spoken with no rewriting feature at all.
///
/// [`Spoken::fallback`] is `None` when `text` *is* the original, so a caller
/// that retries on every refusal cannot submit the same string twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spoken {
    /// The text to submit.
    pub text: String,
    /// What to submit instead if the engine refuses [`Spoken::text`], or
    /// `None` when that already is the text as written.
    pub fallback: Option<String>,
}

impl Spoken {
    /// Text nothing rewrote. There is nothing to fall back to: this is the
    /// fallback.
    pub fn as_written(text: String) -> Spoken {
        Spoken {
            text,
            fallback: None,
        }
    }
}

/// The text to speak: the rewrite if one arrived in time and passed the
/// guard, the original otherwise.
///
/// **Holds no `EngineHandle` and returns a `String`.** That is the whole of
/// the drop rule: the caller submits, so a late answer has nowhere to go.
/// Do not add a submit callback to this signature.
///
/// Assumes [`will_reword`] has already said yes -- and does not check, which
/// is why every call site reaches this only through a [`RewordPlan`], whose
/// sole constructor calls `will_reword`. Nothing here consults
/// [`RewordState::allow`], so a caller that skips it bypasses the auth
/// latch, the transport breaker and the rate limiter entirely.
///
/// Private, therefore: [`RewordPlan::resolve`] is the only caller in the
/// daemon, which is what makes "reached only through a plan" a fact about
/// the module boundary rather than a convention two files agree to keep.
async fn reword_or_original(
    sent: String,
    original: String,
    cfg: &RewordConfig,
    rewriter: Arc<dyn Rewriter>,
    state: Arc<RewordState>,
) -> Spoken {
    let budget = Duration::from_millis(cfg.timeout_ms);
    // No `record` here: `attempt` owns it end to end, because the outcome
    // of an attempt that outlived its deadline is only reachable from
    // inside the job. Recording here as well would count it twice.
    let (outcome, _elapsed) = attempt(rewriter, state, cfg, sent.clone(), budget).await;
    let Attempt::Answered(Ok(candidate)) = outcome else {
        return Spoken::as_written(original);
    };
    // The guard compares like with like -- the candidate against the string
    // the provider was actually given -- while everything handed *back* is
    // the original, because `Engine::submit` is what cleans it. See
    // [`RewordPlan::original`].
    match check(&sent, &candidate) {
        // The one branch that carries a fallback: past here the engine is
        // the only thing left that can refuse this string, and if it does
        // the announcement must not be lost. See [`Spoken`].
        Ok(rewritten) => Spoken {
            text: rewritten,
            fallback: Some(original),
        },
        Err(reason) => {
            debug(format_args!(
                "reword: rejected a candidate ({}): {:?}",
                reason.phrase(),
                sayd_core::reword::truncate_for_debug(&candidate, DEBUG_SNIPPET_CHARS)
            ));
            Spoken::as_written(original)
        }
    }
}

/// Text a person or an application wrote: a `Say`, `SaySelection` or
/// `SayClipboard` body, or an application's own notification. §1's rewrite
/// exists for exactly this.
///
/// A newtype **around the string** rather than a label passed beside it, and
/// that is the whole of the guarantee. This started as a `bool` parameter on
/// `notify::monitor::speak` and then became an `Origin` enum passed
/// alongside the text; both shapes asked each call site to *state* where its
/// text came from, and a call site that states something can state it
/// wrongly. Measured on the enum, not imagined: flipping the coalescing
/// ticker's arm to the written value passed 263 of 263 tests while sending a
/// line this daemon composed itself to a provider, and flipping
/// `Decision::Speak`'s arm to the composed one passed 263 of 263 while
/// switching the whole feature off with `enabled = true` -- that second one
/// was found sitting uncommitted in the tree.
///
/// Carried by the value, provenance is not a thing a call site can state at
/// all: `Limiter::decide` hands back a `Written`, `Limiter::due` hands back
/// `Composed`s, and [`RewordPlan::automatic`] takes `impl Into<Origin>`.
/// Neither call site names an origin, so neither can name the wrong one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written(pub String);

/// Text this daemon composed: the coalesced `"N more notifications"`
/// follow-up `notify::policy` builds when a window closes (§2). Never
/// reworded, for three reasons that are all about the line itself:
/// `notify::policy::announcement` builds it from a template, so it is
/// already a sentence written for the ear and a rewrite can only make it
/// worse; it costs a provider round trip for text this daemon wrote; and
/// its whole job is to arrive the moment the window closes, which a
/// rewrite would delay by up to `timeout_ms`.
///
/// **Not** because rewriting it would let it overtake its opener, which
/// is what this comment, spec §2 and the brief all used to say. That
/// reasoning is backwards and must not be copied into a later task:
/// *excluding* the follow-up is what makes it instant, and rewriting it
/// would push it later, never earlier.
///
/// The inversion those texts describe is real, but it arrives from the
/// other side -- the *opener* is what a rewrite delays. A window that
/// closes before its opener has been submitted lets the follow-up reach
/// an idle engine first and start playing, and `Policy::Front` does not
/// save the opener: `Front` jumps ahead of what is pending, not ahead of
/// what is already playing. That is bounded where it lives, by
/// `sayd_core::config::notify_cooldown_min_secs`, which keeps every
/// non-zero cooldown clear of the configured `reword.timeout_ms` -- clear
/// of the deadline itself, since there is no ceiling to be clear of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed(pub String);

/// A [`Written`] or a [`Composed`], for the one function that has to tell
/// them apart.
///
/// A tuple struct around a **private** enum, not a public enum, and the
/// difference is the point: a public `Origin::Composed(text)` would be
/// spellable at every call site again, which is precisely the shape both
/// measured mutations exploited. The only ways to build one of these are the
/// two `From` impls below, and each of them needs a value whose *type* has
/// already fixed the answer.
pub struct Origin(Provenance);

enum Provenance {
    Written(String),
    Composed(String),
}

impl From<Written> for Origin {
    fn from(w: Written) -> Origin {
        Origin(Provenance::Written(w.0))
    }
}

impl From<Composed> for Origin {
    fn from(c: Composed) -> Origin {
        Origin(Provenance::Composed(c.0))
    }
}

impl Origin {
    /// The text itself, whichever it is.
    ///
    /// Reading the string says nothing about where it came from and cannot
    /// be used to build one, which is why this is safe to expose where a
    /// constructor is not. `notify::monitor::speak` needs it for its
    /// `max_chars` check, which happens before the rewrite so that an
    /// announcement the engine is going to refuse costs no round trip.
    pub fn text(&self) -> &str {
        match &self.0 {
            Provenance::Written(t) | Provenance::Composed(t) => t,
        }
    }

    /// The text itself, consumed.
    ///
    /// Safe to expose for the same reason [`Origin::text`] is: taking the
    /// string out says nothing about where it came from and cannot be used
    /// to build another `Origin`. `pipeline::prepare` needs it for the two
    /// asks that do not consult provenance -- `Ask::Never`, which speaks the
    /// text as written, and `Ask::Requested`, where the caller asking is the
    /// whole of the authority.
    pub fn into_text(self) -> String {
        match self.0 {
            Provenance::Written(t) | Provenance::Composed(t) => t,
        }
    }
}

/// A rewrite the breakers have already cleared, and the only way anything in
/// this daemon reaches [`reword_or_original`].
///
/// The breakers are *advisory*: neither [`attempt`] nor
/// [`reword_or_original`] consults [`RewordState::allow`], so a call site
/// that reached the rewrite without going through [`will_reword`] first
/// would silently bypass the auth latch, the transport breaker and the rate
/// limiter -- and would keep hammering a provider that has already answered
/// 401, once per submission. Nothing in the *signature* of
/// `reword_or_original` prevents that, so the protection is structural
/// instead: this type's fields are private, [`RewordPlan::admit`] is its only
/// constructor and calls `will_reword` on the way, [`RewordPlan::resolve`]
/// consumes `self`, and `will_reword`, `attempt` and `reword_or_original` are
/// all private to this module. A future edit cannot reorder or drop the check
/// without deleting the type that carries it.
///
/// `attempt` was `pub` until a probe measured what that cost: a caller in
/// `dbus.rs` calling `context(cfg)` and then `attempt(..)` compiled clean and
/// skipped the eligibility rule, the auth latch, the transport breaker and
/// the rate limiter -- exactly the bypass this doc claims is impossible. It
/// is private now, so the claim is true.
///
/// There is **one** deliberate exception, and it is a named function rather
/// than a loophole: `settings::model::run_reword_test`, the settings
/// window's Test button. It takes a permit and announces the endpoint like
/// any other send, but consults and updates no breaker, because a user who
/// has just fixed a rejected key must get a real request rather than a
/// cached verdict. Its own doc comment states that as a rule; nothing else
/// in the daemon may do it.
///
/// It lives here rather than in `notify::monitor`, where it was written,
/// because `dbus.rs` is a second call site and a guarantee that protects one
/// caller is not a guarantee. The two paths differ in exactly one respect --
/// whether `[reword] enabled` has a say -- and that difference is the two
/// constructors below rather than an `if` at either call site.
///
/// `will_reword` is called exactly *once* per submission, and then the
/// attempt is made. Past an expired transport cooldown `allow` mints and
/// spends §8's one half-open probe, so asking twice and attempting once
/// would burn a probe the run does not get back until the TTL expires.
///
/// `#[must_use]` for the other half of that: `resolve` consuming `self`
/// enforces *at most* one attempt per admission, and nothing enforced at
/// least one. A plan built and then dropped has already spent a half-open
/// probe token -- the run does not get that back until
/// [`transport_probe_ttl`] expires -- and, on the paths this daemon has, is
/// an announcement that was going to be reworded and now silently is not.
#[must_use = "admitting a plan has already spent §8's half-open probe token; \
              resolve it or the run pays for a rewrite that never happened"]
pub struct RewordPlan {
    rewriter: Arc<dyn Rewriter>,
    state: Arc<RewordState>,
    /// The exact text `will_reword` said yes to -- **after cleanup**.
    ///
    /// Owned by the plan rather than handed to `resolve` later, and this is
    /// the whole of the difference: `admit` judged *this* string against
    /// `max_chars` and the guard, so `resolve` must send *this* string.
    /// Measured on the previous shape -- `resolve(self, text: String)` taking
    /// any string at all -- a plan minted for a 40-character announcement
    /// could be handed 60 KB, and it compiled.
    ///
    /// Cleaned by [`RewordPlan::admit_with`] rather than by either call site,
    /// for the same reason it is owned here rather than passed later: what is
    /// judged, what is sent and what the guard measures against have to be
    /// one string, and a call site that cleans is a call site that can
    /// forget. See this module's doc for what was measured leaving the
    /// machine before it did.
    text: String,
    /// The text exactly as the source handed it over, before any cleanup.
    ///
    /// This is what [`Spoken`] carries back whenever the rewrite does not
    /// happen or does not survive -- never [`RewordPlan::text`]. Everything
    /// this plan hands back goes to `Engine::submit`, which cleans, and
    /// `clean` is **not** idempotent: cleaning a cleaned string can drop
    /// content (`sayd_core::cleanup`'s
    /// `clean_is_not_idempotent_and_callers_must_not_assume_it_is` has the
    /// measured case -- a markdown table whose leading `1.` becomes a list
    /// marker on the second pass and is eaten). Handing back the cleaned
    /// copy made a notification nobody reworded quieter than the same
    /// notification with the feature switched off.
    original: String,
    /// The exact config the decision was taken under, cloned once here
    /// rather than read again later: `notify::monitor` refreshes its cached
    /// `Config` on every tick, and text judged eligible under one
    /// `max_chars` must not then be sent under another. Owning it is also
    /// what lets a caller detach the resolve -- [`attempt`] borrows its
    /// config, and a detached task has nothing to borrow from.
    cfg: RewordConfig,
}

impl RewordPlan {
    /// The configuration's standing ask: `[reword] notifications = true`,
    /// which means "rewrite my notifications without being asked".
    ///
    /// Every step is synchronous and cheap -- a pass over a short string and
    /// one mutex -- so text that is not going to be rewritten costs no
    /// allocation, no clone of the config, and above all no `tokio::spawn`.
    /// That is what keeps the feature-off path exactly what it was: with
    /// `notifications = false` this returns on the second line, and in a build
    /// without the `reword` feature [`build_rewriter`] cannot make a client
    /// at all, so `context` returns `None` and `admit` returns on its first.
    ///
    /// Takes the text by value and hands it back in the `Err`, which is what
    /// makes "the plan owns what it admitted" free for the caller: every
    /// caller has a fallback that speaks the original, and giving the string
    /// back is cheaper than the clone the alternative would need.
    ///
    /// Takes an `impl Into<Origin>` rather than a text and a separate origin,
    /// so the caller never names one: it passes the [`Written`] or
    /// [`Composed`] it is holding and the type decides. See [`Written`] for
    /// the two mutations that shape exists to make unspellable.
    pub fn automatic(
        text: impl Into<Origin>,
        cfg: &RewordConfig,
        cleanup: &CleanupConfig,
    ) -> Result<RewordPlan, String> {
        let text = match text.into().0 {
            Provenance::Written(text) => text,
            // Matched rather than compared, so a third kind of text cannot
            // be added without this rule being re-decided for it.
            Provenance::Composed(text) => return Err(text),
        };
        if !cfg.notifications {
            return Err(text);
        }
        RewordPlan::admit(text, cfg, cleanup)
    }

    /// This caller's explicit ask: `say --reword`, or `reword` in the D-Bus
    /// `opts` map.
    ///
    /// Deliberately does **not** consult `notifications`. That switch means
    /// "rewrite my notifications without being asked", and `--reword` *is*
    /// being asked; refusing an explicit request because a different,
    /// automatic behaviour is switched off would be surprising. It does
    /// consult the `enabled` master, in [`RewordPlan::admit`] -- that one
    /// says whether rewording happens at all. Everything
    /// else -- a usable endpoint, the eligibility rule, all three breakers --
    /// applies identically, because `admit` below is shared.
    ///
    /// Takes a plain `String` rather than an [`Origin`]: an explicit request
    /// is by construction about text its caller wrote, and offering a
    /// composed one here would put back the wrong value that [`Written`]
    /// exists to make unspellable.
    ///
    /// MINOR 5: this is also the one path that owes a line when the build
    /// itself cannot rewrite. [`RewordState::record`] deliberately swallows
    /// [`RewordError::Unavailable`] -- right for the automatic path, which
    /// never asked for anything -- but `dbus.rs` promises an explicit
    /// `--reword` that "the diagnosis is in the log, once per run", and a
    /// feature-off daemon used to log nothing at all. The condition is
    /// exact rather than a guess at the reason: in a build without the
    /// feature `context` can never return a client, so it is the *first*
    /// thing `admit` fails on and `Unavailable` is the only reason there is.
    pub fn requested(
        text: String,
        cfg: &RewordConfig,
        cleanup: &CleanupConfig,
    ) -> Result<RewordPlan, String> {
        RewordPlan::requested_in(text, cfg, cleanup, &state())
    }

    /// [`RewordPlan::requested`] with its process-wide state handed in, so
    /// the once-per-run line above can be tested against a state no other
    /// test has already latched.
    fn requested_in(
        text: String,
        cfg: &RewordConfig,
        cleanup: &CleanupConfig,
        state: &RewordState,
    ) -> Result<RewordPlan, String> {
        let plan = RewordPlan::admit(text, cfg, cleanup);
        if plan.is_err() && !cfg!(feature = "reword") {
            state.note_unavailable();
        }
        plan
    }

    /// The shared body: resolve a client for `cfg`, then judge the text.
    ///
    /// `Err` is not a failure: it is "this text is not being reworded, here
    /// it is back", and every caller speaks it as written.
    ///
    /// The client is resolved *before* anything touches the text, so a build
    /// without the `reword` feature -- where `context` can never return one
    /// -- still pays nothing at all: no cleanup pass, no clone of the config,
    /// no mutex.
    fn admit(
        text: String,
        cfg: &RewordConfig,
        cleanup: &CleanupConfig,
    ) -> Result<RewordPlan, String> {
        // The master, and the first thing tested because it is the cheapest
        // and the broadest: with rewording switched off neither ask admits,
        // and `context` is not even consulted -- so no client is built and
        // no cached failure is recorded for a config nobody is using.
        if !cfg.enabled {
            return Err(text);
        }
        let Some((rewriter, state)) = context(cfg) else {
            return Err(text);
        };
        RewordPlan::admit_with(text, cfg, cleanup, rewriter, state)
    }

    /// [`RewordPlan::admit`] with its client handed in, and **the one place
    /// the text is cleaned**.
    ///
    /// Split out for exactly that: `admit` reaches `context`, a process-wide
    /// cache that builds a real HTTP client, so nothing that goes through it
    /// can be driven by a test double. Here the cleanup, the eligibility rule
    /// and the breakers can all be exercised against a `Rewriter` that
    /// records what it was handed -- which is what pins the module doc's
    /// promise that what leaves the machine is cleaned, in both builds.
    ///
    /// Order is load-bearing. Cleanup comes first, so [`will_reword`] judges
    /// the string that will actually be sent (§4) rather than one that may be
    /// twice as long before its code block is dropped, and so that CRITICAL
    /// 1 holds: the copy on the wire has had its fences and URLs removed.
    ///
    /// What goes back on refusal is the **original**, untouched. Every string
    /// this module hands back is submitted through `Engine::submit`, which
    /// cleans it -- so the cleaned copy must not escape, or it is cleaned
    /// twice. See [`RewordPlan::original`] for what that cost.
    fn admit_with(
        text: String,
        cfg: &RewordConfig,
        cleanup: &CleanupConfig,
        rewriter: Arc<dyn Rewriter>,
        state: Arc<RewordState>,
    ) -> Result<RewordPlan, String> {
        let cleaned = clean(&text, cleanup);
        if !will_reword(&cleaned, cfg, &state) {
            return Err(text);
        }
        Ok(RewordPlan {
            rewriter,
            state,
            text: cleaned,
            original: text,
            cfg: cfg.clone(),
        })
    }

    /// The text to speak. Consumes the plan, so the single `will_reword`
    /// that admitted it buys exactly one attempt -- of the exact string it
    /// admitted, which is why the plan carries it rather than taking one
    /// here.
    ///
    /// Holds no `EngineHandle` and returns a [`Spoken`]: the caller submits,
    /// so a rewrite that lands past the deadline is dropped rather than
    /// spoken second. Do not add a submit callback to this signature.
    pub async fn resolve(self) -> Spoken {
        reword_or_original(
            self.text,
            self.original,
            &self.cfg,
            self.rewriter,
            self.state,
        )
        .await
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

/// What the last attempt to build a client for a given config produced,
/// success *or* failure, and the exact config it was attempted for. Named
/// because the pair is what makes "attempted again only when the config
/// changes" checkable in one comparison.
///
/// `None` in the second slot -- a cached failure -- is the half that used to
/// be missing, and the case it costs is not exotic: `enabled = true` in a
/// build without the `reword` feature, where `build_rewriter` can never
/// succeed. Every announcement re-entered it and took two global mutexes
/// (this one, then `RewordState::inner` inside `record`), on the very
/// `select!` arm the `Journal` split above exists to keep clear.
type Cache = Mutex<Option<(RewordConfig, Option<Arc<dyn Rewriter>>)>>;

/// The rewriter for `cfg`, or `None` when this build cannot make one or the
/// configuration cannot be used.
///
/// Cached and rebuilt only when the config changes. The underlying `ureq`
/// agent is cached separately and outlives config changes entirely --
/// `base_url`, `model` and the key are per-request inputs, not client
/// state.
pub fn context(cfg: &RewordConfig) -> Option<(Arc<dyn Rewriter>, Arc<RewordState>)> {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    context_in(
        CACHE.get_or_init(|| Mutex::new(None)),
        state(),
        cfg,
        build_rewriter,
    )
}

/// The body of [`context`] with its two globals -- the cache and the
/// process-wide state -- and its builder handed in, so the caching rule can
/// be tested without a process-wide cache no test can reset and without a
/// client this build may not be able to make at all.
fn context_in(
    cache: &Cache,
    state: Arc<RewordState>,
    cfg: &RewordConfig,
    build: impl FnOnce(&RewordConfig) -> Result<Arc<dyn Rewriter>, RewordError>,
) -> Option<(Arc<dyn Rewriter>, Arc<RewordState>)> {
    let mut guard = lock(cache);
    if let Some((cached_cfg, outcome)) = guard.as_ref() {
        if cached_cfg == cfg {
            return outcome.clone().map(|rewriter| (rewriter, state));
        }
    }
    match build(cfg) {
        Ok(rewriter) => {
            *guard = Some((cfg.clone(), Some(rewriter.clone())));
            Some((rewriter, state))
        }
        Err(e) => {
            // Recorded once, with the failure remembered, so the row §8 owes
            // this config is still logged and still latches -- and the next
            // announcement under the same config is a comparison rather than
            // a rebuild and a second lock.
            *guard = Some((cfg.clone(), None));
            drop(guard);
            state.record(cfg, &Attempt::Answered(Err(e)), Instant::now());
            None
        }
    }
}

/// A loopback server that accepts a connection and then says nothing for
/// `hold`, then closes it. Returns a `base_url` pointing at it and the thread
/// serving it.
///
/// A *refused* connection would not do: it fails in well under a millisecond,
/// so both the detached notification path and the awaited D-Bus one would
/// return as fast against it whether or not they were doing the thing under
/// test. Measured: with the `tokio::spawn` in `notify::monitor::speak`
/// removed and `base_url` pointed at a closed port,
/// `speak_returns_at_once_when_a_rewrite_is_in_flight` still passed. Against
/// this, the same mutation takes the whole `timeout_ms`.
///
/// Non-blocking accept with a deadline so the thread always ends and can be
/// joined: joining it is what closes the socket, which is what lets the
/// runtime's own shutdown -- which waits on the blocking pool, where the
/// rewrite's `ureq` call lives -- finish promptly.
///
/// Here rather than in either caller's `mod tests` because both
/// `notify::monitor` and `dbus` need exactly this server, for the two halves
/// of the same rule: the notification path must not wait for a stuck
/// provider, and the D-Bus path must wait for it and still answer inside
/// the daemon's own configured `reword.timeout_ms` -- the only bound left on
/// a `--reword` call, since `sayd-cli` deliberately puts none of its own on
/// this path (see `sayd_cli`'s `TIMEOUT` doc comment).
#[cfg(test)]
pub fn silent_provider(hold: Duration) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    listener
        .set_nonblocking(true)
        .expect("non-blocking listener");
    let handle = std::thread::spawn(move || {
        let until = Instant::now() + hold;
        // Held, not dropped: a socket dropped straight away would let the
        // client's read fail at once, which is the refused-fast case again.
        let mut accepted = Vec::new();
        while Instant::now() < until {
            match listener.accept() {
                Ok((sock, _)) => accepted.push(sock),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), handle)
}

/// Build a client for `cfg`. The one function whose body differs between
/// the two builds.
#[cfg(feature = "reword")]
pub fn build_rewriter(cfg: &RewordConfig) -> Result<Arc<dyn Rewriter>, RewordError> {
    http::HttpRewriter::new(cfg).map(|r| Arc::new(r) as Arc<dyn Rewriter>)
}

#[cfg(not(feature = "reword"))]
pub fn build_rewriter(_cfg: &RewordConfig) -> Result<Arc<dyn Rewriter>, RewordError> {
    Err(RewordError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The invariant every other bound in this module now hangs off: **the
    /// transport's ceiling is strictly longer than the deadline the caller
    /// is timing against**, at every deadline a config can hold.
    ///
    /// It has to hold arithmetically rather than by measurement, because the
    /// deadline is unbounded: there is no largest case to test. The two ways
    /// it could fail are a ceiling written as a constant (fine at 1.5 s,
    /// silently truncating at 30) and an overflow at the top of the range
    /// turning a huge deadline into a tiny one, which is why `u64::MAX` is
    /// in the list -- `saturating_add` is what makes that case boring.
    #[test]
    fn the_client_ceiling_always_outlasts_the_deadline_it_is_derived_from() {
        for timeout_ms in [200, 1500, 10_000, 60_000, 3_600_000, u64::MAX] {
            let cfg = RewordConfig {
                timeout_ms,
                ..RewordConfig::default()
            };
            assert!(
                http_ceiling(&cfg) > Duration::from_millis(timeout_ms),
                "at timeout_ms = {timeout_ms} the transport would end the \
                 rewrite before the deadline did, which the user cannot see \
                 and cannot configure around"
            );
            assert_eq!(
                test_ceiling(&cfg),
                http_ceiling(&cfg),
                "and the Test row must report the ceiling that was really \
                 waited"
            );
        }
    }

    /// A test double: a queue of canned outcomes and, optionally, a sleep
    /// so a test can drive the deadline. No runtime, no futures, no
    /// associated types -- which is the whole reason the seam is a
    /// one-method synchronous trait we define ourselves.
    struct Stub {
        outcomes: Mutex<std::collections::VecDeque<Result<String, RewordError>>>,
        sleep: Duration,
        calls: AtomicUsize,
        /// Every string this was handed, in order. What a provider would
        /// have received, which is the only way to check CRITICAL 1 without
        /// a socket.
        seen: Mutex<Vec<String>>,
    }

    impl Stub {
        fn new(outcomes: Vec<Result<String, RewordError>>) -> Arc<Stub> {
            Arc::new(Stub {
                outcomes: Mutex::new(outcomes.into()),
                sleep: Duration::ZERO,
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn slow(sleep: Duration, outcomes: Vec<Result<String, RewordError>>) -> Arc<Stub> {
            Arc::new(Stub {
                outcomes: Mutex::new(outcomes.into()),
                sleep,
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    impl Rewriter for Stub {
        fn reword(&self, text: &str) -> Result<String, RewordError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            lock(&self.seen).push(text.to_string());
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
            notifications: true,
            timeout_ms: 100,
            provider: Some("generic".into()),
            ..RewordConfig::default()
        }
    }

    /// [`attempt`] on a task of its own. The future borrows its config, so
    /// a test that wants it detached hands it one to own.
    fn spawn_attempt(
        rewriter: Arc<dyn Rewriter>,
        state: Arc<RewordState>,
        text: String,
        budget: Duration,
    ) -> tokio::task::JoinHandle<(Attempt, Duration)> {
        tokio::spawn(async move { attempt(rewriter, state, &cfg(), text, budget).await })
    }

    /// Wait for every permit to come back, which is also the point at which
    /// every abandoned job has folded its own outcome into `record`: the
    /// permit is released after that call, not before.
    async fn settle(state: &RewordState) {
        for _ in 0..200 {
            if state.available_permits() == REWORD_MAX_INFLIGHT {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the rewrite jobs never finished");
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
            original.clone(),
            &cfg(),
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
        )
        .await;

        assert_eq!(
            spoken,
            Spoken::as_written(original.clone()),
            "past the deadline the original is what gets spoken, and it is \
             already the fallback -- there is nothing to fall back to"
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
            &cfg(),
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

        let a = spawn_attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            text.clone(),
            Duration::from_millis(900),
        );
        let b = spawn_attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            text.clone(),
            Duration::from_millis(900),
        );
        // Long enough for both permits to be taken, far short of the 400 ms
        // the stub sleeps for.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = std::time::Instant::now();
        let (third, _) = attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            &cfg(),
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

    /// The failure the transport breaker most exists for, in the shape the
    /// daemon actually meets it: a provider that accepts connections and
    /// never answers.
    ///
    /// The client's own ceiling is `REWORD_HTTP_GRACE` past `timeout_ms` and
    /// the caller gives up at `timeout_ms`, so every one of those attempts
    /// returns [`Attempt::Deadline`]
    /// to its caller and the [`RewordError::Ceiling`] arrives long after
    /// the caller is gone. With the outcome recorded only by the caller the
    /// breaker never moved -- measured, ten consecutive ceiling-class
    /// failures and `allow` still said `Ok(())`, so every eligible
    /// notification went on paying the full budget forever. The job folding
    /// its own outcome in is what makes §8's row reachable at all here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ceiling_failures_open_the_breaker_even_though_every_caller_saw_a_deadline() {
        let stub = Stub::slow(
            // Longer than the budget below, so the caller always gives up
            // first, exactly as it does against the real client ceiling --
            // which is that same budget plus the grace.
            Duration::from_millis(150),
            (0..TRANSPORT_FAILURES_TO_OPEN)
                .map(|_| Err(RewordError::Ceiling))
                .collect(),
        );
        let state = RewordState::new();
        let cfg = cfg();

        for i in 0..TRANSPORT_FAILURES_TO_OPEN {
            let (outcome, _) = attempt(
                stub.clone() as Arc<dyn Rewriter>,
                state.clone(),
                &cfg,
                "Alice: where do you want to go for dinner".into(),
                Duration::from_millis(20),
            )
            .await;
            assert!(
                matches!(outcome, Attempt::Deadline),
                "attempt {i} must have given up before the provider did: {outcome:?}"
            );
            settle(&state).await;
        }

        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            TRANSPORT_FAILURES_TO_OPEN as usize,
            "each arrival made exactly one request"
        );
        assert_eq!(
            state.allow(&cfg, Instant::now()),
            Err(Blocked::TransportOpen),
            "three ceiling-class failures opened the breaker even though \
             every caller was handed a Deadline and the answers themselves \
             were dropped unread"
        );
    }

    /// The `warning:`/`info:` line an outcome owes, or `None`. A `Debug`
    /// entry is not one: those are `SAYD_DEBUG`-only and no §8 row is
    /// satisfied by one.
    fn printed_line(journal: Option<Journal>) -> Option<String> {
        match journal {
            Some(Journal::Line(line)) => Some(line),
            Some(Journal::Debug(_)) | None => None,
        }
    }

    /// MINOR 2: §8 gives every failure a line, and a 429 had none.
    ///
    /// The window was set, the feature stopped rewriting for up to an hour,
    /// and the journal said nothing -- indistinguishable, to the user
    /// watching it stop, from the feature being switched off. The
    /// provider's own `error.message` had already been parsed and bounded
    /// by the client and was then dropped, which is the part that says
    /// *which* limit was hit.
    #[test]
    fn a_rate_limit_says_so_once_and_carries_the_providers_words() {
        let mut inner = Inner::default();
        let cfg = cfg();
        let now = Instant::now();
        let limited = |message: Option<&str>| {
            Attempt::Answered(Err(RewordError::RateLimited {
                retry_after: Some(Duration::from_secs(120)),
                message: message.map(str::to_string),
            }))
        };

        let line = printed_line(inner.fold(
            &cfg,
            &limited(Some("Rate limit reached for gpt-4o-mini")),
            now,
        ))
        .expect("§8 owes this row a line");
        assert!(
            line.starts_with("warning: reword: the provider is rate limiting (HTTP 429"),
            "{line}"
        );
        assert!(
            line.contains("Rate limit reached for gpt-4o-mini"),
            "the provider's own words are the useful half: {line}"
        );
        assert!(
            line.contains("120 s"),
            "and how long rewriting stops for: {line}"
        );

        assert_eq!(
            printed_line(inner.fold(&cfg, &limited(None), now)),
            None,
            "a standing rate limit is one line, not one per notification"
        );

        // A success is what clears it, the same event that clears a
        // standing outage: a limit that has lifted and come back is news.
        inner.fold(&cfg, &Attempt::Answered(Ok("a rewrite".into())), now);
        assert!(
            printed_line(inner.fold(&cfg, &limited(None), now)).is_some(),
            "a second, later rate limit is a second event"
        );
    }

    /// A1: `Truncated` did not gate its line the way every other row in
    /// §8's table does, and it is the one row nothing else throttles --
    /// deliberately not transport-class, so it never opens the breaker
    /// either. A reasoning model behind `provider = "generic"` truncates
    /// on nearly every request; an ungated line would be one `warning:`
    /// per notification, forever.
    ///
    /// A2: the advice used to say "set reword.provider" unconditionally.
    /// `HttpRewriter::new` refuses any config whose provider does not
    /// resolve before a request ever leaves the machine, so by the time
    /// this fires `provider` is already `generic` or `llama-cpp`, and the
    /// two need different advice -- see `Inner::fold`'s `Truncated` arm.
    #[test]
    fn a_truncation_says_so_once_per_run_and_names_the_real_fix() {
        let mut inner = Inner::default();
        let now = Instant::now();
        let truncated =
            |reasoning: bool| Attempt::Answered(Err(RewordError::Truncated { reasoning }));

        // `generic`, the default `cfg()` uses: it sends nothing to
        // suppress reasoning, so pointing at `llama-cpp` is real advice.
        let line = printed_line(inner.fold(&cfg(), &truncated(true), now))
            .expect("the first truncation is worth a line");
        assert!(line.contains("token cap"), "{line}");
        assert!(
            line.contains("reword.provider = \"llama-cpp\""),
            "generic sends nothing to suppress reasoning, so naming \
             llama-cpp is the real fix: {line}"
        );

        assert_eq!(
            printed_line(inner.fold(&cfg(), &truncated(true), now)),
            None,
            "a reasoning model that truncates on every request must not \
             log once per notification -- Truncated deliberately does not \
             open the transport breaker, so nothing else throttles it"
        );

        // A success is what clears the latch, like every other once-per-run
        // row.
        inner.fold(&cfg(), &Attempt::Answered(Ok("a rewrite".into())), now);
        assert!(
            printed_line(inner.fold(&cfg(), &truncated(false), now)).is_some(),
            "a second, later truncation is a second event"
        );

        // `llama-cpp` already sent `chat_template_kwargs`; telling the user
        // to set it again would be a circle, not advice.
        let mut llama = cfg();
        llama.provider = Some("llama-cpp".into());
        let mut inner_llama = Inner::default();
        let line = printed_line(inner_llama.fold(&llama, &truncated(true), now))
            .expect("the first truncation is worth a line");
        assert!(
            !line.contains("set reword.provider"),
            "provider is already llama-cpp, so this advice would be \
             circular: {line}"
        );
        assert!(
            line.contains("did not honour the request"),
            "the honest statement is that the server ignored what it was \
             told: {line}"
        );
    }

    /// MINOR 3: the auth line is owed per *config*, like the latch it
    /// belongs to.
    ///
    /// They used to disagree -- the breaker latch is keyed on the exact
    /// `RewordConfig`, the log latch was one global `bool` for the run --
    /// so a 403 from a second endpoint latched silently and left the user
    /// with a feature that had stopped and nothing saying why. The
    /// condition is now the latch itself, so the two cannot drift.
    #[test]
    fn a_rejected_key_on_a_second_endpoint_is_logged_as_well() {
        let mut inner = Inner::default();
        let now = Instant::now();
        let first = cfg();
        let mut second = cfg();
        second.base_url = "https://api.ppq.ai/v1".into();
        let rejected = |host: &str| {
            Attempt::Answered(Err(RewordError::Auth {
                status: 403,
                host: host.to_string(),
                message: None,
            }))
        };

        let line = printed_line(inner.fold(&first, &rejected("box.lan"), now))
            .expect("the first rejection is worth a line");
        assert!(line.contains("box.lan"), "{line}");
        assert_eq!(
            printed_line(inner.fold(&first, &rejected("box.lan"), now)),
            None,
            "the same config again is the same fact"
        );

        let line = printed_line(inner.fold(&second, &rejected("api.ppq.ai"), now))
            .expect("a different endpoint rejecting a different key is a different fact");
        assert!(line.contains("api.ppq.ai"), "{line}");

        // And a key that tested good and then failed is news again: the
        // latch is what carries the answer, so clearing it restores the
        // right to say so.
        inner.auth_latched_for = None;
        assert!(printed_line(inner.fold(&second, &rejected("api.ppq.ai"), now)).is_some());
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
        // ...and only for the config it was set for: a successful test of a
        // different endpoint is not evidence about this one.
        state.clear_auth_latch(&changed);
        assert_eq!(state.allow(&cfg, now), Err(Blocked::AuthLatched));
        state.clear_auth_latch(&cfg);
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

        // The client's own ceiling counts toward the same breaker. The
        // first of these is the probe just taken failing, which re-arms the
        // window on its own; the other two are the count starting again.
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

    /// A model that will not stop talking is not a broken transport. Three of
    /// these in a row must not take the feature offline for a minute the way
    /// three `Ceiling`s do -- the server answered every time, promptly and
    /// completely.
    #[test]
    fn truncation_does_not_open_the_transport_breaker() {
        let cfg = RewordConfig::default();
        let state = RewordState::new();
        let t0 = Instant::now();
        for _ in 0..3 {
            state.record(
                &cfg,
                &Attempt::Answered(Err(RewordError::Truncated { reasoning: true })),
                t0,
            );
        }
        assert_eq!(
            state.allow(&cfg, t0),
            Ok(()),
            "the provider answered three times; nothing about the transport is down"
        );
    }

    /// §8 says "stop attempting for 60 s, **then let one through**", and
    /// *one* is the whole of the sentence. Clearing the window on the first
    /// `allow` past it opens the gate for everyone until three fresh
    /// failures accumulate -- measured on the previous implementation, 100
    /// of 100 arrivals at the same instant past the cooldown were admitted,
    /// which against a provider that is still down is 100 requests, 100
    /// full `timeout_ms` delays, and both permits occupied.
    #[test]
    fn an_expired_cooldown_admits_one_probe_and_not_the_hundred_behind_it() {
        let state = RewordState::new();
        let cfg = cfg();
        let t0 = Instant::now();

        for _ in 0..TRANSPORT_FAILURES_TO_OPEN {
            state.record(
                &cfg,
                &Attempt::Answered(Err(RewordError::Unreachable("connection refused".into()))),
                t0,
            );
        }
        assert_eq!(state.allow(&cfg, t0), Err(Blocked::TransportOpen));

        let past = t0 + TRANSPORT_BREAKER_COOLDOWN + Duration::from_secs(1);
        assert_eq!(state.allow(&cfg, past), Ok(()), "one is let through");
        let admitted = (0..100).filter(|_| state.allow(&cfg, past).is_ok()).count();
        assert_eq!(
            admitted, 0,
            "and the hundred arrivals behind it are still refused: the \
             cooldown expiring mints one probe, it does not reopen the gate"
        );
    }

    /// The probe's two resolutions, which are what makes it a probe rather
    /// than a free pass: a failure buys another full cooldown, an answer
    /// closes the breaker for everyone.
    #[test]
    fn a_failed_probe_re_arms_the_cooldown_and_a_successful_one_closes_it() {
        let cfg = cfg();
        let t0 = Instant::now();
        let past = t0 + TRANSPORT_BREAKER_COOLDOWN + Duration::from_secs(1);

        let open = || {
            let state = RewordState::new();
            for _ in 0..TRANSPORT_FAILURES_TO_OPEN {
                state.record(
                    &cfg,
                    &Attempt::Answered(Err(RewordError::Unreachable("refused".into()))),
                    t0,
                );
            }
            assert_eq!(state.allow(&cfg, t0), Err(Blocked::TransportOpen));
            state
        };

        // The probe fails: another 60 s, not three more chances.
        let state = open();
        assert_eq!(state.allow(&cfg, past), Ok(()));
        state.record(
            &cfg,
            &Attempt::Answered(Err(RewordError::Unreachable("refused".into()))),
            past,
        );
        assert_eq!(
            state.allow(&cfg, past + Duration::from_secs(59)),
            Err(Blocked::TransportOpen),
            "one failed probe re-arms the whole cooldown by itself"
        );
        assert_eq!(
            state.allow(
                &cfg,
                past + TRANSPORT_BREAKER_COOLDOWN + Duration::from_secs(1)
            ),
            Ok(()),
            "and then another probe, indefinitely, for as long as it keeps failing"
        );

        // The probe succeeds: the breaker is closed for everyone, not just
        // for the caller that happened to hold the token.
        let state = open();
        assert_eq!(state.allow(&cfg, past), Ok(()));
        state.record(&cfg, &Attempt::Answered(Ok("a rewrite".into())), past);
        for _ in 0..10 {
            assert_eq!(
                state.allow(&cfg, past),
                Ok(()),
                "a provider that came back is not rationed"
            );
        }
    }

    /// A probe nobody resolves must not shut the feature off for the life
    /// of the process. Every way of taking one without resolving it is a
    /// caller that passed `allow` and then did not attempt -- no permit was
    /// free, or `context` could not build a client -- and the second of
    /// those does not even reach a `record`. So the token expires.
    #[test]
    fn an_abandoned_probe_expires_rather_than_latching_the_breaker_shut() {
        let state = RewordState::new();
        let cfg = cfg();
        let t0 = Instant::now();
        for _ in 0..TRANSPORT_FAILURES_TO_OPEN {
            state.record(
                &cfg,
                &Attempt::Answered(Err(RewordError::Unreachable("refused".into()))),
                t0,
            );
        }
        let past = t0 + TRANSPORT_BREAKER_COOLDOWN + Duration::from_secs(1);
        assert_eq!(state.allow(&cfg, past), Ok(()));
        // ...and this caller never attempts. Nothing resolves the token.
        assert_eq!(state.allow(&cfg, past), Err(Blocked::TransportOpen));
        assert_eq!(
            state.allow(
                &cfg,
                past + transport_probe_ttl(&cfg) + Duration::from_secs(1)
            ),
            Ok(()),
            "past the ceiling a real attempt could have taken, the probe \
             cannot still be in flight, so the next arrival may take one"
        );

        // A caller that found no permit hands the token straight back
        // rather than sitting on it for the ceiling.
        let state = RewordState::new();
        for _ in 0..TRANSPORT_FAILURES_TO_OPEN {
            state.record(
                &cfg,
                &Attempt::Answered(Err(RewordError::Unreachable("refused".into()))),
                t0,
            );
        }
        assert_eq!(state.allow(&cfg, past), Ok(()));
        state.record(&cfg, &Attempt::Busy, past);
        assert_eq!(
            state.allow(&cfg, past),
            Ok(()),
            "nothing was sent, so nothing was learned and the probe is \
             still there to be taken"
        );
    }

    /// The concurrent shape of the same rule, on real threads rather than
    /// on an argument about the lock: eight callers arrive at the same
    /// instant past an expired cooldown and exactly one is admitted. The
    /// previous implementation admitted all eight.
    #[test]
    fn concurrent_callers_past_an_expired_window_admit_exactly_one() {
        let state = RewordState::new();
        let cfg = cfg();
        let t0 = Instant::now();
        for _ in 0..TRANSPORT_FAILURES_TO_OPEN {
            state.record(
                &cfg,
                &Attempt::Answered(Err(RewordError::Unreachable("refused".into()))),
                t0,
            );
        }
        let past = t0 + TRANSPORT_BREAKER_COOLDOWN + Duration::from_secs(1);

        let admitted = AtomicUsize::new(0);
        let ready = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    ready.wait();
                    if state.allow(&cfg, past).is_ok() {
                        admitted.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(
            admitted.load(Ordering::SeqCst),
            1,
            "the probe is a token, and a token is taken by exactly one caller"
        );
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

    /// A later, shorter `Retry-After` must not cancel a longer one that is
    /// still in force. Two rewrites may be in flight, so two 429s in a row
    /// is ordinary rather than exotic -- and a provider that answers the
    /// second with `Retry-After: 1` has not withdrawn the hour it asked for
    /// on the first. Measured on the previous implementation:
    /// `record(3600 s)` then `record(1 s)`, and the next attempt went out
    /// two seconds later.
    #[test]
    fn a_second_retry_after_extends_the_backoff_and_never_shortens_it() {
        let state = RewordState::new();
        let cfg = cfg();
        let t0 = Instant::now();
        let limited = |after: u64| {
            Attempt::Answered(Err(RewordError::RateLimited {
                retry_after: Some(Duration::from_secs(after)),
                message: None,
            }))
        };

        state.record(&cfg, &limited(3600), t0);
        state.record(&cfg, &limited(1), t0);
        assert_eq!(
            state.allow(&cfg, t0 + Duration::from_secs(2)),
            Err(Blocked::RateLimited),
            "the shorter of the two did not replace the hour"
        );
        assert_eq!(
            state.allow(&cfg, t0 + Duration::from_secs(3599)),
            Err(Blocked::RateLimited)
        );
        assert_eq!(state.allow(&cfg, t0 + Duration::from_secs(3601)), Ok(()));

        // And the other order still extends: a longer second window moves
        // the deadline out.
        let state = RewordState::new();
        state.record(&cfg, &limited(5), t0);
        state.record(&cfg, &limited(600), t0);
        assert_eq!(
            state.allow(&cfg, t0 + Duration::from_secs(30)),
            Err(Blocked::RateLimited)
        );
        assert_eq!(state.allow(&cfg, t0 + Duration::from_secs(601)), Ok(()));
    }

    /// Both breaker windows are dated by `Instant::checked_add`, and its
    /// `None` used to be stored as "no window at all" -- so the arithmetic
    /// that overflowed *because* the wait was absurd produced no wait.
    /// `notify::policy::is_expired` makes the opposite call for the same
    /// arithmetic and says why; this is that call, made in the type.
    #[test]
    fn a_window_that_cannot_be_dated_is_shut_rather_than_open() {
        let now = Instant::now();
        assert_eq!(Window::opening(now, Duration::MAX), Window::Forever);
        assert!(Window::Forever.shut_at(now));
        assert!(Window::Forever.shut_at(now + Duration::from_secs(86_400)));
        assert_eq!(
            Window::Forever.later(Window::Until(now)),
            Window::Forever,
            "and it cannot be shortened by a window that can be dated"
        );
        assert!(
            !Window::opening(now, Duration::from_secs(60)).shut_at(now + Duration::from_secs(61))
        );
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

    /// CRITICAL 1: what is sent is the *cleaned* text, and the guard measures
    /// that same string.
    ///
    /// The reviewer's own reproduction, cut down to the parts that matter:
    /// one announcement carrying a URL with a reset token in its query, a
    /// fenced shell secret, markdown emphasis and an ANSI escape. Measured
    /// against a fake provider before this was fixed, every byte of it went
    /// out verbatim; the README told the user the opposite. Every assertion
    /// below fails if the `clean` in `admit_with` is deleted.
    ///
    /// Not gated on the `reword` feature: the plan, the cleanup and the guard
    /// are all in the default build, and only the client that would carry the
    /// string to a socket is not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn what_leaves_the_machine_is_cleaned_before_it_leaves() {
        let raw = "Alice sent a link. reset here \
                   https://example.com/reset?token=SECRET123 \
                   ```\nexport AWS_SECRET=hunter2\n``` **bold** _em_\u{1b}[31mRED\u{1b}[0m";
        let stub = Stub::new(vec![Ok("Alice sent a link about a reset".into())]);
        let state = RewordState::new();

        let plan = RewordPlan::admit_with(
            raw.to_string(),
            &cfg(),
            &CleanupConfig::default(),
            stub.clone() as Arc<dyn Rewriter>,
            state,
        )
        .expect("the announcement is eligible");
        let spoken = plan.resolve().await;

        let sent = lock(&stub.seen)[0].clone();
        assert!(
            !sent.contains("SECRET123"),
            "a token in a URL must not reach the provider: {sent:?}"
        );
        assert!(
            !sent.contains("AWS_SECRET"),
            "a fenced block is dropped whole, contents and all: {sent:?}"
        );
        assert!(
            !sent.contains("```"),
            "and so is the fence itself: {sent:?}"
        );
        assert!(
            !sent.contains("**") && !sent.contains('_'),
            "markdown emphasis is stripped: {sent:?}"
        );
        assert!(
            !sent.contains('\u{1b}'),
            "and an ANSI escape cannot travel: {sent:?}"
        );
        assert!(
            sent.contains("link"),
            "the URL is reduced to the word `link`, not deleted: {sent:?}"
        );

        // What the fallback carries is the **raw** text, and that is not a
        // hole in any of the above: it never reaches a socket, only
        // `Engine::submit`, which cleans it -- to exactly the string that
        // went on the wire. Asserting that equality rather than asserting
        // the fallback is already cleaned is what keeps the two halves of
        // this from drifting apart, and it is the stronger claim: it says
        // the user hears precisely what the provider was shown.
        let fallback = spoken.fallback.expect("a rewrite carries one");
        assert_eq!(
            clean(&fallback, &CleanupConfig::default()),
            sent,
            "the fallback cleans to the string that was sent"
        );
    }

    /// The refusal path hands back **what it was given**, not a cleaned copy.
    ///
    /// `Engine::submit` cleans whatever it is handed, and `clean` is not
    /// idempotent -- see
    /// `sayd_core::cleanup::tests::clean_is_not_idempotent_and_callers_must_not_assume_it_is`
    /// for the measured counterexample. A cleaned string returned here is
    /// therefore cleaned a *second* time downstream, and a notification that
    /// nobody reworded is spoken with less in it than the same notification
    /// with rewording switched off entirely.
    #[test]
    fn a_refused_plan_hands_back_the_original_not_a_cleaned_copy() {
        const TABLE: &str = "| 1. | 2,5 EUR |\n| 3. | 1.234 |";
        let mut c = cfg();
        c.max_chars = 4; // nothing is eligible at this ceiling
        let back = RewordPlan::admit_with(
            TABLE.to_string(),
            &c,
            &CleanupConfig::default(),
            Stub::new(vec![]) as Arc<dyn Rewriter>,
            RewordState::new(),
        )
        .err()
        .expect("over the ceiling, so no plan");
        assert_eq!(
            back, TABLE,
            "the refusal path must hand back what it was given"
        );
    }

    /// Whatever happens to a rewrite, the string that reaches the engine
    /// cleans to the same thing the rewording-off path speaks.
    ///
    /// The property the double-clean broke, stated over the three ways a
    /// submission can end up speaking its original: never admitted, admitted
    /// and refused by the guard, and rewritten but then refused by the
    /// engine. All three are byte-identical to switching the feature off.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_path_back_to_the_original_matches_the_rewording_off_path() {
        const TABLE: &str = "| 1. | 2,5 EUR |\n| 3. | 1.234 |";
        let cleanup = CleanupConfig::default();
        let spoken_when_off = clean(TABLE, &cleanup);

        // 1. The reference: rewording off, so `automatic` returns on its
        //    second line and nothing has touched the text.
        let mut off = cfg();
        off.enabled = false;
        let back = RewordPlan::automatic(Written(TABLE.into()), &off, &cleanup)
            .err()
            .expect("off, so no plan");
        assert_eq!(clean(&back, &cleanup), spoken_when_off, "rewording off");

        // 2. Admitted nowhere: `will_reword` refuses this text.
        let mut small = cfg();
        small.max_chars = 4;
        let back = RewordPlan::admit_with(
            TABLE.to_string(),
            &small,
            &cleanup,
            Stub::new(vec![]) as Arc<dyn Rewriter>,
            RewordState::new(),
        )
        .err()
        .expect("over the ceiling");
        assert_eq!(
            clean(&back, &cleanup),
            spoken_when_off,
            "refused by the guard"
        );

        // 3. Rewritten, and then refused by the engine: the fallback is what
        //    gets submitted, and it goes through `Engine::submit` too.
        let stub = Stub::new(vec![Ok("Two rows of numbers.".into())]);
        let spoken = RewordPlan::admit_with(
            TABLE.to_string(),
            &cfg(),
            &cleanup,
            stub as Arc<dyn Rewriter>,
            RewordState::new(),
        )
        .expect("eligible")
        .resolve()
        .await;
        let fallback = spoken.fallback.expect("a rewrite carries one");
        assert_eq!(
            clean(&fallback, &cleanup),
            spoken_when_off,
            "the engine refused the rewrite"
        );
    }

    /// The other half of CRITICAL 1: cleaning happens *before* the
    /// eligibility rule, so §4's ceiling measures the string that gets spoken.
    ///
    /// A fenced build log is the case. Raw it is over `reword.max_chars` and
    /// was refused as `TooLong`; cleaned it is a short sentence -- and a short
    /// sentence is exactly what this feature exists to improve.
    #[test]
    fn eligibility_is_decided_on_the_cleaned_text_not_the_raw_one() {
        let raw = format!(
            "The build failed with this error\n```\n{}\n```",
            "x".repeat(600)
        );
        let mut c = cfg();
        c.max_chars = 400;
        assert!(
            eligible(&raw, c.max_chars).is_err(),
            "raw, this is over the ceiling"
        );

        let stub = Stub::new(vec![Ok("The build failed.".into())]);
        assert!(
            RewordPlan::admit_with(
                raw,
                &c,
                &CleanupConfig::default(),
                stub as Arc<dyn Rewriter>,
                RewordState::new(),
            )
            .is_ok(),
            "cleaned, it is a short sentence and is worth rewriting"
        );
    }

    /// The two asks, and the one difference between them: `enabled` governs
    /// the automatic rewrite and nothing else. "Rewrite my notifications
    /// without being asked" is what that switch says, and `--reword` is
    /// being asked -- refusing an explicit request because a different,
    /// automatic behaviour is off would be surprising. Everything else is
    /// the same code, because both constructors share `admit` -- including
    /// the `enabled` master, which refuses both and is checked at the end.
    ///
    /// `is_ok()` is compared against `cfg!(feature = "reword")` rather than
    /// asserted outright: without the feature there is no client to build, so
    /// `context` returns `None` and *nothing* is ever admitted -- which is
    /// the compiler's half of the promise and is worth pinning here too.
    ///
    /// The `Err` is the text itself, handed back for the caller to speak as
    /// written, so each rejection is checked to be that text and not some
    /// other string.
    #[test]
    fn only_the_automatic_ask_consults_notifications() {
        let text = "Alice: where do you want to go for dinner";
        let mut off = cfg();
        off.notifications = false;

        assert_eq!(
            RewordPlan::automatic(Written(text.into()), &off, &CleanupConfig::default()).err(),
            Some(text.to_string()),
            "`notifications = false` must not even look for a client, and the \
             text comes straight back"
        );
        assert_eq!(
            RewordPlan::requested(text.into(), &off, &CleanupConfig::default()).is_ok(),
            cfg!(feature = "reword"),
            "an explicit --reword does not need `notifications`"
        );
        assert_eq!(
            RewordPlan::automatic(Written(text.into()), &cfg(), &CleanupConfig::default()).is_ok(),
            cfg!(feature = "reword")
        );

        // The master is the other switch, and it is not this one: it refuses
        // both asks. Without this the pair above would pass just as well if
        // `notifications` had been left as the only switch there is.
        let mut master_off = cfg();
        master_off.enabled = false;
        assert_eq!(
            RewordPlan::automatic(Written(text.into()), &master_off, &CleanupConfig::default())
                .err(),
            Some(text.to_string()),
        );
        assert_eq!(
            RewordPlan::requested(text.into(), &master_off, &CleanupConfig::default()).err(),
            Some(text.to_string()),
            "rewording off means off, however explicitly it is asked for"
        );
    }

    /// The rule the enum exists to carry: text this daemon composed itself
    /// is never reworded, whichever ask is being made and whatever `enabled`
    /// says. As a `bool` parameter this was flippable at its call site --
    /// measured: `true` compiled and passed the whole suite, while sending a
    /// templated line to a provider and delaying it by up to `timeout_ms`.
    /// See [`Composed`] for why that is wrong and for the ordering
    /// hazard it is *not*.
    #[test]
    fn composed_text_is_never_admitted() {
        let followup = "Signal: 3 more notifications";
        assert_eq!(
            RewordPlan::automatic(Composed(followup.into()), &cfg(), &CleanupConfig::default())
                .err(),
            Some(followup.to_string()),
            "a follow-up is refused, and gets its own text back to speak"
        );
        // ...and not because it was ineligible on its own account: the same
        // string with the other origin is admitted wherever there is a client.
        assert_eq!(
            RewordPlan::automatic(Written(followup.into()), &cfg(), &CleanupConfig::default())
                .is_ok(),
            cfg!(feature = "reword"),
            "the exclusion must be the origin, not the length"
        );
    }

    /// MINOR 5: an explicit `--reword` that this *build* cannot honour says
    /// so, once per run.
    ///
    /// `dbus.rs` promises every `--reword` failure that "the diagnosis is in
    /// the log, once per run", and a feature-off daemon used to print nothing
    /// at all: `RewordState::record` swallows `RewordError::Unavailable`,
    /// which is right for the automatic path (it would owe a line per
    /// notification for a rewrite nobody asked for) and wrong for the path
    /// that asked.
    ///
    /// Both directions are asserted, and the `cfg!` is what makes this one
    /// test rather than two: a build *with* a client must not print the line,
    /// because in that build `Unavailable` is not the reason for anything.
    #[test]
    fn an_explicit_reword_says_so_when_the_build_cannot_rewrite() {
        let state = RewordState::new();
        let text = "Alice: where do you want to go for dinner";
        let plan = RewordPlan::requested_in(text.into(), &cfg(), &CleanupConfig::default(), &state);
        assert_eq!(plan.is_ok(), cfg!(feature = "reword"));
        assert_eq!(
            state.unavailable_logged(),
            !cfg!(feature = "reword"),
            "a build with no rewriting client in it owes an explicit --reword a \
             line; one that has a client owes it nothing"
        );

        // ...and only ever one line, whichever build this is: the latch is
        // per run, not per `--reword`.
        let fresh = RewordState::new();
        assert!(fresh.note_unavailable(), "the first ask is worth a line");
        assert!(!fresh.note_unavailable(), "and the thousandth is not");
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
                original.clone(),
                &cfg(),
                good as Arc<dyn Rewriter>,
                state.clone()
            )
            .await,
            Spoken {
                text: "Alice is asking where you want to go for dinner".into(),
                fallback: Some(original.clone()),
            },
            "an accepted rewrite carries the text it replaced, so a submission \
             the engine refuses is not the end of the announcement"
        );

        let chatty = Stub::new(vec![Ok("Sure!\nHere you go:\nAlice is asking.".into())]);
        assert_eq!(
            reword_or_original(
                original.clone(),
                original.clone(),
                &cfg(),
                chatty as Arc<dyn Rewriter>,
                RewordState::new()
            )
            .await,
            Spoken::as_written(original.clone()),
            "a model that explained itself gets the original spoken instead"
        );

        let dead = Stub::new(vec![Err(RewordError::Unreachable("refused".into()))]);
        assert_eq!(
            reword_or_original(
                original.clone(),
                original.clone(),
                &cfg(),
                dead as Arc<dyn Rewriter>,
                RewordState::new()
            )
            .await,
            Spoken::as_written(original.clone())
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

    /// IMPORTANT 1: nothing under `RewordState::inner` may perform I/O.
    ///
    /// `notify::monitor`'s `tokio::select!` arm reaches
    /// [`RewordState::allow`] -- and this mutex -- on every notification,
    /// while [`RewordState::record`] runs on a blocking-pool thread inside
    /// the rewrite job. An `eprintln!` into a pipe nobody is draining (a
    /// stalled journald, a terminal whose reader has stopped) blocks in
    /// `write(2)`, and held across the lock it holds the arm with it: the
    /// `MessageStream` stops being polled and `ticker.tick()` stops firing
    /// for as long as the write is stuck. Measured on the previous shape
    /// with a 2000 ms stall standing in for the blocked write, the arm was
    /// held for 1.70 s.
    ///
    /// The stall is a per-instance field, so this test slows nothing else in
    /// the suite down, and it is `#[cfg(test)]`, so it is not in the shipped
    /// binary at all.
    #[test]
    fn a_stalled_journal_never_holds_the_lock_the_monitors_arm_needs() {
        const STALL: Duration = Duration::from_millis(500);
        let cfg = cfg();

        // One case per *kind* of printing branch, reached the way the daemon
        // reaches it: `record`'s six lines share one body and one `drop`, and
        // the two `note_` methods have bodies of their own.
        /// One printing branch: what to call it, and how the daemon gets
        /// there.
        type Case = (&'static str, fn(&RewordState, &RewordConfig));

        let cases: [Case; 4] = [
            ("record: a missed deadline", |s, cfg| {
                s.record(cfg, &Attempt::Deadline, Instant::now())
            }),
            ("record: an unreachable provider", |s, cfg| {
                s.record(
                    cfg,
                    &Attempt::Answered(Err(RewordError::Unreachable("refused".into()))),
                    Instant::now(),
                )
            }),
            ("note_endpoint", |s, cfg| {
                s.note_endpoint(cfg);
            }),
            ("note_ineligible", |s, _cfg| {
                s.note_ineligible(Ineligible::TooLong)
            }),
        ];

        for (what, printing) in cases {
            let state = RewordState::with_stalled_journal(STALL);
            let printer = {
                let state = state.clone();
                let cfg = cfg.clone();
                std::thread::spawn(move || printing(&state, &cfg))
            };
            // Long enough for the printer to be inside the stalled write,
            // far short of the stall itself.
            std::thread::sleep(STALL / 5);

            let started = Instant::now();
            let _ = state.allow(&cfg, Instant::now());
            let waited = started.elapsed();
            printer.join().expect("the printing thread must not panic");

            assert!(
                waited < STALL / 5,
                "{what}: the monitor's arm waited {waited:?} for a journal write \
                 that had not finished, so the lock was held across the print"
            );
        }
    }

    /// §7's privacy line is what a user greps to find out where their text
    /// goes, so it must not name a destination for a run in which nothing
    /// left the machine. An arrival that finds no permit makes no request
    /// at all -- and used to announce one anyway, because the line was
    /// printed before the permit was asked for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nothing_is_announced_for_an_utterance_that_never_sent_anything() {
        let stub = Stub::slow(
            Duration::from_millis(400),
            vec![Ok("a".into()), Ok("b".into())],
        );
        let state = RewordState::new();
        let cfg = cfg();
        let text = "Alice: where do you want to go for dinner".to_string();

        // Both permits taken by rewrites that are still in flight.
        let a = spawn_attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            text.clone(),
            Duration::from_millis(900),
        );
        let b = spawn_attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            text.clone(),
            Duration::from_millis(900),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;

        // A third arrival, against an endpoint nothing has announced yet.
        let mut unannounced = cfg.clone();
        unannounced.base_url = "https://api.ppq.ai/v1".into();
        let (third, _) = attempt(
            stub.clone() as Arc<dyn Rewriter>,
            state.clone(),
            &unannounced,
            text,
            Duration::from_millis(900),
        )
        .await;
        assert!(matches!(third, Attempt::Busy));
        assert!(
            !state.endpoint_seen(&unannounced),
            "no permit, no request, no line claiming the text was sent"
        );

        let _ = a.await;
        let _ = b.await;
        // ...and the endpoint the two that *did* run were pointed at is
        // announced, so the line has not simply gone away.
        assert!(state.endpoint_seen(&cfg));
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
                spawned.push(spawn_attempt(
                    stub.clone() as Arc<dyn Rewriter>,
                    state.clone(),
                    text.clone(),
                    Duration::from_secs(5),
                ));
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
    /// stuck. The original incident was counted in threads -- 30 to 548 in
    /// three and a half minutes, then tokio's 512-thread cap, then `Say`
    /// over D-Bus never returning -- but a process-wide thread count is not
    /// a quantity this test can own: this suite runs its tests in parallel
    /// in one process, and engine threads, dbus integration tests, the
    /// settings writer and tokio's own pools all move `/proc/self/task`
    /// underneath it, which is what made this test fail two runs in three
    /// in the full workspace suite despite the property it guards being
    /// intact. `two_rewrites_run_at_once_and_never_three` already solved
    /// this the right way for the concurrent-burst case: watch the
    /// rewriter's own in-flight count instead of the process's thread
    /// count. That is not a duplicate of this test -- that one bursts four
    /// requests at a provider that eventually answers and checks that
    /// permits are reused wave over wave; this one is the pathological case
    /// Task 3 exists for, 60 arrivals in a row against a provider that
    /// *never* answers inside its deadline, and it is pinning a different
    /// half of the same guarantee: that arrivals past the permit count are
    /// refused outright rather than each costing a fresh blocking job.
    ///
    /// With the permit released at the deadline instead of held, every one
    /// of these 60 arrivals would get a permit and a fresh blocking job
    /// while the previous ones were still parked, so both the peak in-flight
    /// count and the call count would climb far past
    /// [`REWORD_MAX_INFLIGHT`] instead of stopping at it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stuck_rewriter_never_exceeds_its_permit_count_in_flight() {
        let stub = Counted::new(Duration::from_millis(500));
        let state = RewordState::new();

        for _ in 0..60 {
            let _ = attempt(
                stub.clone() as Arc<dyn Rewriter>,
                state.clone(),
                &cfg(),
                "Alice: where do you want to go for dinner".into(),
                Duration::from_millis(10),
            )
            .await;
        }

        assert!(
            stub.peak.load(Ordering::SeqCst) <= REWORD_MAX_INFLIGHT,
            "at most {REWORD_MAX_INFLIGHT} rewrites were ever in flight against a \
             stuck provider, no matter how many arrivals came after them; saw {}",
            stub.peak.load(Ordering::SeqCst)
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
            original.clone(),
            &cfg(),
            rewriter as Arc<dyn Rewriter>,
            RewordState::new(),
        )
        .await;
        assert_eq!(spoken, Spoken::as_written(original.clone()));

        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            lock(&produced).len(),
            1,
            "the job ran to completion: it was abandoned, not cancelled"
        );
        assert_ne!(
            spoken.text,
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
            original.clone(),
            &cfg(),
            Arc::new(Panicky) as Arc<dyn Rewriter>,
            state.clone(),
        )
        .await;
        assert_eq!(spoken, Spoken::as_written(original.clone()));
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
        let past = t0 + TRANSPORT_BREAKER_COOLDOWN + Duration::from_secs(1);
        assert_eq!(state.allow(&cfg, past), Ok(()));
        // That was the probe, and the probe is one: a real-clock caller
        // behind it is still refused until the probe comes back.
        assert!(!will_reword(text, &cfg, &state));
        state.record(&cfg, &Attempt::Answered(Ok("a rewrite".into())), past);
        assert!(
            will_reword(text, &cfg, &state),
            "and the breaker the injected clock closed is closed for the \
             real clock too"
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

    /// A build *failure* is remembered exactly like a success. It used not to
    /// be, and the case that costs is not exotic: `enabled = true` in a build
    /// without the `reword` feature, where `build_rewriter` can never
    /// succeed. Every announcement then re-entered it and took two global
    /// mutexes -- the cache, then `RewordState::inner` inside `record` --
    /// on the `select!` arm `Journal` exists to keep clear.
    #[test]
    fn a_build_failure_is_remembered_as_firmly_as_a_client() {
        let cache: Cache = Mutex::new(None);
        let state = RewordState::new();
        let cfg = cfg();
        let builds = AtomicUsize::new(0);

        let refuses = |_: &RewordConfig| {
            builds.fetch_add(1, Ordering::SeqCst);
            Err(RewordError::NotConfigured("base_url is empty".into()))
        };
        for _ in 0..5 {
            assert!(context_in(&cache, state.clone(), &cfg, refuses).is_none());
        }
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "a configuration that could not produce a client is asked once, \
             not once per announcement"
        );

        // ...and a changed config is a fresh question, exactly as it is for a
        // client that did build.
        let mut other = cfg.clone();
        other.base_url = "http://localhost:1234/v1".into();
        assert!(context_in(&cache, state.clone(), &other, refuses).is_none());
        assert_eq!(builds.load(Ordering::SeqCst), 2);

        // The success half of the same rule, so the cache cannot be "fixed"
        // by never caching anything.
        let cache: Cache = Mutex::new(None);
        let builds = AtomicUsize::new(0);
        let succeeds = |_: &RewordConfig| {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(Stub::new(vec![Ok("a rewrite".into())]) as Arc<dyn Rewriter>)
        };
        for _ in 0..5 {
            assert!(context_in(&cache, state.clone(), &cfg, succeeds).is_some());
        }
        assert_eq!(builds.load(Ordering::SeqCst), 1);
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
