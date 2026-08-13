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
use crate::engine::{Command, Engine, SayOpts, Snapshot};
use crate::synth::Synthesizer;

/// How long the engine thread waits for a command before ticking anyway.
///
/// `tick` must be called even when no command arrives: it is what advances
/// synthesis, drains the sink, and runs the idle-unload check.
const TICK_INTERVAL: Duration = Duration::from_millis(10);

/// A submission plus the channel its answer goes back on.
type SubmitJob = (String, SayOpts, Sender<Result<Option<u64>, String>>);

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
    /// Set just before the engine thread's run loop returns. Lets the
    /// daemon's main loop notice a `Command::Shutdown` that arrived over the
    /// channel (e.g. from a D-Bus `Quit` call) without having to call
    /// `shutdown` itself.
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

    /// Submit text and wait for the engine's answer.
    pub fn submit(&self, text: String, opts: SayOpts) -> Result<Option<u64>, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Msg::Submit(Box::new((text, opts, reply_tx))))
            .map_err(|_| "engine thread is not running".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "engine thread stopped before answering".to_string())?
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

    /// Whether the engine thread has exited, whether because `shutdown` was
    /// called or because a `Command::Shutdown` arrived over the channel
    /// (e.g. from a D-Bus `Quit` call). Lets the daemon's main loop notice
    /// the latter without having to poll `shutdown` itself.
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

fn run(
    mut engine: Engine,
    rx: Receiver<Msg>,
    published: Arc<Mutex<Snapshot>>,
    shut_down: Arc<AtomicBool>,
) {
    loop {
        match rx.recv_timeout(TICK_INTERVAL) {
            Ok(Msg::Cmd(c)) => engine.handle(c),
            Ok(Msg::Submit(job)) => {
                let (text, opts, reply) = *job;
                let r = engine.submit(text, opts);
                let _ = reply.send(r);
            }
            Ok(Msg::ReplaceSink(sink)) => engine.replace_sink(sink),
            Ok(Msg::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        engine.tick();

        if let Ok(mut g) = published.lock() {
            *g = engine.snapshot();
        }

        if engine.is_shutdown() {
            break;
        }
    }

    shut_down.store(true, Ordering::Release);
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

    /// Poll until `f` holds or the deadline passes.
    fn wait_for(h: &EngineHandle, label: &str, f: impl Fn(&crate::engine::Snapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if f(&h.snapshot()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {label}; snapshot = {:?}", h.snapshot());
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
        let id = h.submit("hello there.".into(), SayOpts::default()).expect("accepted");
        assert!(id.is_some());
        h.shutdown();
    }

    #[test]
    fn submit_propagates_a_rejection() {
        let h = EngineHandle::spawn(
            Config { max_chars: 5, ..Config::default() },
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        );
        assert!(h.submit("much too long".into(), SayOpts::default()).is_err());
        h.shutdown();
    }

    #[test]
    fn the_engine_ticks_on_its_own_thread() {
        let h = handle();
        h.submit("hello there. this is a test.".into(), SayOpts::default()).expect("accepted");
        wait_for(&h, "speaking", |s| s.state == State::Speaking);
        h.shutdown();
    }

    #[test]
    fn commands_reach_the_engine() {
        let h = handle();
        h.submit("hello there. this is a test.".into(), SayOpts::default()).expect("accepted");
        wait_for(&h, "speaking", |s| s.state == State::Speaking);
        h.send(Command::Stop);
        wait_for(&h, "idle after stop", |s| s.state == State::Idle && s.queue_len == 0);
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
        assert!(r.expect("accepted").is_some());
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
        assert!(start.elapsed() < Duration::from_secs(5), "second shutdown hung");
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
            Config { max_chars: 5, ..Config::default() },
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        );
        // Push the engine into Error via a rejection.
        assert!(h.submit("much too long".into(), SayOpts::default()).is_err());
        wait_for(&h, "error", |s| s.state == State::Error);

        h.replace_sink(Box::new(VecSink::new(24_000 * 10)));
        wait_for(&h, "idle after replace_sink", |s| s.state == State::Idle);

        // max_chars is still 5, so keep this within the limit.
        let id = h.submit("hi.".into(), SayOpts::default()).expect("accepted");
        assert!(id.is_some());
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
}
