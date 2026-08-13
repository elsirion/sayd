//! The engine: one owner for the queue, the synthesizer and the sink.
//!
//! Nothing else in the process may touch that state. Commands come in,
//! immutable snapshots go out, so the tray, MPRIS and any settings window
//! cannot disagree about what is playing.
//!
//! `tick` does one unit of work and returns. The binary calls it in a loop
//! between commands; tests call it directly, which is what makes the whole
//! state machine assertable without threads or timing.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crate::audio::AudioSink;
use crate::chunk::{chunk, refit, Chunk};
use crate::cleanup::clean;
use crate::config::Config;
use crate::queue::{Policy, Queue, Source, Utterance};
use crate::synth::Synthesizer;

/// Measured: 181.55 s of audio for 498 words in kokoro-eval's passage bench.
pub const SECONDS_PER_WORD: f64 = 0.365;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Speaking,
    Paused,
    Error,
}

#[derive(Clone, Debug, Default)]
pub struct SayOpts {
    pub policy: Option<Policy>,
    pub voice: Option<String>,
    pub speed: Option<f32>,
    /// Defaults to `Source::DBus`, whose policy is `Enqueue`.
    pub source: Source,
}

#[derive(Clone, Debug)]
pub enum Command {
    Say { text: String, opts: SayOpts },
    Pause,
    Resume,
    PlayPause,
    Stop,
    Next,
    SkipSentence,
    ClearQueue,
    Cancel(u64),
    SetMuted(bool),
    SetVoice(String),
    SetSpeed(f32),
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub state: State,
    pub muted: bool,
    pub voice: String,
    pub speed: f32,
    pub queue_len: usize,
    pub remaining_secs: f64,
    pub current_text: String,
    pub current_id: u64,
    pub error: Option<String>,
}

/// The utterance currently being spoken, decomposed into chunks.
struct Current {
    id: u64,
    text: String,
    voice: String,
    speed: f32,
    chunks: Vec<Chunk>,
    next_chunk: usize,
    /// Samples produced but not yet accepted by the sink.
    carry: Vec<f32>,
}

pub struct Engine {
    cfg: Config,
    synth: Box<dyn Synthesizer>,
    sink: Box<dyn AudioSink>,
    queue: Queue,
    current: Option<Current>,
    state: State,
    /// Invariant: `error.is_some() <=> state == State::Error`. Every place
    /// that changes `state` away from or into `Error` must keep this in
    /// sync in the same step; see `submit`, `dismiss_error_and_go_idle` and
    /// the pop branch of `tick`.
    ///
    /// Second invariant: `state != State::Paused` implies `!sink.is_paused()`.
    /// `Command::Pause` is the only place that pauses the sink, and it always
    /// sets `state = Paused` in the same step; every route by which `state`
    /// can leave `Paused` (`Command::Resume`, and every call to
    /// `dismiss_error_and_go_idle`, which is the one place that can move
    /// `state` to `Idle` unconditionally -- including out of `Paused` --
    /// from `Stop`, `Next`, `SkipSentence` and `SetMuted(true)`) must
    /// unpause the sink in that same step, or a later command has no way
    /// left to reach it: `Resume` only fires when `state == Paused`.
    error: Option<String>,
    idle_since: Option<Instant>,
    shutdown: bool,
}

impl Engine {
    pub fn new(cfg: Config, synth: Box<dyn Synthesizer>, sink: Box<dyn AudioSink>) -> Self {
        Engine {
            cfg,
            synth,
            sink,
            queue: Queue::new(),
            current: None,
            state: State::Idle,
            error: None,
            idle_since: Some(Instant::now()),
            shutdown: false,
        }
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    pub fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Say { text, opts } => {
                let _ = self.submit(text, opts);
            }
            Command::Pause => {
                if self.state == State::Speaking {
                    self.state = State::Paused;
                    self.sink.set_paused(true);
                }
            }
            Command::Resume => {
                if self.state == State::Paused {
                    self.state = State::Speaking;
                    self.sink.set_paused(false);
                }
            }
            Command::PlayPause => match self.state {
                State::Speaking => self.handle(Command::Pause),
                State::Paused => self.handle(Command::Resume),
                _ => {}
            },
            Command::Stop => {
                // The shut-up verb: always returns to a clean slate, even
                // from Error. `dismiss_error_and_go_idle` unpauses the sink
                // (see the pause invariant on the `error` field's doc
                // comment), so there is no separate `set_paused(false)` here.
                self.queue.clear();
                self.discard_current();
                self.dismiss_error_and_go_idle();
            }
            Command::Next => {
                self.discard_current();
                if self.queue.is_empty() {
                    self.dismiss_error_and_go_idle();
                }
            }
            Command::SkipSentence => {
                self.sink.clear();
                if let Some(c) = self.current.as_mut() {
                    c.carry.clear();
                    if c.next_chunk >= c.chunks.len() {
                        self.current = None;
                        if self.queue.is_empty() {
                            self.dismiss_error_and_go_idle();
                        }
                    }
                } else if self.queue.is_empty() {
                    self.dismiss_error_and_go_idle();
                }
            }
            Command::ClearQueue => {
                self.queue.clear();
            }
            Command::Cancel(id) => {
                self.queue.cancel(id);
            }
            Command::SetMuted(m) => {
                self.cfg.muted = m;
                if m {
                    self.queue.clear();
                    self.discard_current();
                    self.dismiss_error_and_go_idle();
                }
            }
            Command::SetVoice(v) => self.cfg.voice = v,
            Command::SetSpeed(s) => self.cfg.speed = s.clamp(0.5, 2.0),
            Command::Shutdown => {
                self.shutdown = true;
                self.handle(Command::Stop);
            }
        }
    }

    /// Submit text for synthesis, returning the queued utterance's id on
    /// acceptance, `None` if accepted but not queued (muted or empty after
    /// cleanup), or the rejection reason on failure. This is the synchronous
    /// answer a caller gets back for *its own* submission -- distinct from
    /// `error`/`state`, which describe the engine as a whole and must not be
    /// disturbed by a rejection that has nothing to do with whatever else is
    /// legitimately in flight (see the busy check below).
    ///
    /// Returns `Ok(Some(id))` if the text was queued for synthesis.
    /// Returns `Ok(None)` if the submission was accepted but nothing was
    /// queued (muted, or empty after cleanup). This is not an error.
    /// Returns `Err(reason)` if the submission was rejected (e.g. text too long).
    ///
    /// `handle(Command::Say { .. })` calls this and discards the result, so
    /// the existing command path is unchanged for callers that do not care.
    /// A D-Bus `Say` method or a CLI entry point should call this directly
    /// to learn whether its own submission was accepted and queued.
    pub fn submit(&mut self, text: String, opts: SayOpts) -> Result<Option<u64>, String> {
        if text.chars().count() > self.cfg.max_chars {
            let msg = format!(
                "text is {} characters, limit is {}",
                text.chars().count(),
                self.cfg.max_chars
            );
            // Something unrelated is genuinely still playing (or paused):
            // this rejection must not stomp on it and report a global Error
            // when nothing about A is actually wrong. The caller still
            // learns the submission was refused, via the `Err` returned
            // here rather than a shared snapshot field.
            if self.state != State::Speaking && self.state != State::Paused {
                self.state = State::Error;
                self.error = Some(msg.clone());
            }
            return Err(msg);
        }
        if self.state == State::Error {
            // Error only ever arises with nothing legitimately in flight
            // (see the branch above and `tick`'s synth-failure path), so
            // there is no `current` to preserve here.
            self.state = State::Idle;
            self.error = None;
        }
        if self.cfg.muted {
            return Ok(None); // accepted and discarded
        }

        let cleaned = clean(&text, &self.cfg.cleanup);
        if cleaned.trim().is_empty() {
            return Ok(None);
        }

        let policy = opts.policy.unwrap_or_else(|| opts.source.default_policy());
        let id = self.queue.next_id();
        let u = Utterance {
            id,
            text: cleaned,
            voice: opts.voice.unwrap_or_else(|| self.cfg.voice.clone()),
            speed: opts.speed.unwrap_or(self.cfg.speed),
            source: opts.source,
        };
        self.queue.submit(u, policy);

        match policy {
            Policy::Replace | Policy::Interrupt => self.discard_current(),
            _ => {}
        }

        if self.state != State::Paused {
            self.state = State::Speaking;
            self.idle_since = None;
        }

        Ok(Some(id))
    }

    /// One unit of work: top up the sink, or advance the queue, or unload.
    pub fn tick(&mut self) {
        if self.state == State::Paused {
            return;
        }

        // Flush anything left over from a previous partial push.
        if let Some(c) = self.current.as_mut() {
            if !c.carry.is_empty() {
                let n = self.sink.push(&c.carry);
                c.carry.drain(..n);
                if !c.carry.is_empty() {
                    return; // sink is full; try again next tick
                }
            }
        }

        // Start the next utterance if nothing is current.
        if self.current.is_none() {
            match self.queue.pop_front() {
                Some(u) => {
                    // `refit`'s predicate must be `Fn`, but phonemizing needs
                    // `&mut self.synth`. A closure that captures `&mut
                    // self.synth` and calls a `&mut self` method through it
                    // is inferred as `FnMut`, which `refit` will not accept.
                    // A `RefCell` around the mutable borrow gives interior
                    // mutability so the closure only ever needs `&self`,
                    // satisfying `Fn` while still driving the real
                    // synthesizer on every call `refit` makes (as opposed to
                    // cloning the synthesizer, or phonemizing text up front
                    // and discarding the result).
                    //
                    // The voice must come from `u` before `u.text`/`u.voice`
                    // are moved into `Current` below, since this predicate
                    // runs first.
                    let voice = u.voice.clone();
                    let cs = chunk(&u.text, self.cfg.chunking.target_chars);
                    let synth = RefCell::new(&mut self.synth);
                    let cs = refit(cs, |t| {
                        let mut synth = synth.borrow_mut();
                        let ph = synth.phonemize(t, &voice);
                        synth.fits(&ph)
                    });
                    self.current = Some(Current {
                        id: u.id,
                        text: u.text,
                        voice: u.voice,
                        speed: u.speed,
                        chunks: cs,
                        next_chunk: 0,
                        carry: Vec::new(),
                    });
                    self.state = State::Speaking;
                    self.error = None;
                    self.idle_since = None;
                }
                None => {
                    if self.state != State::Error {
                        self.go_idle();
                    }
                    self.maybe_unload();
                    return;
                }
            }
        }

        // Bound the lookahead: stop synthesizing once the sink is well fed.
        // `lookahead_chunks` comes straight from a user-editable config file
        // with no validation, so the `+ 1` must not be able to overflow (it
        // would panic with overflow checks on, or silently wrap to 0 and
        // then get masked back up to a divisor of 2 by `.max(2)` in a
        // release build -- behaviour that must not depend on build profile).
        let headroom = self.sink.capacity().saturating_sub(self.sink.pending());
        let divisor = self.cfg.chunking.lookahead_chunks.saturating_add(1).max(2);
        if headroom < self.sink.capacity() / divisor {
            return;
        }

        let Some(c) = self.current.as_mut() else { return };
        if c.next_chunk >= c.chunks.len() {
            self.current = None;
            if self.queue.is_empty() {
                self.go_idle();
            }
            return;
        }

        let text = c.chunks[c.next_chunk].text.clone();
        let voice = c.voice.clone();
        let speed = c.speed;
        c.next_chunk += 1;

        let phonemes = self.synth.phonemize(&text, &voice);
        match self.synth.synth(&phonemes, &voice, speed) {
            Ok(samples) => {
                let n = self.sink.push(&samples);
                if n < samples.len() {
                    if let Some(c) = self.current.as_mut() {
                        c.carry = samples[n..].to_vec();
                    }
                }
            }
            Err(e) => {
                self.state = State::Error;
                self.error = Some(e);
                self.current = None;
                self.queue.clear();
            }
        }
    }

    /// Both call sites in `tick` reach this only once `current` is already
    /// `None` and the queue is empty, so the one thing left to check before
    /// announcing `Idle` is whether the sink has actually finished playing
    /// what it was given. Without that check the engine would report `Idle`
    /// while `sink.pending()` (and therefore `Snapshot::remaining_secs`)
    /// still counts seconds of audio that has not been heard yet --
    /// self-contradictory, and both M2's D-Bus `State` property and M3's
    /// MPRIS `PlaybackStatus` read this field directly.
    ///
    /// Only reached with `state != State::Paused`: `tick` returns before
    /// this point while paused, so this never has to reason about a sink
    /// that is deliberately not draining.
    fn go_idle(&mut self) {
        if self.state != State::Error {
            if self.sink.pending() > 0 {
                // Nothing left to queue or synthesize, but the sink is
                // still draining what it already has -- stay Speaking until
                // it actually finishes, not the instant nothing is left to
                // feed it. `idle_since` stays untouched (still `None` from
                // when this utterance started) so `maybe_unload` keeps
                // declining to fire; see its own guard.
                self.state = State::Speaking;
                return;
            }
            self.state = State::Idle;
        }
        if self.idle_since.is_none() {
            self.idle_since = Some(Instant::now());
        }
    }

    /// Like `go_idle`, but unconditionally -- including out of `Error` and
    /// `Paused`, and without waiting on `sink.pending()`. Used by the
    /// explicit "shut up" commands (`Stop`, `Next`, `SkipSentence`,
    /// `SetMuted`), which must be able to dismiss a stuck error even though
    /// nothing else can. `go_idle` itself stays Error-preserving and
    /// pending-gated: it is also reached from plain `tick()` when the queue
    /// drains with no command involved at all, and an error (or audio still
    /// playing) must not evaporate on its own just because the caller kept
    /// polling.
    ///
    /// Also enforces the pause invariant documented on the `error` field:
    /// every one of this function's callers is a point where `state` can
    /// move to `Idle` regardless of what it was before, including `Paused`,
    /// and `Command::Resume` -- the only other place that unpauses the sink
    /// -- is itself gated on `state == Paused`, so this is the last chance
    /// to unpause before that guard becomes permanently unreachable.
    fn dismiss_error_and_go_idle(&mut self) {
        self.state = State::Idle;
        self.error = None;
        self.sink.set_paused(false);
        if self.idle_since.is_none() {
            self.idle_since = Some(Instant::now());
        }
    }

    /// Discard whatever is currently speaking and drop any buffered audio.
    /// Shared by `Stop`, `Next`, a `Replace`/`Interrupt` submission and
    /// `SetMuted(true)` -- the four places that blow away the current
    /// utterance.
    fn discard_current(&mut self) {
        self.current = None;
        self.sink.clear();
    }

    fn maybe_unload(&mut self) {
        if !self.synth.is_loaded() {
            return;
        }
        let Some(since) = self.idle_since else { return };
        if since.elapsed() >= Duration::from_secs(self.cfg.idle_unload_secs) {
            self.synth.unload();
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            state: self.state,
            muted: self.cfg.muted,
            voice: self.cfg.voice.clone(),
            speed: self.cfg.speed,
            queue_len: self.queue.len(),
            remaining_secs: self.remaining_secs(),
            current_text: self.current.as_ref().map(|c| c.text.clone()).unwrap_or_default(),
            current_id: self.current.as_ref().map(|c| c.id).unwrap_or(0),
            error: self.error.clone(),
        }
    }

    /// Three buckets, in decreasing order of certainty: exact for audio
    /// already accepted by the sink, exact for audio already synthesized
    /// but still parked in `carry` waiting for room in the sink, and
    /// estimated (via `SECONDS_PER_WORD`) for text not yet spoken at all.
    fn remaining_secs(&self) -> f64 {
        let sr = self.synth.sample_rate() as f64;
        let carried = self.current.as_ref().map(|c| c.carry.len()).unwrap_or(0) as f64;
        let buffered = (self.sink.pending() as f64 + carried) / sr;

        let mut words = 0usize;
        if let Some(c) = self.current.as_ref() {
            for ch in &c.chunks[c.next_chunk.min(c.chunks.len())..] {
                words += ch.text.split_whitespace().count();
            }
        }
        for u in self.queue.iter() {
            words += u.text.split_whitespace().count();
        }
        let speed = self.cfg.speed.max(0.1) as f64;
        buffered + (words as f64 * SECONDS_PER_WORD) / speed
    }

    // --- test helpers -------------------------------------------------

    #[cfg(test)]
    fn audio_written(&self) -> usize {
        self.sink.total_written()
    }

    #[cfg(test)]
    fn is_model_loaded(&self) -> bool {
        self.synth.is_loaded()
    }

    #[cfg(test)]
    fn snapshot_queue_ids(&self) -> Vec<u64> {
        self.queue.iter().map(|u| u.id).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::audio::{AudioSink, VecSink};
    use crate::config::Config;
    use crate::synth::StubSynthesizer;

    fn engine() -> Engine {
        Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        )
    }

    /// `Engine` owns its sink as a private `Box<dyn AudioSink>`, so a test
    /// that wants to model *playback* -- not just accept samples -- needs a
    /// handle it can drain from outside after handing the sink away. This
    /// wraps `VecSink` (which already knows how to simulate playback via
    /// `drain`) behind `Arc<Mutex<_>>` so both sides can reach it:
    /// `AudioSink: Send` rules out `Rc<RefCell<_>>`.
    ///
    /// This is the "explicitly-drained sink" option from C1's two choices
    /// (test double that reports samples as played, vs. an explicitly
    /// drained sink) -- chosen because `VecSink::drain` already exists and
    /// models exactly the fact the review measured: a sink that only frees
    /// space as audio is actually played, not the instant it's pushed. A
    /// sink that auto-drains on every `push`/`pending` call was considered
    /// and rejected: it would make `pending() > 0` unobservable, which is
    /// the exact condition C1's new tests need to hold under an explicit
    /// hand.
    #[derive(Clone)]
    struct SharedVecSink(Arc<Mutex<VecSink>>);

    impl AudioSink for SharedVecSink {
        fn push(&mut self, samples: &[f32]) -> usize {
            self.0.lock().unwrap().push(samples)
        }
        fn pending(&self) -> usize {
            self.0.lock().unwrap().pending()
        }
        fn clear(&mut self) {
            self.0.lock().unwrap().clear()
        }
        fn set_paused(&mut self, paused: bool) {
            self.0.lock().unwrap().set_paused(paused)
        }
        fn is_paused(&self) -> bool {
            self.0.lock().unwrap().is_paused()
        }
        fn capacity(&self) -> usize {
            self.0.lock().unwrap().capacity()
        }
        fn total_written(&self) -> usize {
            self.0.lock().unwrap().total_written()
        }
    }

    /// Build an engine over a sink the test can drain (simulate playback)
    /// from outside, plus a handle to do that draining with.
    fn engine_with_drainable_sink(capacity: usize) -> (Engine, Arc<Mutex<VecSink>>) {
        let sink = Arc::new(Mutex::new(VecSink::new(capacity)));
        let e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(SharedVecSink(sink.clone())),
        );
        (e, sink)
    }

    fn say(text: &str) -> Command {
        Command::Say { text: text.into(), opts: SayOpts::default() }
    }

    /// Run `tick` until idle or `max` iterations, whichever comes first.
    fn run(e: &mut Engine, max: usize) {
        for _ in 0..max {
            if e.snapshot().state == State::Idle {
                return;
            }
            e.tick();
        }
    }

    #[test]
    fn starts_idle() {
        let e = engine();
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.queue_len, 0);
        assert_eq!(s.error, None);
    }

    #[test]
    fn say_moves_to_speaking_and_produces_audio() {
        let mut e = engine();
        e.handle(say("Hello there. This is a test."));
        e.tick();
        assert_eq!(e.snapshot().state, State::Speaking);
        run(&mut e, 500);
        assert!(e.audio_written() > 0, "expected samples to reach the sink");
    }

    #[test]
    fn returns_to_idle_when_the_queue_empties() {
        // C1: with nothing draining it, a `VecSink` never empties on its
        // own, so reaching `Idle` here now requires modelling playback --
        // this is what pins the original intent of this test (an emptied
        // queue eventually leads to `Idle`) now that the engine also waits
        // for the sink to actually finish. See
        // `state_stays_speaking_while_audio_is_still_pending_in_the_sink`
        // for the part of C1's behaviour this test used to (silently) not
        // cover: that it does *not* go `Idle` before that.
        let (mut e, sink) = engine_with_drainable_sink(24_000 * 10);
        e.handle(say("Short."));
        run(&mut e, 500);
        sink.lock().unwrap().drain(usize::MAX);
        e.tick();
        assert_eq!(e.snapshot().state, State::Idle);
    }

    #[test]
    fn state_stays_speaking_while_audio_is_still_pending_in_the_sink() {
        // C1, pinned directly: the engine must not announce Idle the instant
        // there is nothing left to *synthesize* -- it must wait until the
        // sink has actually finished playing what it already has.
        let (mut e, sink) = engine_with_drainable_sink(24_000 * 10);
        e.handle(say("Hello there. This is sayd speaking from the engine."));
        run(&mut e, 500);

        let pending = sink.lock().unwrap().pending();
        assert!(pending > 0, "test is only meaningful with audio still buffered");
        let s = e.snapshot();
        assert_eq!(
            s.state,
            State::Speaking,
            "must not report Idle with {pending} samples still unplayed"
        );
        assert!(
            s.remaining_secs > 0.0,
            "remaining_secs must agree with state: both say audio is still outstanding"
        );

        sink.lock().unwrap().drain(usize::MAX);
        e.tick();
        assert_eq!(e.snapshot().state, State::Idle, "must go Idle once the sink actually drains");
    }

    #[test]
    fn paused_engine_with_pending_audio_does_not_go_idle_or_spin() {
        // The interaction C1 calls out explicitly: while Paused the sink
        // does not drain (nothing is popping it) and `tick` returns before
        // ever reaching `go_idle`, so a paused engine with buffered audio
        // must neither drift to Idle on its own nor loop/panic under
        // repeated ticking.
        let (mut e, sink) = engine_with_drainable_sink(24_000 * 10);
        e.handle(say("Hello there. This is a reasonably long test sentence."));
        run(&mut e, 500);
        assert_eq!(e.snapshot().state, State::Speaking);
        let pending_before = sink.lock().unwrap().pending();
        assert!(pending_before > 0, "test is only meaningful with audio still buffered");

        e.handle(Command::Pause);
        assert_eq!(e.snapshot().state, State::Paused);

        for _ in 0..200 {
            e.tick();
        }

        let s = e.snapshot();
        assert_eq!(s.state, State::Paused, "must not spuriously become Idle while paused");
        assert_eq!(
            sink.lock().unwrap().pending(),
            pending_before,
            "a paused sink must not drain, and tick must not touch it while paused"
        );
    }

    #[test]
    fn pause_and_resume_toggle_state() {
        let mut e = engine();
        e.handle(say("Hello there. This is a test."));
        e.tick();
        e.handle(Command::Pause);
        assert_eq!(e.snapshot().state, State::Paused);
        e.handle(Command::Resume);
        assert_eq!(e.snapshot().state, State::Speaking);
    }

    #[test]
    fn play_pause_toggles_both_ways() {
        let mut e = engine();
        e.handle(say("Hello there."));
        e.tick();
        e.handle(Command::PlayPause);
        assert_eq!(e.snapshot().state, State::Paused);
        e.handle(Command::PlayPause);
        assert_eq!(e.snapshot().state, State::Speaking);
    }

    #[test]
    fn pause_when_idle_is_a_no_op() {
        let mut e = engine();
        e.handle(Command::Pause);
        assert_eq!(e.snapshot().state, State::Idle);
    }

    #[test]
    fn stop_clears_the_queue_and_goes_idle() {
        let mut e = engine();
        e.handle(say("First one here."));
        e.handle(say("Second one here."));
        e.tick();
        e.handle(Command::Stop);
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.queue_len, 0, "Stop is the shut-up verb: it clears everything");
    }

    #[test]
    fn clear_queue_keeps_the_current_utterance() {
        let mut e = engine();
        e.handle(say("First one here."));
        e.handle(say("Second one here."));
        e.tick();
        e.handle(Command::ClearQueue);
        let s = e.snapshot();
        assert_eq!(s.state, State::Speaking, "the current utterance survives");
        assert_eq!(s.queue_len, 0);
    }

    #[test]
    fn next_advances_to_the_following_utterance() {
        let mut e = engine();
        e.handle(say("First."));
        e.handle(say("Second."));
        e.tick();
        let first = e.snapshot().current_id;
        e.handle(Command::Next);
        e.tick();
        assert_ne!(e.snapshot().current_id, first);
    }

    #[test]
    fn hotkey_source_replaces_by_default() {
        let mut e = engine();
        e.handle(say("First one here."));
        e.handle(say("Second one here."));
        e.tick();
        e.handle(Command::Say {
            text: "Selected text.".into(),
            opts: SayOpts { source: Source::Hotkey, ..Default::default() },
        });
        assert_eq!(e.snapshot().queue_len, 1, "replace drops everything pending");
    }

    #[test]
    fn explicit_policy_overrides_the_source_default() {
        let mut e = engine();
        e.handle(say("First one here."));
        e.handle(Command::Say {
            text: "Selected.".into(),
            opts: SayOpts {
                source: Source::Hotkey,
                policy: Some(Policy::Enqueue),
                ..Default::default()
            },
        });
        assert_eq!(e.snapshot().queue_len, 2, "explicit enqueue beats the hotkey default");
    }

    #[test]
    fn muted_accepts_and_discards() {
        let mut e = engine();
        e.handle(Command::SetMuted(true));
        e.handle(say("Nobody hears this."));
        run(&mut e, 100);
        assert_eq!(e.audio_written(), 0, "muted must produce no audio");
        assert_eq!(e.snapshot().state, State::Idle);
    }

    #[test]
    fn text_over_max_chars_is_rejected() {
        let cfg = Config { max_chars: 10, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        );
        e.handle(say("this is definitely longer than ten characters"));
        let s = e.snapshot();
        assert_eq!(s.state, State::Error);
        assert!(s.error.as_deref().unwrap_or("").contains("10"));
    }

    #[test]
    fn a_later_successful_say_clears_the_error() {
        let cfg = Config { max_chars: 10, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("far too long to be accepted"));
        assert_eq!(e.snapshot().state, State::Error);
        e.handle(say("ok."));
        assert_eq!(e.snapshot().error, None);
    }

    #[test]
    fn stop_dismisses_a_stuck_error() {
        // Stop is the daemon's designated "shut up" command: it must be able
        // to clear Error even though nothing else naturally can.
        let cfg = Config { max_chars: 5, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("way too long for the limit"));
        assert_eq!(e.snapshot().state, State::Error);
        e.handle(Command::Stop);
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.error, None);
    }

    #[test]
    fn next_dismisses_a_stuck_error_when_the_queue_is_empty() {
        let cfg = Config { max_chars: 5, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("way too long for the limit"));
        assert_eq!(e.snapshot().state, State::Error);
        e.handle(Command::Next);
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.error, None);
    }

    #[test]
    fn a_stuck_error_survives_plain_ticking_with_no_command() {
        // Mirror image of the two tests above: an error must not clear
        // itself just because the caller kept polling `tick()` -- only an
        // explicit command may dismiss it. `synth_failure_surfaces_as_error_
        // and_does_not_wedge` already covers the synth-failure route to
        // Error; this covers the rejection route.
        let cfg = Config { max_chars: 5, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("way too long for the limit"));
        assert_eq!(e.snapshot().state, State::Error);
        for _ in 0..20 {
            e.tick();
        }
        let s = e.snapshot();
        assert_eq!(s.state, State::Error, "no command was issued; the error must persist");
        assert!(s.error.is_some());
    }

    #[test]
    fn rejection_while_speaking_leaves_playback_untouched() {
        // Submitting an over-long text while an unrelated utterance A is
        // legitimately speaking must not flip the engine to Error: A is
        // unaffected and should play to completion. The rejection must
        // still be observable -- now synchronously, as `submit`'s `Err`.
        let cfg = Config { max_chars: 5, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("Hi.")); // 3 chars, within the limit
        e.tick();
        let before = e.snapshot();
        assert_eq!(before.state, State::Speaking);
        let id = before.current_id;

        let result = e.submit(
            "this one is definitely too long for the limit".into(),
            SayOpts::default(),
        );

        assert!(
            result.as_ref().unwrap_err().contains('5'),
            "the rejection must still be observable: {result:?}"
        );
        let after = e.snapshot();
        assert_eq!(after.state, State::Speaking, "A must keep playing");
        assert_eq!(after.current_id, id, "A must not be disturbed");
        assert_eq!(after.error, None, "nothing about A is actually wrong");
        assert_eq!(after.queue_len, 0, "the rejected text must not be queued");
    }

    #[test]
    fn rejection_while_paused_leaves_playback_untouched() {
        let cfg = Config { max_chars: 5, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("Hi."));
        e.tick();
        e.handle(Command::Pause);
        assert_eq!(e.snapshot().state, State::Paused);

        let result = e.submit(
            "this one is definitely too long for the limit".into(),
            SayOpts::default(),
        );

        assert!(result.is_err());
        let after = e.snapshot();
        assert_eq!(after.state, State::Paused);
        assert_eq!(after.error, None);
    }

    #[test]
    fn error_state_invariant_holds_after_every_command_from_every_state() {
        // Pin both invariants documented on `Engine::error`, not just the
        // individual scenarios covering each of them elsewhere: `error.
        // is_some() == (state == State::Error)`, and `state == State::Paused`
        // whenever the sink is left paused -- both after every command, from
        // every reachable state. (C2's bug was exactly a case where the
        // second invariant broke while the first stayed fine: `Next`,
        // `SkipSentence` and `SetMuted(true)` all correctly reached `Idle`
        // with `error == None`, while quietly leaving `sink.paused == true`
        // behind.)
        fn assert_invariants(e: &Engine, ctx: &str) {
            let s = e.snapshot();
            assert_eq!(
                s.error.is_some(),
                s.state == State::Error,
                "{ctx}: error={:?} state={:?}",
                s.error,
                s.state
            );
            assert!(
                s.state == State::Paused || !e.sink.is_paused(),
                "{ctx}: state={:?} but the sink is still paused",
                s.state
            );
        }

        fn all_commands() -> Vec<Command> {
            vec![
                say("Something reasonably short."),
                Command::Pause,
                Command::Resume,
                Command::PlayPause,
                Command::Stop,
                Command::Next,
                Command::SkipSentence,
                Command::ClearQueue,
                Command::Cancel(1),
                Command::SetMuted(true),
                Command::SetMuted(false),
                Command::SetVoice("am_fenrir".into()),
                Command::SetSpeed(1.5),
                Command::Shutdown,
            ]
        }

        fn build_idle() -> Engine {
            engine()
        }
        fn build_speaking() -> Engine {
            let mut e = engine();
            e.handle(say("Hello there. This keeps it busy for quite a while indeed."));
            e.tick();
            e
        }
        fn build_paused() -> Engine {
            let mut e = build_speaking();
            e.handle(Command::Pause);
            e
        }
        fn build_error() -> Engine {
            let cfg = Config { max_chars: 5, ..Config::default() };
            let mut e = Engine::new(
                cfg,
                Box::new(StubSynthesizer::new()),
                Box::new(VecSink::new(24_000 * 10)),
            );
            e.handle(say("way too long for the limit"));
            e
        }

        fn check_from(name: &str, build: fn() -> Engine) {
            for cmd in all_commands() {
                let mut e = build();
                assert_invariants(&e, &format!("before {name} -> {cmd:?}"));
                e.handle(cmd.clone());
                assert_invariants(&e, &format!("after {name} -> {cmd:?}"));
            }
        }

        check_from("idle", build_idle);
        check_from("speaking", build_speaking);
        check_from("paused", build_paused);
        check_from("error", build_error);
    }

    #[test]
    fn huge_lookahead_chunks_does_not_overflow() {
        // `lookahead_chunks` comes straight from a user-editable config file
        // with no validation. A value near `usize::MAX` used to panic on
        // `+ 1` with overflow checks on (which is how tests run), and
        // silently wrap to a different divisor in release builds.
        let mut cfg = Config::default();
        cfg.chunking.lookahead_chunks = usize::MAX;
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("Hello there. This is a test."));
        for _ in 0..10 {
            e.tick();
        }
        assert!(e.audio_written() > 0, "expected samples to reach the sink");
    }

    #[test]
    fn remaining_seconds_includes_audio_parked_in_carry() {
        // A tiny sink forces most of the first chunk's synthesized audio
        // into `carry` rather than the sink itself. `next_chunk` has already
        // advanced past that chunk, so if `carry` weren't counted the
        // estimate would understate the time left by nearly the whole chunk.
        let mut e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(100)),
        );
        e.handle(say("A reasonably long sentence to force a big carry remainder."));
        e.tick(); // pop -> synth -> partial push -> the rest parked in carry
        let s = e.snapshot();
        // With only 100 samples possibly in the sink (100 / 24_000 s), any
        // reading much larger than that must be coming from carry.
        assert!(
            s.remaining_secs > 1.0,
            "carry must count toward remaining_secs, got {}",
            s.remaining_secs
        );
    }

    #[test]
    fn remaining_seconds_scales_with_word_count() {
        let mut e = engine();
        e.handle(say("one two three four five six seven eight nine ten."));
        let s = e.snapshot();
        // 10 words at 0.365 s each, give or take the estimator's rounding.
        assert!(s.remaining_secs > 2.0, "got {}", s.remaining_secs);
        assert!(s.remaining_secs < 6.0, "got {}", s.remaining_secs);
    }

    #[test]
    fn remaining_seconds_halves_at_double_speed() {
        let mut e = engine();
        e.handle(Command::SetSpeed(2.0));
        e.handle(say("one two three four five six seven eight nine ten."));
        let fast = e.snapshot().remaining_secs;
        let mut e2 = engine();
        e2.handle(say("one two three four five six seven eight nine ten."));
        let normal = e2.snapshot().remaining_secs;
        assert!(fast < normal * 0.75, "fast {fast} vs normal {normal}");
    }

    #[test]
    fn set_voice_applies_to_the_next_utterance() {
        let mut e = engine();
        e.handle(Command::SetVoice("am_fenrir".into()));
        assert_eq!(e.snapshot().voice, "am_fenrir");
    }

    #[test]
    fn cancel_removes_a_queued_utterance() {
        let mut e = engine();
        e.handle(say("First."));
        e.handle(say("Second."));
        let id = e.snapshot_queue_ids()[1];
        e.handle(Command::Cancel(id));
        assert_eq!(e.snapshot().queue_len, 1);
    }

    #[test]
    fn idle_unload_drops_the_model_after_the_configured_delay() {
        // unload as soon as idle. C1: as in `returns_to_idle_when_the_queue_
        // empties`, actually reaching `Idle` -- the precondition this test
        // is exercising -- now requires draining the sink first.
        let cfg = Config { idle_unload_secs: 0, ..Config::default() };
        let sink = Arc::new(Mutex::new(VecSink::new(24_000 * 10)));
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(SharedVecSink(sink.clone())),
        );
        e.handle(say("Hello."));
        run(&mut e, 500);
        assert_eq!(
            e.snapshot().state,
            State::Speaking,
            "sanity: audio must still be pending before the drain below"
        );
        sink.lock().unwrap().drain(usize::MAX);
        e.tick(); // reach Idle and trigger the unload check in the same tick
        assert_eq!(e.snapshot().state, State::Idle);
        assert!(!e.is_model_loaded(), "expected the model to unload when idle");
    }

    #[test]
    fn model_does_not_unload_while_speaking() {
        let cfg = Config { idle_unload_secs: 0, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("A reasonably long sentence to keep it busy for a while."));
        e.tick();
        e.tick();
        assert!(e.is_model_loaded());
    }

    #[test]
    fn lookahead_is_bounded() {
        let mut e = engine();
        // Long text, small sink: the engine must stop synthesizing once the
        // sink is full rather than running ahead unboundedly.
        e.handle(say(&"word ".repeat(500)));
        for _ in 0..50 {
            e.tick();
        }
        assert!(
            e.audio_written() <= 24_000 * 10 + 24_000,
            "engine ran further ahead than the sink can hold"
        );
    }

    #[test]
    fn skip_sentence_stops_current_audio_promptly() {
        // Correction 3: with the default target_chars (400) this 57-char text
        // becomes a single chunk, so after the first tick next_chunk ==
        // chunks.len() and SkipSentence would drop straight to Idle. Use a
        // small target_chars so the three sentences become three chunks
        // (20, 21, 15 chars; none merge since 20 + 1 + 21 > 25), which is
        // what this test is actually meant to exercise.
        let mut cfg = Config::default();
        cfg.chunking.target_chars = 25;
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("First sentence here. Second sentence here. Third one here."));
        e.tick();
        let before = e.audio_written();
        e.handle(Command::SkipSentence);
        e.tick();
        assert!(e.audio_written() >= before, "skip must not lose the sink");
        assert_eq!(e.snapshot().state, State::Speaking);
    }

    #[test]
    fn skip_sentence_dismisses_a_stuck_error_from_a_rejected_submission() {
        // `SkipSentence`'s call to `dismiss_error_and_go_idle` used to sit
        // inside `if let Some(c) = self.current.as_mut()`, but `Error`
        // always implies `current.is_none()` -- so the branch could never
        // run while in `Error` and `SkipSentence` from an error state was a
        // silent no-op, contradicting `dismiss_error_and_go_idle`'s own doc
        // comment. This pins the rejection entry point to `Error`.
        let cfg = Config { max_chars: 5, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("way too long for the limit"));
        assert_eq!(e.snapshot().state, State::Error);
        e.handle(Command::SkipSentence);
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.error, None);
    }

    #[test]
    fn skip_sentence_dismisses_a_stuck_error_from_a_synthesis_failure() {
        // Mirror of the test above via the other entry point into `Error`.
        struct Failing;
        impl crate::synth::Synthesizer for Failing {
            fn phonemize(&mut self, t: &str, _voice: &str) -> String {
                t.into()
            }
            fn fits(&mut self, _: &str) -> bool {
                true
            }
            fn synth(&mut self, _: &str, _: &str, _: f32) -> Result<Vec<f32>, String> {
                Err("model exploded".into())
            }
            fn unload(&mut self) {}
            fn is_loaded(&self) -> bool {
                true
            }
        }
        let mut e = Engine::new(
            Config::default(),
            Box::new(Failing),
            Box::new(VecSink::new(24_000)),
        );
        e.handle(say("Anything."));
        for _ in 0..20 {
            e.tick();
        }
        assert_eq!(e.snapshot().state, State::Error);
        e.handle(Command::SkipSentence);
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.error, None);
    }

    #[test]
    fn synth_failure_surfaces_as_error_and_does_not_wedge() {
        struct Failing;
        impl crate::synth::Synthesizer for Failing {
            fn phonemize(&mut self, t: &str, _voice: &str) -> String {
                t.into()
            }
            fn fits(&mut self, _: &str) -> bool {
                true
            }
            fn synth(&mut self, _: &str, _: &str, _: f32) -> Result<Vec<f32>, String> {
                Err("model exploded".into())
            }
            fn unload(&mut self) {}
            fn is_loaded(&self) -> bool {
                true
            }
        }
        let mut e = Engine::new(
            Config::default(),
            Box::new(Failing),
            Box::new(VecSink::new(24_000)),
        );
        e.handle(say("Anything."));
        for _ in 0..20 {
            e.tick();
        }
        let s = e.snapshot();
        assert_eq!(s.state, State::Error);
        assert!(s.error.as_deref().unwrap_or("").contains("model exploded"));
    }

    #[test]
    fn empty_text_is_accepted_and_produces_nothing() {
        let mut e = engine();
        e.handle(say("   "));
        run(&mut e, 50);
        assert_eq!(e.snapshot().state, State::Idle);
        assert_eq!(e.audio_written(), 0);
    }

    #[test]
    fn submit_rejection_while_idle_returns_err_and_sets_error_state() {
        // Nothing is legitimately in flight, so the rejection both answers
        // the caller directly and becomes the engine-wide `Error`.
        let cfg = Config { max_chars: 5, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let result = e.submit("way too long for the limit".into(), SayOpts::default());
        let msg = result.expect_err("over-long text must be rejected");
        assert!(msg.contains('5'), "got {msg:?}");
        let s = e.snapshot();
        assert_eq!(s.state, State::Error);
        assert_eq!(s.error.as_deref(), Some(msg.as_str()));
    }

    #[test]
    fn submit_rejection_while_speaking_returns_err_but_leaves_state_untouched() {
        // Same busy-vs-idle distinction as `rejection_while_speaking_leaves_
        // playback_untouched`, but pinned directly against the new method's
        // return value rather than only through `handle`.
        let cfg = Config { max_chars: 5, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("Hi."));
        e.tick();
        assert_eq!(e.snapshot().state, State::Speaking);

        let result = e.submit(
            "this one is definitely too long for the limit".into(),
            SayOpts::default(),
        );

        assert!(result.is_err());
        let s = e.snapshot();
        assert_eq!(s.state, State::Speaking, "the unrelated playback must continue");
        assert_eq!(s.error, None);
    }

    #[test]
    fn submit_accepted_returns_the_id_that_later_appears_as_current_id() {
        let mut e = engine();
        let id = e
            .submit("Hello there. This is a test.".into(), SayOpts::default())
            .expect("well-formed text must be accepted")
            .expect("well-formed text must be queued");
        e.tick();
        assert_eq!(e.snapshot().current_id, id);
    }

    #[test]
    fn submit_returns_none_when_muted() {
        let mut e = engine();
        e.handle(Command::SetMuted(true));
        assert_eq!(e.submit("nobody hears this".into(), SayOpts::default()), Ok(None));
    }

    #[test]
    fn submit_returns_none_for_text_that_is_empty_after_cleanup() {
        let mut e = engine();
        assert_eq!(e.submit("   ".into(), SayOpts::default()), Ok(None));
    }

    #[test]
    fn submit_returns_some_nonzero_id_when_queued() {
        let mut e = engine();
        let id = e.submit("hello there.".into(), SayOpts::default()).expect("accepted");
        assert!(id.is_some());
        assert_ne!(id, Some(0), "id 0 is the nothing-is-playing sentinel");
    }

    #[test]
    fn submit_still_returns_err_when_rejected() {
        let mut e = Engine::new(
            Config { max_chars: 5, ..Config::default() },
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        );
        assert!(e.submit("far too long".into(), SayOpts::default()).is_err());
    }
}
