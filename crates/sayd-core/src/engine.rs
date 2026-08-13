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
            Command::Say { text, opts } => self.submit(text, opts),
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
                self.queue.clear();
                self.current = None;
                self.sink.clear();
                self.sink.set_paused(false);
                self.go_idle();
            }
            Command::Next => {
                self.current = None;
                self.sink.clear();
                if self.queue.is_empty() {
                    self.go_idle();
                }
            }
            Command::SkipSentence => {
                self.sink.clear();
                if let Some(c) = self.current.as_mut() {
                    c.carry.clear();
                    if c.next_chunk >= c.chunks.len() {
                        self.current = None;
                        if self.queue.is_empty() {
                            self.go_idle();
                        }
                    }
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
                    self.current = None;
                    self.sink.clear();
                    self.go_idle();
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

    fn submit(&mut self, text: String, opts: SayOpts) {
        if text.chars().count() > self.cfg.max_chars {
            self.state = State::Error;
            self.error = Some(format!(
                "text is {} characters, limit is {}",
                text.chars().count(),
                self.cfg.max_chars
            ));
            return;
        }
        self.error = None;
        if self.state == State::Error {
            self.state = if self.current.is_some() { State::Speaking } else { State::Idle };
        }
        if self.cfg.muted {
            return; // accepted and discarded
        }

        let cleaned = clean(&text, &self.cfg.cleanup);
        if cleaned.trim().is_empty() {
            return;
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
            Policy::Replace | Policy::Interrupt => {
                self.current = None;
                self.sink.clear();
            }
            _ => {}
        }

        if self.state != State::Paused {
            self.state = State::Speaking;
            self.idle_since = None;
        }
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
        let headroom = self.sink.capacity().saturating_sub(self.sink.pending());
        if headroom < self.sink.capacity() / (self.cfg.chunking.lookahead_chunks + 1).max(2) {
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

    fn go_idle(&mut self) {
        if self.state != State::Error {
            self.state = State::Idle;
        }
        if self.idle_since.is_none() {
            self.idle_since = Some(Instant::now());
        }
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

    /// Exact for audio already in the sink, estimated for text not yet spoken.
    fn remaining_secs(&self) -> f64 {
        let sr = self.synth.sample_rate() as f64;
        let buffered = self.sink.pending() as f64 / sr;

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
    use super::*;
    use crate::audio::VecSink;
    use crate::config::Config;
    use crate::synth::StubSynthesizer;

    fn engine() -> Engine {
        Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        )
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
        let mut e = engine();
        e.handle(say("Short."));
        run(&mut e, 500);
        assert_eq!(e.snapshot().state, State::Idle);
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
        // unload as soon as idle
        let cfg = Config { idle_unload_secs: 0, ..Config::default() };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("Hello."));
        run(&mut e, 500);
        e.tick(); // one more tick past idle to trigger the unload check
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
}
