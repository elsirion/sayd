//! Runs the engine on its own thread and gives everyone else a handle to it.
//!
//! The engine is deliberately single-threaded: it owns the queue, the
//! synthesizer and the sink outright, and `tick()` does one unit of work and
//! returns. So it moves wholesale onto one thread, and callers send commands
//! in and read published snapshots out. Nothing outside this module ever
//! holds a reference to the `Engine` itself.
//!
//! Snapshots are published into a mutex after every tick. Readers take that
//! lock for the length of a clone and never while the engine is working, so
//! a D-Bus poller cannot stall synthesis.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::audio::AudioSink;
use crate::config::Config;
use crate::engine::{Command, Engine, SayOpts, Snapshot, Submitted};
use crate::synth::Synthesizer;

/// How long the engine thread waits for a command before ticking anyway.
///
/// `tick` must be called even when no command arrives: it is what advances
/// synthesis, drains the sink, and runs the idle-unload check.
const TICK_INTERVAL: Duration = Duration::from_millis(10);

/// How long `submit` waits for the engine's synchronous answer before
/// falling back to `Ok(Submitted::TimedOut)` instead of continuing to block.
///
/// `Engine::submit` itself is cheap -- it cleans the text, applies policy
/// and queues -- and `run`'s loop answers it (and publishes the resulting
/// snapshot) before starting the next `tick()`, not after. The only way
/// this wait runs long is if the run loop was already mid-`tick()` --
/// synthesizing a whole chunk, measured 3.5-7.5s on real ONNX -- when the
/// message arrived: that single call cannot be interrupted, so the message
/// simply waits in the channel until it returns. This bound keeps a caller
/// from waiting out the rest of a chunk that has nothing to do with its own
/// request. In particular, the `sayd` binary's async D-Bus handler calls
/// this from inside `tokio::task::spawn_blocking`, precisely so a long wait
/// here blocks a blocking-pool thread instead of starving the async
/// runtime's worker threads (which would otherwise stall unrelated,
/// non-blocking calls like `Stop` too) -- but this bound is also what keeps
/// the wait itself well under the CLI's own 3s call timeout.
///
/// Returning `Err` on timeout would recreate exactly the bug this exists to
/// fix: the message was already handed to the engine successfully (`send`
/// below returned `Ok`), so the text *is* queued (or otherwise handled)
/// regardless of whether this wait times out. An `Err` would tell the
/// caller otherwise, inviting a retry that double-queues.
///
/// Finding 3: this used to collapse into the very same `Ok(None)` that
/// means "accepted, no id" for a muted/empty submission -- but those are
/// different situations. A muted/empty submission has nothing to cancel; a
/// timed-out one *is* queued somewhere, just without an id its caller can
/// `Cancel` it by. `Submitted::TimedOut` keeps that distinction visible
/// instead of quietly reporting a queued utterance as if nothing had been
/// queued at all.
const SUBMIT_REPLY_TIMEOUT: Duration = Duration::from_millis(250);

/// A submission plus the channel its answer goes back on.
type SubmitJob = (String, SayOpts, Sender<Result<Submitted, String>>);

enum Msg {
    Cmd(Command),
    Submit(Box<SubmitJob>),
    ReplaceSink(Box<dyn AudioSink>),
    Shutdown,
}

#[derive(Clone)]
pub struct EngineHandle {
    tx: Sender<Msg>,
    latest: Arc<Mutex<Snapshot>>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Set once the engine thread's run loop has exited, by any means:
    /// a normal `break` out of the loop, or the loop body unwinding on a
    /// panic (e.g. from a `Synthesizer` implementation). Lets the daemon's
    /// main loop notice a `Command::Shutdown` that arrived over the channel
    /// (e.g. from a D-Bus `Quit` call), or a crashed engine thread, without
    /// having to call `shutdown` itself.
    shut_down: Arc<AtomicBool>,
}

impl EngineHandle {
    pub fn spawn(
        cfg: Config,
        synth: Box<dyn Synthesizer>,
        sink: Box<dyn AudioSink>,
    ) -> EngineHandle {
        let (tx, rx) = mpsc::channel::<Msg>();
        let engine = Engine::new(cfg, synth, sink);
        let latest = Arc::new(Mutex::new(engine.snapshot()));
        let published = latest.clone();
        let shut_down = Arc::new(AtomicBool::new(false));
        let shut_down_writer = shut_down.clone();

        let thread = std::thread::Builder::new()
            .name("sayd-engine".into())
            .spawn(move || run(engine, rx, published, shut_down_writer))
            .ok();

        EngineHandle {
            tx,
            latest,
            thread: Arc::new(Mutex::new(thread)),
            shut_down,
        }
    }

    /// Fire and forget. A dead engine thread silently drops the command —
    /// the daemon notices through the snapshot, not here.
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(Msg::Cmd(cmd));
    }

    /// Submit text and wait for the engine's answer, up to
    /// [`SUBMIT_REPLY_TIMEOUT`] -- see its doc comment for why this is
    /// bounded and what a timeout means.
    pub fn submit(&self, text: String, opts: SayOpts) -> Result<Submitted, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Msg::Submit(Box::new((text, opts, reply_tx))))
            .map_err(|_| "engine thread is not running".to_string())?;
        match reply_rx.recv_timeout(SUBMIT_REPLY_TIMEOUT) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => Ok(Submitted::TimedOut),
            Err(RecvTimeoutError::Disconnected) => {
                Err("engine thread stopped before answering".to_string())
            }
        }
    }

    /// The most recently published snapshot.
    pub fn snapshot(&self) -> Snapshot {
        match self.latest.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Hand the engine a fresh audio device after a failure. Fire and
    /// forget, like `send`: the daemon learns the outcome through the next
    /// published snapshot.
    pub fn replace_sink(&self, sink: Box<dyn AudioSink>) {
        let _ = self.tx.send(Msg::ReplaceSink(sink));
    }

    /// Whether the engine thread has exited, for any reason: `shutdown` was
    /// called, a `Command::Shutdown` arrived over the channel (e.g. from a
    /// D-Bus `Quit` call), or the thread panicked. Lets the daemon's main
    /// loop notice a dead engine without having to poll `shutdown` itself.
    pub fn has_shut_down(&self) -> bool {
        self.shut_down.load(Ordering::Acquire)
    }

    /// Ask the engine thread to stop, and wait for it. Safe to call more
    /// than once, including concurrently from two clones: only the first
    /// caller to take the `JoinHandle` actually joins it, and every other
    /// caller's `Shutdown` send is a harmless no-op once the thread has
    /// already exited (the channel is simply dropped along with it).
    pub fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
        let handle = match self.thread.lock() {
            Ok(mut g) => g.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

/// Marks `shut_down` true when dropped, on any exit from `run`'s loop --
/// a normal `break` as well as a panic unwinding out of the loop body.
/// `engine.tick()` calls into `Synthesizer::phonemize`/`synth` and
/// `AudioSink` methods, none of which are `catch_unwind`-wrapped (and
/// `phonemize` has no `Result` to fail through anyway), so a panic there
/// unwinds straight past a plain `shut_down.store(true, ..)` placed after
/// the loop. Tying the store to `Drop` instead means it runs no matter how
/// the stack unwinds.
struct ShutDownOnDrop(Arc<AtomicBool>);

impl Drop for ShutDownOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Copy the engine's current snapshot into the published slot.
///
/// Called both right after handling a message and after every `tick()` (see
/// `run`): a `submit`/`handle` state change must be visible the moment it
/// happens, not deferred until the tick that happens to follow it -- `tick`
/// synthesizes one whole chunk synchronously and can run for seconds on
/// real ONNX, so "only after tick" meant every D-Bus property lagged a
/// submission by a full chunk (C1).
fn publish(published: &Arc<Mutex<Snapshot>>, engine: &Engine) {
    match published.lock() {
        Ok(mut g) => *g = engine.snapshot(),
        Err(poisoned) => *poisoned.into_inner() = engine.snapshot(),
    }
}

fn run(
    mut engine: Engine,
    rx: Receiver<Msg>,
    published: Arc<Mutex<Snapshot>>,
    shut_down: Arc<AtomicBool>,
) {
    let _guard = ShutDownOnDrop(shut_down);

    loop {
        match rx.recv_timeout(TICK_INTERVAL) {
            Ok(Msg::Cmd(c)) => {
                engine.handle(c);
                publish(&published, &engine);
            }
            Ok(Msg::Submit(job)) => {
                let (text, opts, reply) = *job;
                let r = engine.submit(text, opts);
                // Published before the reply is sent, not after: a caller
                // waiting on `reply_rx` (in particular `EngineHandle::
                // submit`) must never be able to observe a stale snapshot
                // by polling immediately after its own call returns.
                publish(&published, &engine);
                let _ = reply.send(r);
            }
            Ok(Msg::ReplaceSink(sink)) => {
                engine.replace_sink(sink);
                publish(&published, &engine);
            }
            // An explicit `Msg::Shutdown` breaks immediately, skipping the
            // final `tick()` and publish below; a `Command::Shutdown` sent
            // through `send()` instead falls through to `tick()` and is
            // only caught by `is_shutdown()` afterwards. Harmless either
            // way -- the engine and sink are dropped either way -- but the
            // two routes are not identical.
            Ok(Msg::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        engine.tick();
        publish(&published, &engine);

        if engine.is_shutdown() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::VecSink;
    use crate::config::Config;
    use crate::engine::{Command, SayOpts, State};
    use crate::synth::StubSynthesizer;
    use std::time::{Duration, Instant};

    fn handle() -> EngineHandle {
        EngineHandle::spawn(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        )
    }

    /// A `Synthesizer` that sleeps before returning, modelling the 3.5-7.5s
    /// measured per chunk on real ONNX. `StubSynthesizer` returns instantly,
    /// which is exactly why the engine tests above never caught C1 or C2:
    /// "published after tick" and "published immediately" are
    /// indistinguishable when tick() itself takes no measurable time.
    struct SlowSynthesizer {
        delay: Duration,
    }

    impl SlowSynthesizer {
        fn new(delay: Duration) -> Self {
            SlowSynthesizer { delay }
        }
    }

    impl crate::synth::Synthesizer for SlowSynthesizer {
        fn phonemize(&mut self, text: &str, _voice: &str) -> String {
            text.to_string()
        }
        fn fits(&mut self, _phonemes: &str) -> bool {
            true
        }
        fn synth(&mut self, phonemes: &str, _voice: &str, _speed: f32) -> Result<Vec<f32>, String> {
            std::thread::sleep(self.delay);
            Ok(vec![0.0; phonemes.len() * 100])
        }
        fn unload(&mut self) {}
        fn is_loaded(&self) -> bool {
            true
        }
    }

    #[test]
    fn published_snapshot_reflects_a_submission_without_waiting_for_a_slow_synthesis() {
        // C1, pinned with a synthesizer slow enough that "after tick" and
        // "immediately" cannot be confused by timing noise: `Engine::
        // submit` sets `state = Speaking` synchronously (see engine.rs), so
        // the published snapshot must show it right away, not after the
        // chunk of synthesis that submission triggers.
        let delay = Duration::from_millis(600);
        let h = EngineHandle::spawn(
            Config::default(),
            Box::new(SlowSynthesizer::new(delay)),
            Box::new(VecSink::new(24_000 * 10)),
        );
        h.submit("hello there.".into(), SayOpts::default())
            .expect("accepted");
        let s = h.snapshot();
        assert_eq!(
            s.state,
            State::Speaking,
            "the snapshot must reflect the submission immediately, not after \
             the {delay:?} synthesis it triggers; got {s:?}"
        );
        h.shutdown();
    }

    #[test]
    fn submit_returns_promptly_even_while_a_slow_synthesis_is_in_flight() {
        // C2, pinned directly: `EngineHandle::submit` used to block on an
        // unbounded `reply_rx.recv()` that was only serviced between
        // whole-chunk ticks, so a second submission arriving while the
        // first's chunk was still synthesizing had to wait out that whole
        // chunk -- measured 3.5-7.5s on real ONNX, blowing the CLI's 3s
        // timeout. `SUBMIT_REPLY_TIMEOUT`'s bounded backstop is what
        // actually guarantees this now; see its doc comment.
        let delay = Duration::from_secs(2);
        let h = EngineHandle::spawn(
            Config::default(),
            Box::new(SlowSynthesizer::new(delay)),
            Box::new(VecSink::new(24_000 * 10)),
        );
        // Long enough that `chunk()` splits it into more than one chunk, so
        // the engine thread is still inside `tick()`'s `synth()` call for
        // the *second* submission below, not just idling between chunks.
        h.submit(
            "This is one sentence in a long batch of text. ".repeat(15),
            SayOpts::default(),
        )
        .expect("accepted");
        // Give the engine thread a moment to actually enter the slow
        // `synth()` call before submitting again, so this reliably
        // exercises "arrives mid-synthesis" rather than racing the first
        // message's own handling.
        std::thread::sleep(Duration::from_millis(100));

        let start = Instant::now();
        let result = h.submit("a second, unrelated utterance.".into(), SayOpts::default());
        let elapsed = start.elapsed();

        assert!(
            elapsed < delay / 4,
            "submit took {elapsed:?} while a {delay:?} synthesis was in flight -- \
             it must not wait for it"
        );
        assert!(
            result.is_ok(),
            "a timed-out-but-queued submission must not be reported as an error \
             -- that would recreate the exact double-queue bug this backstop \
             exists to avoid: {result:?}"
        );
        // Finding 3: this is the one scenario that reliably produces a
        // timeout (250ms backstop vs. a synthesis call known to still be
        // running 2s in), so it is also the place to pin that the timeout
        // case is a distinguishable outcome -- not the same `Discarded` a
        // muted/empty submission gets, which would leave a caller unable to
        // tell "nothing was queued" from "something was queued, but I have
        // no id for it."
        assert_eq!(
            result,
            Ok(Submitted::TimedOut),
            "a submission the engine thread was too busy to confirm in time \
             must be reported as TimedOut, not folded into Discarded"
        );
        h.shutdown();
    }

    /// Poll until `f` holds or the deadline passes.
    fn wait_for(h: &EngineHandle, label: &str, f: impl Fn(&crate::engine::Snapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if f(&h.snapshot()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "timed out waiting for {label}; snapshot = {:?}",
            h.snapshot()
        );
    }

    #[test]
    fn spawns_idle_and_reports_a_snapshot() {
        let h = handle();
        assert_eq!(h.snapshot().state, State::Idle);
        h.shutdown();
    }

    #[test]
    fn submit_returns_the_engines_answer() {
        let h = handle();
        let outcome = h
            .submit("hello there.".into(), SayOpts::default())
            .expect("accepted");
        assert!(matches!(outcome, Submitted::Queued(_)), "got {outcome:?}");
        h.shutdown();
    }

    #[test]
    fn submit_propagates_a_rejection() {
        let h = EngineHandle::spawn(
            Config {
                max_chars: 5,
                ..Config::default()
            },
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        );
        assert!(h
            .submit("much too long".into(), SayOpts::default())
            .is_err());
        h.shutdown();
    }

    #[test]
    fn the_engine_ticks_on_its_own_thread() {
        let h = handle();
        h.submit("hello there. this is a test.".into(), SayOpts::default())
            .expect("accepted");
        wait_for(&h, "speaking", |s| s.state == State::Speaking);
        h.shutdown();
    }

    #[test]
    fn commands_reach_the_engine() {
        let h = handle();
        h.submit("hello there. this is a test.".into(), SayOpts::default())
            .expect("accepted");
        wait_for(&h, "speaking", |s| s.state == State::Speaking);
        h.send(Command::Stop);
        wait_for(&h, "idle after stop", |s| {
            s.state == State::Idle && s.queue_len == 0
        });
        h.shutdown();
    }

    #[test]
    fn the_handle_is_clonable_and_shared_across_threads() {
        let h = handle();
        let h2 = h.clone();
        let t = std::thread::spawn(move || {
            h2.submit("from another thread.".into(), SayOpts::default())
        });
        let r = t.join().expect("thread panicked");
        let outcome = r.expect("accepted");
        assert!(matches!(outcome, Submitted::Queued(_)), "got {outcome:?}");
        h.shutdown();
    }

    #[test]
    fn shutdown_joins_the_thread_and_does_not_hang() {
        let h = handle();
        let start = Instant::now();
        h.shutdown();
        assert!(start.elapsed() < Duration::from_secs(5), "shutdown hung");
    }

    #[test]
    fn snapshot_after_shutdown_does_not_panic() {
        let h = handle();
        let h2 = h.clone();
        h.shutdown();
        // The engine thread is gone; the last published snapshot is still readable.
        let _ = h2.snapshot();
    }

    #[test]
    fn shutdown_twice_on_the_same_handle_does_not_hang_or_panic() {
        let h = handle();
        h.shutdown();
        let start = Instant::now();
        h.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "second shutdown hung"
        );
    }

    #[test]
    fn concurrent_shutdown_from_two_clones_does_not_hang_or_panic() {
        let h = handle();
        let h2 = h.clone();
        let t1 = std::thread::spawn(move || h.shutdown());
        let t2 = std::thread::spawn(move || h2.shutdown());
        t1.join().expect("shutdown panicked on handle 1");
        t2.join().expect("shutdown panicked on handle 2");
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn handle_is_send_and_sync() {
        assert_send_sync::<EngineHandle>();
    }

    #[test]
    fn replace_sink_forwards_to_the_engine() {
        // A failed sink should be swappable for a fresh one without
        // restarting the engine thread.
        let h = EngineHandle::spawn(
            Config {
                max_chars: 5,
                ..Config::default()
            },
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        );
        // Push the engine into Error via a rejection.
        assert!(h
            .submit("much too long".into(), SayOpts::default())
            .is_err());
        wait_for(&h, "error", |s| s.state == State::Error);

        h.replace_sink(Box::new(VecSink::new(24_000 * 10)));
        wait_for(&h, "idle after replace_sink", |s| s.state == State::Idle);

        // max_chars is still 5, so keep this within the limit.
        let outcome = h
            .submit("hi.".into(), SayOpts::default())
            .expect("accepted");
        assert!(matches!(outcome, Submitted::Queued(_)), "got {outcome:?}");
        h.shutdown();
    }

    #[test]
    fn has_shut_down_is_false_until_shutdown_completes() {
        let h = handle();
        assert!(!h.has_shut_down());
        h.shutdown();
        assert!(h.has_shut_down());
    }

    #[test]
    fn has_shut_down_becomes_true_after_a_shutdown_command_over_the_channel() {
        let h = handle();
        assert!(!h.has_shut_down());
        h.send(Command::Shutdown);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if h.has_shut_down() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for has_shut_down() to become true");
    }

    #[test]
    fn has_shut_down_becomes_true_when_the_engine_thread_panics() {
        // A `Synthesizer` whose `phonemize` panics: `phonemize` runs inside
        // `engine.tick()` on the engine thread, has no `Result` to fail
        // through, and nothing wraps the loop body in `catch_unwind`. The
        // panic unwinds the engine thread. `has_shut_down()` must still
        // become true -- that is what lets a daemon loop notice a crashed
        // engine rather than waiting on it forever.
        struct PhonemizePanics;
        impl crate::synth::Synthesizer for PhonemizePanics {
            fn phonemize(&mut self, _text: &str, _voice: &str) -> String {
                panic!("PhonemizePanics: synthesizer exploded on purpose");
            }
            fn fits(&mut self, _phonemes: &str) -> bool {
                true
            }
            fn synth(
                &mut self,
                _phonemes: &str,
                _voice: &str,
                _speed: f32,
            ) -> Result<Vec<f32>, String> {
                Ok(Vec::new())
            }
            fn unload(&mut self) {}
            fn is_loaded(&self) -> bool {
                true
            }
        }

        // The panic below is expected and its message is uninteresting; a
        // std test binary still prints it to stderr via the default hook on
        // every run. Swap in a no-op hook for the duration of this test so
        // that output stays quiet, and restore the previous hook
        // immediately after so a genuine panic elsewhere in the suite still
        // prints normally.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let h = EngineHandle::spawn(
            Config::default(),
            Box::new(PhonemizePanics),
            Box::new(VecSink::new(24_000 * 10)),
        );
        // Queuing succeeds -- `submit` only enqueues the text; `phonemize`
        // is not called until a later `tick()` picks the utterance up off
        // the queue, on the engine thread.
        let _ = h.submit("hello there.".into(), SayOpts::default());

        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            if h.has_shut_down() {
                break Ok(());
            }
            if Instant::now() >= deadline {
                break Err(());
            }
            std::thread::sleep(Duration::from_millis(5));
        };

        std::panic::set_hook(previous_hook);
        assert!(
            result.is_ok(),
            "timed out waiting for has_shut_down() to become true after a panic"
        );
    }

    #[test]
    fn submit_after_shutdown_returns_an_error_instead_of_hanging() {
        // Pins the explicitly-named attack scenario: a `submit` issued after
        // the engine thread is gone must come back with an error promptly,
        // not block forever. Run on its own thread with a timeout so a
        // regression fails loudly instead of hanging the test suite.
        let h = handle();
        h.shutdown();

        let h2 = h.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(h2.submit("hello there.".into(), SayOpts::default()));
        });

        match done_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(r) => assert!(
                r.is_err(),
                "submit after shutdown should be rejected, got {r:?}"
            ),
            Err(_) => panic!("submit after shutdown hung instead of returning promptly"),
        }
    }
}
