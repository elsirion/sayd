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

/// How many pending utterances `Snapshot::queue_heads` carries. The tray
/// menu this feeds (M3) shows a handful of upcoming items plus a "+N more"
/// count for the rest -- `queue_len` already carries the true total, so the
/// head list only needs to cover what a menu can usefully display at once.
pub const QUEUE_HEAD_LIMIT: usize = 5;

/// How many characters of a queued utterance's text `Snapshot::queue_heads`
/// keeps, per entry. `Snapshot` is cloned on every publish (once per tick,
/// `handle.rs::publish`) and read by pollers on a fixed interval, so this is
/// a menu label, not a paragraph -- 60 characters is enough to identify an
/// utterance at a glance (comparable to a single line in a desktop tray
/// menu) without a long queue of large submissions making every publish
/// (and every diff against the previous snapshot) allocate and copy
/// kilobytes of text nothing will read past the first few words of.
pub const QUEUE_HEAD_TEXT_CHARS: usize = 60;

/// Truncate `s` to at most `max_chars` characters, always on a character
/// boundary. Byte-slicing at a fixed offset would panic on multi-byte text
/// (e.g. an emoji or non-Latin script) whose boundaries don't land on
/// `max_chars` bytes; counting chars first and collecting that many avoids
/// it. Appends an ellipsis when truncation actually happened, so a
/// truncated head is visibly distinct from a short utterance that just
/// happens to fit.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('\u{2026}'); // '…'
    out
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Speaking,
    Paused,
    Error,
}

/// Where an `Error` came from, so a caller (in practice, the daemon's
/// device-recovery loop) can react to *why* the engine is in `Error`
/// instead of treating every cause alike.
///
/// C3: the daemon used to trigger audio-device reacquisition on `state ==
/// Error` alone, which was right for a genuine device failure but wrong for
/// a synthesis failure (bad model path, corrupt weights) or a rejected
/// submission (text too long) -- reacquiring a perfectly fine device
/// cleared the error and made the daemon look recovered while the real
/// problem (a missing model file, say) was still there and every
/// submission kept failing the same way, silently.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// This submission alone was rejected on its own terms (e.g. over
    /// `max_chars`); nothing about the engine as a whole is broken, so a
    /// later, valid submission clears it -- see `Engine::submit`.
    Rejected,
    /// `AudioSink::take_error` reported a device failure. The only kind
    /// that should arm a device-reacquisition loop.
    Sink,
    /// `Synthesizer::synth` itself failed. Reacquiring the audio device
    /// would not fix this; it persists until an explicit "shut up" command
    /// (`Stop`/`Next`/`SkipSentence`/`SetMuted`) dismisses it, and new
    /// submissions are rejected rather than silently accepted while it
    /// holds -- see `Engine::submit`.
    Synth,
}

/// The three distinguishable answers a submission can get back.
///
/// Finding 3: `EngineHandle::submit`'s backstop timeout used to fold into
/// the same `Ok(None)` that means "accepted but nothing queued" (muted, or
/// empty after cleanup). Those are different: a timed-out submission *is*
/// queued (the message reached the engine; `Engine::submit` itself already
/// returned before the timeout could even fire -- see `EngineHandle::
/// submit`), just without an id the caller can `Cancel` it by. `Engine::
/// submit` -- synchronous, no channel involved -- can only ever produce
/// `Queued` or `Discarded`; `TimedOut` is minted solely by `EngineHandle::
/// submit`'s timeout branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// Queued for synthesis with this id.
    Queued(u64),
    /// Accepted but nothing was queued: muted, or empty after cleanup. Not
    /// an error -- see `Engine::submit`'s doc comment.
    Discarded,
    /// The engine already handled this submission (queued or discarded),
    /// but the confirmation did not arrive before `EngineHandle::submit`'s
    /// bounded wait gave up. There is no id to report, so a caller cannot
    /// `Cancel` this one utterance specifically -- unlike `Discarded`, where
    /// there is nothing to cancel in the first place.
    TimedOut,
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
    /// Replace every setting at once, from the config file or the settings
    /// window.
    ///
    /// This is the only way a config change reaches a running engine: the
    /// settings window, the file watcher and the CLI all send this, so there
    /// is exactly one place where "new config" turns into "new behaviour".
    /// `SetVoice`/`SetSpeed` remain for the single-value paths (MPRIS `Rate`)
    /// that have no whole-config to send.
    ApplyConfig(Config),
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub state: State,
    pub muted: bool,
    /// The current utterance's own voice while one is playing (which may
    /// differ from the configured default -- a submission can override it
    /// per utterance via `SayOpts`); the configured default with nothing
    /// current. MPRIS's `Rate`/`Metadata` (M3) read this, so it must track
    /// what is actually audible, not just the config file.
    pub voice: String,
    /// Same rule as `voice`: the current utterance's own speed while one is
    /// playing, the configured default otherwise.
    pub speed: f32,
    /// The configured default speed (`Config::speed`), always -- regardless
    /// of whether an utterance with its own per-submission override
    /// (`SayOpts::speed`) is currently playing.
    ///
    /// I1: distinct from `speed` above on purpose. `speed` deliberately
    /// tracks what is *audible* right now (Finding 1), but MPRIS's `Rate`
    /// needs to track what `SetSpeed` actually controls: reading `speed`
    /// there meant a `SetSpeed` issued while an utterance was playing was
    /// invisible on `Rate` -- not clamped, not rejected, just silently not
    /// reflected -- until that utterance finished, because `speed` does not
    /// change again until the *next* utterance starts. A client cannot tell
    /// "ignored" from "applied" in that gap. `configured_speed` is what
    /// `SetSpeed` writes and is current the instant it lands, so a reader
    /// that wants "did my write take" -- not "what is this utterance doing"
    /// -- has a field to read.
    pub configured_speed: f32,
    pub queue_len: usize,
    pub remaining_secs: f64,
    pub current_text: String,
    pub current_id: u64,
    /// Up to `QUEUE_HEAD_LIMIT` pending utterances, in play order, as
    /// `(id, text)` with `text` truncated to `QUEUE_HEAD_TEXT_CHARS`
    /// characters (see `truncate_chars`). Additive alongside `queue_len`,
    /// which still counts the whole queue even when it is longer than this
    /// list -- the tray menu (M3) wants both: a handful of visible entries
    /// and an accurate "+N more".
    pub queue_heads: Vec<(u64, String)>,
    pub error: Option<String>,
    /// `None` iff `error` is `None` -- see the invariant documented on
    /// `Engine::error`. Not exposed over D-Bus (the wire `Error` property
    /// stays a plain string); this is for in-process callers like the
    /// daemon's recovery loop that need to know *why*, not just *that*.
    pub error_kind: Option<ErrorKind>,
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
    /// `tick`'s device-failure branch (`Paused -> Error`) is the same kind
    /// of route and unpauses the sink for the same reason.
    error: Option<String>,
    /// Third invariant, alongside the two above: `error_kind.is_some() <=>
    /// error.is_some()`. Every place that sets or clears `error` must set or
    /// clear this in the same step. See `ErrorKind`'s own doc comment for
    /// why this exists.
    error_kind: Option<ErrorKind>,
    idle_since: Option<Instant>,
    /// A model/thread-count change (`Command::ApplyConfig`) that arrived
    /// while `current` was `Some` -- i.e. mid-utterance -- so the unload was
    /// deferred instead of dropping the session out from under whatever is
    /// playing. Applied by `apply_pending_unload`, called from every place
    /// `current` transitions back to `None`: that is the first moment the
    /// session is no longer needed by anything in flight, whether the
    /// utterance finished on its own, was skipped past its last chunk,
    /// discarded (`Stop`/`Next`/a replacing submission/`SetMuted(true)`), or
    /// lost to a synth/device error. See `ApplyConfig`'s handler for why
    /// unloading immediately is wrong in the first place.
    pending_unload: bool,
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
            error_kind: None,
            idle_since: Some(Instant::now()),
            pending_unload: false,
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
                        self.apply_pending_unload();
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
            Command::ApplyConfig(mut cfg) => {
                // Same clamp as `SetSpeed`, done before `cfg` moves into
                // `self.cfg` below so an out-of-range speed is never even
                // briefly stored: a hand-edited config file is untrusted.
                cfg.speed = cfg.speed.clamp(0.5, 2.0);
                // Only `model` and `threads` can invalidate a loaded ORT
                // session; the synthesizer decides, since it owns those.
                // Everything else here is read per-utterance from `self.cfg`
                // and so takes effect on the next submission by assignment
                // alone.
                //
                // IMPORTANT 1: an immediate `unload` here would drop the
                // live session out from under whatever `current` is already
                // playing -- `tick` would then have to rebuild a session
                // (~1.27 GB, over a second) for the *remaining* chunks of
                // the utterance already in progress, audibly switching
                // model mid-sentence, or -- if the newly named model file
                // isn't installed -- turning the rest of that utterance and
                // everything queued behind it into a sticky
                // `ErrorKind::Synth`. The brief's own rationale was "the
                // next utterance picks up the new settings"; unloading only
                // once nothing is in flight is what actually makes that
                // true instead of "whichever utterance happens to be
                // playing when the settings window is open". So: unload now
                // when nothing is mid-utterance (`current.is_none()`,
                // matching today's behaviour when idle), otherwise defer to
                // the moment `current` next clears -- see
                // `apply_pending_unload`.
                if self.synth.reconfigure(&cfg) {
                    if self.current.is_some() {
                        self.pending_unload = true;
                    } else {
                        self.synth.unload();
                    }
                }
                self.cfg = cfg;
            }
            Command::Shutdown => {
                self.shutdown = true;
                self.handle(Command::Stop);
            }
        }
    }

    /// Submit text for synthesis, returning a [`Submitted`] describing what
    /// happened to *this* submission on acceptance, or the rejection reason
    /// on failure. This is the synchronous answer a caller gets back for its
    /// own submission -- distinct from `error`/`state`, which describe the
    /// engine as a whole and must not be disturbed by a rejection that has
    /// nothing to do with whatever else is legitimately in flight (see the
    /// busy check below).
    ///
    /// Returns `Ok(Submitted::Queued(id))` if the text was queued for
    /// synthesis. Returns `Ok(Submitted::Discarded)` if the submission was
    /// accepted but nothing was queued (muted, or empty after cleanup) --
    /// this is not an error. `Engine::submit` never returns
    /// `Ok(Submitted::TimedOut)`: that variant exists for
    /// `EngineHandle::submit`'s bounded wait, which this synchronous method
    /// has no notion of. Returns `Err(reason)` if the submission was
    /// rejected (e.g. text too long).
    ///
    /// `handle(Command::Say { .. })` calls this and discards the result, so
    /// the existing command path is unchanged for callers that do not care.
    /// A D-Bus `Say` method or a CLI entry point should call this directly
    /// to learn whether its own submission was accepted and queued.
    pub fn submit(&mut self, text: String, opts: SayOpts) -> Result<Submitted, String> {
        // Checked before `max_chars` (the reverse of this method's original
        // order): a systemic error (`Sink`/`Synth`) must persist and reject
        // *every* new submission while it holds, not just ones that happen
        // to also be over-long. Checking `max_chars` first would let an
        // over-long submission's own `Rejected` overwrite a standing
        // `Sink`/`Synth` error, which is exactly the "submissions are
        // silently swallowed" bug C3 fixes -- see `ErrorKind`'s doc comment.
        if self.state == State::Error {
            match self.error_kind {
                Some(ErrorKind::Sink) | Some(ErrorKind::Synth) => {
                    return Err(self
                        .error
                        .clone()
                        .unwrap_or_else(|| "the engine is in an error state".to_string()));
                }
                // `Rejected` is about *this* submission trying again, not a
                // systemic problem, so it clears the way `submit` always
                // has. `None` is defensive only: the invariant on
                // `error_kind` says it cannot happen alongside `state ==
                // Error`, but treating it as "nothing worth persisting"
                // rather than silently falling through to `Sink`/`Synth`'s
                // reject-forever behaviour keeps this from becoming stuck.
                Some(ErrorKind::Rejected) | None => {
                    // Error only ever arises with nothing legitimately in
                    // flight (see the branch below and `tick`'s
                    // synth-failure path), so there is no `current` to
                    // preserve here.
                    self.state = State::Idle;
                    self.error = None;
                    self.error_kind = None;
                }
            }
        }
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
                self.error_kind = Some(ErrorKind::Rejected);
            }
            return Err(msg);
        }

        // M21: reject an unknown voice synchronously, before it can ever
        // reach `tick`'s synth path and turn into a sticky `ErrorKind::Synth`
        // error there (see that variant's doc comment) -- one that persists
        // until an explicit dismiss and silently swallows every submission
        // behind it, including ones that never named a bad voice themselves.
        // `voice_exists` is a cheap, no-load check (`Synthesizer`'s doc
        // comment), so there is no reason not to run it before doing any
        // other work.
        //
        // Unlike the `max_chars` rejection above, this deliberately never
        // touches `state`/`error`/`error_kind`, even when the engine is
        // otherwise idle: nothing about the engine itself is wrong here --
        // only this one submission's voice name -- so nothing should make
        // `say status` report an error for it. The caller still learns the
        // submission was refused, via this method's own `Err`.
        let voice = opts.voice.as_deref().unwrap_or(&self.cfg.voice);
        if !self.synth.voice_exists(voice) {
            return Err(format!(
                "unknown voice '{voice}'; check the voices installed in the \
                 daemon's models directory (its voices/ subdirectory has one \
                 file per installed voice) and pick one of those"
            ));
        }

        if self.cfg.muted {
            return Ok(Submitted::Discarded);
        }

        let cleaned = clean(&text, &self.cfg.cleanup);
        if cleaned.trim().is_empty() {
            return Ok(Submitted::Discarded);
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

        Ok(Submitted::Queued(id))
    }

    /// One unit of work: top up the sink, or advance the queue, or unload.
    pub fn tick(&mut self) {
        // Checked before the `Paused` early-return, not after: the failure
        // this reports comes from cpal's stream error callback, which fires
        // on its own thread whenever the device dies, independent of
        // whether playback is paused. `push` cannot detect it -- it only
        // ever writes into the in-process ring, which keeps accepting
        // samples whether or not anything is left to drain them -- so this
        // poll is the only place a lost device is ever noticed. Deferring
        // it until a later `Resume` would leave a paused user believing
        // their queued speech is intact for however long they stay paused,
        // only to discover otherwise (and only then) on the next `tick`
        // after resuming; checking here surfaces it as soon as it happens
        // instead. The clear-the-queue behaviour is the same either way, so
        // this does not special-case `Paused` versus `Speaking` -- it just
        // stops the special-casing from mattering.
        if let Some(e) = self.sink.take_error() {
            self.state = State::Error;
            self.error = Some(e);
            self.error_kind = Some(ErrorKind::Sink);
            self.current = None;
            self.apply_pending_unload();
            self.queue.clear();
            // This is a route out of `Paused` (see the pause invariant on
            // `error`'s doc comment): a device failure can arrive while
            // paused, since this check runs before the `Paused` early
            // return below, so it must unpause the sink in the same step
            // rather than leaving it stranded for a `Resume` that can no
            // longer fire.
            self.sink.set_paused(false);
            return;
        }

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
                    self.error_kind = None;
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

        let Some(c) = self.current.as_mut() else {
            return;
        };
        if c.next_chunk >= c.chunks.len() {
            self.current = None;
            self.apply_pending_unload();
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
                self.error_kind = Some(ErrorKind::Synth);
                self.current = None;
                self.apply_pending_unload();
                self.queue.clear();
            }
        }
    }

    /// Swap in a fresh sink after a device failure, clearing the error.
    ///
    /// The daemon calls this when it manages to reacquire the audio device,
    /// which it now only attempts for a `Sink`-kind error (see `ErrorKind`)
    /// -- so in practice this is only ever called while `error_kind ==
    /// Some(ErrorKind::Sink)`. It still clears whatever error is present
    /// unconditionally rather than checking the kind itself: a fresh,
    /// working sink is a reasonable "start clean" point regardless of how
    /// it got here, and gating the *call site* in the daemon is enough to
    /// keep a `Synth` error from being cleared by an unrelated device
    /// reacquisition in the first place.
    ///
    /// The queue was cleared when the failure surfaced, so this returns the
    /// engine to a clean idle state rather than resuming a half-played
    /// utterance whose audio is gone.
    pub fn replace_sink(&mut self, sink: Box<dyn AudioSink>) {
        self.sink = sink;
        // Enforce the pause invariant by construction rather than trusting
        // the incoming sink to already be unpaused: both `AudioSink` impls
        // in this codebase happen to construct unpaused, but nothing about
        // the trait guarantees that of an arbitrary future implementation,
        // and this method always leaves `state == Idle`.
        self.sink.set_paused(false);
        self.current = None;
        self.apply_pending_unload();
        self.error = None;
        self.error_kind = None;
        self.state = State::Idle;
        self.idle_since = Some(Instant::now());
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
        self.error_kind = None;
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
        self.apply_pending_unload();
        self.sink.clear();
    }

    /// Unload a session that `ApplyConfig` deferred while the utterance now
    /// ending (or being discarded) was still in progress -- see the
    /// `ApplyConfig` handler for why it can't unload at that point. A no-op
    /// when nothing is pending, so every place `current` transitions to
    /// `None` can call this unconditionally instead of each having to
    /// re-check whether an unload is actually owed.
    fn apply_pending_unload(&mut self) {
        if self.pending_unload {
            self.pending_unload = false;
            self.synth.unload();
        }
    }

    fn maybe_unload(&mut self) {
        if !self.synth.is_loaded() {
            return;
        }
        // `0` disables idle unloading, per spec §8 ("Idle unload -- seconds,
        // 0 to disable") and §9. This early return is what makes that true:
        // without it the comparison below reads `elapsed() >= 0`, which
        // holds on the very first tick after the queue drains, so `0` would
        // mean the *most* aggressive unloading there is rather than none at
        // all. That is not a theoretical inversion -- it is one drag to the
        // bottom of the settings window's Idle unload spinner, whose own
        // subtitle promises the opposite, and the user who reaches for it is
        // by definition the one trying to get rid of the ~1.27 GB reload
        // pause before the first utterance.
        if self.cfg.idle_unload_secs == 0 {
            return;
        }
        let Some(since) = self.idle_since else { return };
        if since.elapsed() >= Duration::from_secs(self.cfg.idle_unload_secs) {
            self.synth.unload();
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        // Finding 1: report the utterance actually playing, not the
        // configured default -- a submission can override voice and speed
        // per utterance via `SayOpts` (see `submit`), and MPRIS's `Rate`/
        // `Metadata` (M3) read these fields directly. With nothing current,
        // the configured defaults are exactly right (that is what the next
        // utterance will use absent its own override).
        let (voice, speed) = match self.current.as_ref() {
            Some(c) => (c.voice.clone(), c.speed),
            None => (self.cfg.voice.clone(), self.cfg.speed),
        };
        // I2: `state` flips to `Speaking` synchronously in `submit` (and
        // stays there when `Next`/`SkipSentence` discard `current` while the
        // queue is still non-empty), but `current` itself is only populated
        // once `tick` reaches the front of the queue -- up to one synthesis
        // chunk later (measured 4.41-4.49s, constant across submission
        // sizes). Every consumer that reads `current_id`/`current_text`
        // during that gap -- the D-Bus control interface's `CurrentText`,
        // the tray's status line, and MPRIS's `Metadata`/`PlaybackStatus` --
        // otherwise shows nothing to describe an utterance that is, from
        // `state`'s point of view, already playing. For MPRIS specifically
        // that means `PlaybackStatus = Playing` alongside `mpris:trackid =
        // .../NoTrack`, which the MPRIS2 spec reserves for "no current
        // track" -- non-conformant while something genuinely is about to
        // play, and the reason waybar's mpris module shows a blank title for
        // the first few seconds of every utterance.
        //
        // Falling back to the queue head here -- the one place all three
        // consumers read from -- fixes all of them at once rather than
        // requiring each to reimplement (and risk disagreeing about) the
        // same fallback. Only while `state == Speaking`: with nothing
        // current and the queue empty too (e.g. `go_idle`'s "sink still
        // draining" case, or genuinely idle/paused/errored), there is
        // nothing to fall back to and `(0, "")` -- MPRIS's `NoTrack` case --
        // is the honest answer.
        let (current_id, current_text) = match self.current.as_ref() {
            Some(c) => (c.id, c.text.clone()),
            None if self.state == State::Speaking => match self.queue.iter().next() {
                Some(u) => (u.id, u.text.clone()),
                None => (0, String::new()),
            },
            None => (0, String::new()),
        };
        let queue_heads = self
            .queue
            .iter()
            .take(QUEUE_HEAD_LIMIT)
            .map(|u| (u.id, truncate_chars(&u.text, QUEUE_HEAD_TEXT_CHARS)))
            .collect();
        Snapshot {
            state: self.state,
            muted: self.cfg.muted,
            voice,
            speed,
            configured_speed: self.cfg.speed,
            queue_len: self.queue.len(),
            remaining_secs: self.remaining_secs(),
            current_text,
            current_id,
            queue_heads,
            error: self.error.clone(),
            error_kind: self.error_kind,
        }
    }

    /// The engine's live [`Config`], for a caller that needs what is
    /// actually configured rather than what is currently audible --
    /// `Snapshot::voice`/`Snapshot::speed` deliberately report the *current
    /// utterance's* overrides while one is playing (Finding 1), which is
    /// wrong for, say, a settings window's speed slider (I3: `EngineHandle::
    /// config` is the intended caller of this).
    ///
    /// Cheap: `Config` is small and this is a plain clone, no I/O -- unlike
    /// `Config::load()`, which would diverge from this in-memory copy the
    /// moment `SetMuted`/`SetVoice`/`SetSpeed` runs.
    pub fn config(&self) -> Config {
        self.cfg.clone()
    }

    /// Three buckets, in decreasing order of certainty: exact for audio
    /// already accepted by the sink, exact for audio already synthesized
    /// but still parked in `carry` waiting for room in the sink, and
    /// estimated (via `SECONDS_PER_WORD`) for text not yet spoken at all --
    /// the last bucket at each utterance's *own* speed (current or queued),
    /// not one engine-wide speed, since a submission can override it per
    /// utterance (Finding 1).
    fn remaining_secs(&self) -> f64 {
        let sr = self.synth.sample_rate() as f64;
        let carried = self.current.as_ref().map(|c| c.carry.len()).unwrap_or(0) as f64;
        let buffered = (self.sink.pending() as f64 + carried) / sr;

        let mut estimate = 0.0f64;
        if let Some(c) = self.current.as_ref() {
            let mut words = 0usize;
            for ch in &c.chunks[c.next_chunk.min(c.chunks.len())..] {
                words += ch.text.split_whitespace().count();
            }
            let speed = c.speed.max(0.1) as f64;
            estimate += (words as f64 * SECONDS_PER_WORD) / speed;
        }
        for u in self.queue.iter() {
            let words = u.text.split_whitespace().count();
            let speed = u.speed.max(0.1) as f64;
            estimate += (words as f64 * SECONDS_PER_WORD) / speed;
        }
        buffered + estimate
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

    /// Pretend the engine went idle `ago` earlier than it actually did, so a
    /// test can reach the far side of an idle-unload delay without sleeping
    /// through it.
    ///
    /// This exists because `idle_unload_secs: 0` -- the obvious shortcut,
    /// and what these tests used to use -- is no longer one: `0` now means
    /// *never* unload (see `maybe_unload`). A no-op while something is
    /// speaking, since `idle_since` is `None` then; that is precisely the
    /// state `model_does_not_unload_while_speaking` asserts about, so it
    /// backdates by an hour and still expects the model to be loaded.
    #[cfg(test)]
    fn backdate_idle(&mut self, ago: Duration) {
        self.idle_since = self
            .idle_since
            .map(|t| t.checked_sub(ago).expect("monotonic clock predates the test"));
    }

    #[cfg(test)]
    fn snapshot_queue_ids(&self) -> Vec<u64> {
        self.queue.iter().map(|u| u.id).collect()
    }

    #[cfg(test)]
    fn queued_utterance(&self, id: u64) -> Option<&Utterance> {
        self.queue.iter().find(|u| u.id == id)
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

    /// A sink whose `take_error` can be populated from outside, independent
    /// of `push` -- unlike `FailingSink` below, which only ever fails from
    /// inside `push` and therefore can never be triggered while the engine
    /// is `Paused` (`tick` never calls `push` while paused). The real
    /// `RingSink` fails this way for real: cpal reports a dead device from
    /// its own error-callback thread, asynchronously and independent of
    /// whether anything is currently being pushed. This wraps `VecSink` the
    /// same way `SharedVecSink` does, plus a second shared slot a test can
    /// write into directly to model that asynchronous arrival.
    #[derive(Clone)]
    struct FaultInjectableSink {
        inner: Arc<Mutex<VecSink>>,
        fault: Arc<Mutex<Option<String>>>,
    }

    impl FaultInjectableSink {
        fn new(capacity: usize) -> Self {
            FaultInjectableSink {
                inner: Arc::new(Mutex::new(VecSink::new(capacity))),
                fault: Arc::new(Mutex::new(None)),
            }
        }

        /// Simulate cpal's error callback firing on its own thread: make the
        /// next `take_error` observe a failure, with no `push` involved.
        fn inject_failure(&self, msg: &str) {
            *self.fault.lock().unwrap() = Some(msg.into());
        }
    }

    impl AudioSink for FaultInjectableSink {
        fn push(&mut self, samples: &[f32]) -> usize {
            self.inner.lock().unwrap().push(samples)
        }
        fn pending(&self) -> usize {
            self.inner.lock().unwrap().pending()
        }
        fn clear(&mut self) {
            self.inner.lock().unwrap().clear()
        }
        fn set_paused(&mut self, paused: bool) {
            self.inner.lock().unwrap().set_paused(paused)
        }
        fn is_paused(&self) -> bool {
            self.inner.lock().unwrap().is_paused()
        }
        fn capacity(&self) -> usize {
            self.inner.lock().unwrap().capacity()
        }
        fn total_written(&self) -> usize {
            self.inner.lock().unwrap().total_written()
        }
        fn take_error(&mut self) -> Option<String> {
            self.fault.lock().unwrap().take()
        }
    }

    fn say(text: &str) -> Command {
        Command::Say {
            text: text.into(),
            opts: SayOpts::default(),
        }
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
        assert!(
            pending > 0,
            "test is only meaningful with audio still buffered"
        );
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
        assert_eq!(
            e.snapshot().state,
            State::Idle,
            "must go Idle once the sink actually drains"
        );
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
        assert!(
            pending_before > 0,
            "test is only meaningful with audio still buffered"
        );

        e.handle(Command::Pause);
        assert_eq!(e.snapshot().state, State::Paused);

        for _ in 0..200 {
            e.tick();
        }

        let s = e.snapshot();
        assert_eq!(
            s.state,
            State::Paused,
            "must not spuriously become Idle while paused"
        );
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
        assert_eq!(
            s.queue_len, 0,
            "Stop is the shut-up verb: it clears everything"
        );
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
            opts: SayOpts {
                source: Source::Hotkey,
                ..Default::default()
            },
        });
        assert_eq!(
            e.snapshot().queue_len,
            1,
            "replace drops everything pending"
        );
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
        assert_eq!(
            e.snapshot().queue_len,
            2,
            "explicit enqueue beats the hotkey default"
        );
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
        let cfg = Config {
            max_chars: 10,
            ..Config::default()
        };
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
        let cfg = Config {
            max_chars: 10,
            ..Config::default()
        };
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
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
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
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
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
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
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
        assert_eq!(
            s.state,
            State::Error,
            "no command was issued; the error must persist"
        );
        assert!(s.error.is_some());
    }

    #[test]
    fn an_unknown_voice_is_rejected_synchronously_and_leaves_the_engine_idle() {
        // M21: an unknown voice must be caught at submission, not left to
        // surface later as a sticky `ErrorKind::Synth` error out of `tick`.
        // The check must not touch engine state at all -- unlike the
        // `max_chars` rejection, this must leave the engine idle and usable,
        // not `Error`, so `say status` reports nothing wrong.
        let mut e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::with_known_voices(["af_heart"])),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let result = e.submit(
            "hello there".into(),
            SayOpts {
                voice: Some("totally_bogus_name".into()),
                ..Default::default()
            },
        );
        let err = result.expect_err("an unknown voice must be rejected");
        assert!(
            err.contains("totally_bogus_name"),
            "the error must name the bad voice: {err}"
        );

        let s = e.snapshot();
        assert_eq!(
            s.state,
            State::Idle,
            "a bad voice must not poison the engine into Error"
        );
        assert_eq!(s.error, None);
        assert_eq!(s.queue_len, 0, "the rejected utterance must not be queued");

        // A following normal submission -- no voice override at all -- must
        // succeed, proving the engine was never actually wedged.
        let ok = e.submit("hello there".into(), SayOpts::default());
        assert!(
            matches!(ok, Ok(Submitted::Queued(_))),
            "a following plain submission must still work: {ok:?}"
        );
    }

    #[test]
    fn a_known_voice_is_still_accepted() {
        let mut e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::with_known_voices(["af_heart", "bf_emma"])),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let result = e.submit(
            "hello there".into(),
            SayOpts {
                voice: Some("bf_emma".into()),
                ..Default::default()
            },
        );
        assert!(matches!(result, Ok(Submitted::Queued(_))), "got {result:?}");
        assert_eq!(e.snapshot().state, State::Speaking);
    }

    #[test]
    fn a_bad_configured_default_voice_is_rejected_per_submission_without_wedging() {
        // The config-file-default half of M21: a bad *default* voice (no
        // per-utterance override at all) must hit the same synchronous
        // check as an explicit bad `--voice`, submission after submission,
        // rather than queuing once and then wedging the engine via a sticky
        // synth failure.
        let cfg = Config {
            voice: "totally_bogus_default".into(),
            ..Config::default()
        };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::with_known_voices(["af_heart"])),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let first = e.submit("hello there".into(), SayOpts::default());
        assert!(first.is_err(), "the bad default must be rejected");
        assert_eq!(e.snapshot().state, State::Idle, "must not wedge into Error");

        // Retrying with the same bad default fails the same way, not
        // differently -- there is nothing stuck to clear.
        let second = e.submit("hello again".into(), SayOpts::default());
        assert!(second.is_err());
        assert_eq!(e.snapshot().state, State::Idle);

        // But an explicit override to a known voice succeeds immediately.
        let third = e.submit(
            "hello there".into(),
            SayOpts {
                voice: Some("af_heart".into()),
                ..Default::default()
            },
        );
        assert!(matches!(third, Ok(Submitted::Queued(_))), "got {third:?}");
    }

    #[test]
    fn rejection_while_speaking_leaves_playback_untouched() {
        // Submitting an over-long text while an unrelated utterance A is
        // legitimately speaking must not flip the engine to Error: A is
        // unaffected and should play to completion. The rejection must
        // still be observable -- now synchronously, as `submit`'s `Err`.
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
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
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
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
                Command::ApplyConfig(Config::default()),
                Command::Shutdown,
            ]
        }

        fn build_idle() -> Engine {
            engine()
        }
        fn build_speaking() -> Engine {
            let mut e = engine();
            e.handle(say(
                "Hello there. This keeps it busy for quite a while indeed.",
            ));
            e.tick();
            e
        }
        fn build_paused() -> Engine {
            let mut e = build_speaking();
            e.handle(Command::Pause);
            e
        }
        fn build_error() -> Engine {
            let cfg = Config {
                max_chars: 5,
                ..Config::default()
            };
            let mut e = Engine::new(
                cfg,
                Box::new(StubSynthesizer::new()),
                Box::new(VecSink::new(24_000 * 10)),
            );
            e.handle(say("way too long for the limit"));
            e
        }
        // C2/M2's device-failure branch (`tick`'s `take_error` check) is a
        // second, independent route into `Error`, and the only one that can
        // fire while `state == Paused` -- exactly the case Finding 1 missed.
        // `all_commands()` has nothing that triggers `take_error` (nor could
        // it: the real failure arrives asynchronously from cpal's callback,
        // not from a `Command`), so a dedicated build function is the only
        // way to get this class of failure under the same sweep as every
        // other reachable state, rather than only the bespoke tests below.
        fn build_error_from_device_failure_while_paused() -> Engine {
            let sink = FaultInjectableSink::new(24_000 * 10);
            let mut e = Engine::new(
                Config::default(),
                Box::new(StubSynthesizer::new()),
                Box::new(sink.clone()),
            );
            e.handle(say(
                "Hello there. This keeps it busy for quite a while indeed.",
            ));
            e.tick();
            e.handle(Command::Pause);
            sink.inject_failure("audio device disappeared");
            e.tick();
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
        check_from(
            "device_failed_while_paused",
            build_error_from_device_failure_while_paused,
        );
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
        e.handle(say(
            "A reasonably long sentence to force a big carry remainder.",
        ));
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

    /// The whole point of `ApplyConfig`: a settings change is visible to the
    /// very next submission, without a restart and without a second copy of
    /// the config living anywhere.
    #[test]
    fn apply_config_changes_the_defaults_the_next_utterance_uses() {
        let mut e = engine();

        let cfg = Config {
            voice: "am_fenrir".into(),
            speed: 1.3,
            ..Config::default()
        };
        e.handle(Command::ApplyConfig(cfg));

        let snap = e.snapshot();
        assert_eq!(snap.voice, "am_fenrir");
        assert!((snap.configured_speed - 1.3).abs() < f32::EPSILON);

        // A submission that names neither must inherit both.
        let outcome = e
            .submit("hello".into(), SayOpts::default())
            .expect("accepted");
        let id = match outcome {
            Submitted::Queued(id) => id,
            other => panic!("expected Queued, got {other:?}"),
        };
        let u = e.queued_utterance(id).expect("queued");
        assert_eq!(u.voice, "am_fenrir");
        assert!((u.speed - 1.3).abs() < f32::EPSILON);
    }

    /// Speed is clamped on this path exactly as it is on `SetSpeed`. A
    /// hand-edited config file is an untrusted input.
    #[test]
    fn apply_config_clamps_speed_the_same_way_set_speed_does() {
        let mut e = engine();

        let fast = Config {
            speed: 9.0,
            ..Config::default()
        };
        e.handle(Command::ApplyConfig(fast));
        assert!((e.snapshot().configured_speed - 2.0).abs() < f32::EPSILON);

        let slow = Config {
            speed: 0.01,
            ..Config::default()
        };
        e.handle(Command::ApplyConfig(slow));
        assert!((e.snapshot().configured_speed - 0.5).abs() < f32::EPSILON);
    }

    /// A model or thread-count change has to reach the synthesizer
    /// eventually, and a voice-only change must never touch a loaded model
    /// at all (reloading costs ~1.27 GB and over a second of latency) --
    /// but IMPORTANT 1: while an utterance is actually in progress, even a
    /// real model change must not drop the session out from under it. The
    /// brief's own rationale was "the next utterance picks up the new
    /// settings", which this test now pins literally: the session the
    /// current utterance started on survives until that utterance finishes,
    /// and only then does the deferred unload happen, in time for whatever
    /// comes next.
    ///
    /// Also pins IMPORTANT 2, the brief's headline guarantee that
    /// `ApplyConfig` touches nothing besides `cfg` and the synthesizer: an
    /// implementation that routed a model change through
    /// `dismiss_error_and_go_idle()` -- exactly what the neighbouring
    /// `SetMuted(true)` arm does -- would satisfy every assertion above
    /// while silently breaking `state`, `error`, `current_id` and the
    /// queue, so all four are checked unchanged across the `ApplyConfig`
    /// that lands mid-utterance.
    #[test]
    fn a_model_change_defers_the_unload_until_the_utterance_in_progress_finishes() {
        // Small `target_chars` so the utterance below splits into more than
        // one chunk (same technique as `skip_sentence_stops_current_audio_
        // promptly`) -- otherwise a single tick synthesizes the whole
        // utterance in one shot and there is no "mid-utterance" moment left
        // to test.
        let mut cfg = Config::default();
        cfg.chunking.target_chars = 25;
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );

        e.handle(say("First sentence here. Second sentence here."));
        e.tick(); // pops the utterance into `current` and synthesizes chunk 1 of 2
        assert!(
            e.is_model_loaded(),
            "the stub should be loaded after speaking"
        );
        // A second utterance behind it, so the queue-length assertion below
        // actually exercises something (an empty queue staying empty would
        // pass trivially).
        e.handle(say("A second utterance queued behind it."));

        let before = e.snapshot();
        assert_eq!(before.state, State::Speaking);
        let current_id = before.current_id;
        assert_ne!(current_id, 0, "test needs a genuine in-progress utterance");
        let queue_len = before.queue_len;
        assert_eq!(queue_len, 1, "sanity: the second utterance is queued");
        let queued_id = before
            .queue_heads
            .first()
            .map(|(id, _)| *id)
            .expect("sanity: the second utterance is queued");

        let voice_only = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        e.handle(Command::ApplyConfig(voice_only));
        assert!(
            e.is_model_loaded(),
            "a voice change must not drop the model"
        );

        let new_model = Config {
            voice: "am_fenrir".into(),
            model: "q8".into(),
            ..Config::default()
        };
        e.handle(Command::ApplyConfig(new_model));

        // IMPORTANT 1: chunk 2 of this utterance has not synthesized yet --
        // still mid-utterance -- so the live session must survive.
        assert!(
            e.is_model_loaded(),
            "a model change must not drop a session still in use \
             mid-utterance"
        );
        // IMPORTANT 2: ApplyConfig touches only `cfg` and the synthesizer.
        let after = e.snapshot();
        assert_eq!(
            after.state,
            State::Speaking,
            "ApplyConfig must not touch state"
        );
        assert_eq!(after.error, None, "ApplyConfig must not touch error");
        assert_eq!(
            after.current_id, current_id,
            "ApplyConfig must not touch the utterance in progress"
        );
        assert_eq!(
            after.queue_len, queue_len,
            "ApplyConfig must not touch the queue"
        );

        // Cancel the queued second utterance before driving to completion:
        // this plain `VecSink` is never drained, so `sink.pending() > 0`
        // holds forever and `go_idle` never lets `state` reach `Idle` (see
        // `state_stays_speaking_while_audio_is_still_pending_in_the_sink`)
        // -- `run`'s Idle-seeking exit condition would just burn its whole
        // budget regardless of what's queued. With nothing left to pop once
        // `current` clears, `current_id` resetting to 0 is a bound-agnostic
        // signal that the deferred boundary was actually crossed, instead
        // of one that depends on how many ticks a second utterance happens
        // to need.
        e.handle(Command::Cancel(queued_id));

        // Drive the utterance to completion; only once `current` actually
        // clears -- the deferred boundary -- does the unload happen. Bounded
        // like every other multi-tick loop in this file (see `run` and the
        // `for _ in 0..50` loops below): an unbounded `while current_id ==
        // ..` here would hang the whole suite, rather than just fail this
        // test, the moment some future change stopped `current_id` from
        // ever advancing -- so the exit condition is asserted explicitly
        // instead of being baked into the loop.
        run(&mut e, 50);
        assert_ne!(
            e.snapshot().current_id, current_id,
            "test needs the utterance in progress to actually finish within \
             the tick budget, not just run out of ticks"
        );
        assert!(
            !e.is_model_loaded(),
            "the deferred model change must take effect once the \
             utterance in progress finishes"
        );
    }

    /// Build an engine with a session loaded and a model change already
    /// deferred, mid-utterance -- the setup every test below shares with
    /// `a_model_change_defers_the_unload_until_the_utterance_in_progress_
    /// finishes`, factored out because each of those tests only cares about
    /// *one* of the five other call sites `apply_pending_unload` (see
    /// `pending_unload`'s doc comment) has to reach it from, not about
    /// re-deriving this state each time.
    ///
    /// After this returns: `current` is `Some` (chunk 1 of 2 already
    /// synthesized), `pending_unload` is `true`, and the stub is still
    /// loaded on the *old* model -- exactly the state a reviewer found
    /// untested at five of those six sites.
    fn mid_utterance_with_deferred_unload(sink: Box<dyn AudioSink>) -> Engine {
        let mut cfg = Config::default();
        cfg.chunking.target_chars = 25;
        let mut e = Engine::new(cfg, Box::new(StubSynthesizer::new()), sink);

        e.handle(say("First sentence here. Second sentence here."));
        e.tick(); // pops the utterance into `current`, synthesizes chunk 1 of 2
        assert!(
            e.is_model_loaded(),
            "setup: the stub should be loaded after speaking"
        );

        let new_model = Config {
            model: "q8".into(),
            ..Config::default()
        };
        e.handle(Command::ApplyConfig(new_model));
        assert!(
            e.is_model_loaded(),
            "setup: a model change must defer, not drop, a session still in \
             use mid-utterance"
        );
        e
    }

    /// `Stop` discards `current` through `discard_current` (`engine.rs:
    /// 306` -> `:763`). If that call to `apply_pending_unload` were
    /// deleted, `pending_unload` would stay `true` across the `Stop` and
    /// the *next* utterance would synthesize on the old, still-loaded
    /// model -- the daemon would speak with the model the user just
    /// switched away from, once, silently, self-healing only when that
    /// utterance ends.
    #[test]
    fn stop_unloads_a_deferred_model_change() {
        let mut e = mid_utterance_with_deferred_unload(Box::new(VecSink::new(24_000 * 10)));
        e.handle(Command::Stop);
        assert!(
            !e.is_model_loaded(),
            "Stop must apply the deferred unload via discard_current"
        );
    }

    /// `Next` discards `current` through the same `discard_current` ->
    /// `apply_pending_unload` call as `Stop` (`engine.rs:310` -> `:763`),
    /// but is a distinct call *site* in `handle`'s match arm -- see
    /// `stop_unloads_a_deferred_model_change` for the failure this pins.
    #[test]
    fn next_unloads_a_deferred_model_change() {
        let mut e = mid_utterance_with_deferred_unload(Box::new(VecSink::new(24_000 * 10)));
        e.handle(Command::Next);
        assert!(
            !e.is_model_loaded(),
            "Next must apply the deferred unload via discard_current"
        );
    }

    /// `SetMuted(true)` also routes through `discard_current` (`engine.rs:
    /// 340` -> `:763`) -- see `stop_unloads_a_deferred_model_change` for the
    /// failure this pins.
    #[test]
    fn set_muted_true_unloads_a_deferred_model_change() {
        let mut e = mid_utterance_with_deferred_unload(Box::new(VecSink::new(24_000 * 10)));
        e.handle(Command::SetMuted(true));
        assert!(
            !e.is_model_loaded(),
            "SetMuted(true) must apply the deferred unload via discard_current"
        );
    }

    /// A `Replace`/`Interrupt` submission blows away whatever is current
    /// through the same `discard_current` (`engine.rs:509` -> `:763`) --
    /// see `stop_unloads_a_deferred_model_change` for the failure this
    /// pins. `Policy::Replace` is forced explicitly here (rather than via a
    /// source's default policy) so the test exercises exactly this branch
    /// regardless of `Source::default_policy`'s own mapping.
    #[test]
    fn a_replacing_submission_unloads_a_deferred_model_change() {
        let mut e = mid_utterance_with_deferred_unload(Box::new(VecSink::new(24_000 * 10)));
        e.handle(Command::Say {
            text: "Replacement, arriving mid-utterance.".into(),
            opts: SayOpts {
                policy: Some(Policy::Replace),
                ..Default::default()
            },
        });
        assert!(
            !e.is_model_loaded(),
            "a Replace/Interrupt submission must apply the deferred unload \
             via discard_current"
        );
    }

    /// `SkipSentence` has its own, separate `apply_pending_unload` call
    /// (`engine.rs:320-321`) for the one tick-wide window where every chunk
    /// has already been dispatched to the synthesizer (`next_chunk ==
    /// chunks.len()`) but `current` has not yet been cleared -- that only
    /// happens on `tick`'s *next* call, at the natural-completion site
    /// (`:635-636`). Skipping in that window must not wait for that next
    /// tick to apply a deferred unload; it has to do it itself. Deleting
    /// `:321` leaves the model loaded here even though `tick`'s own
    /// call at `:636` would still (eventually) cover most other cases,
    /// which is exactly why the reviewer found this one silently uncovered.
    #[test]
    fn skip_sentence_past_the_last_chunk_unloads_a_deferred_model_change() {
        let mut e = mid_utterance_with_deferred_unload(Box::new(VecSink::new(24_000 * 10)));
        // Dispatches chunk 2 of 2: next_chunk becomes == chunks.len(), but
        // `current` stays `Some` until the *next* tick notices. Calling
        // SkipSentence right here is the only way to reach its own
        // apply_pending_unload call instead of tick's.
        e.tick();
        e.handle(Command::SkipSentence);
        assert!(
            !e.is_model_loaded(),
            "SkipSentence past the last chunk must apply the deferred \
             unload itself, not rely on tick's natural-completion path"
        );
    }

    /// `tick`'s synth-error branch has its own `apply_pending_unload` call
    /// (`engine.rs:662-663`), separate from the natural-completion one a
    /// few lines above it. `FlakySynthesizer` succeeds once (so there is a
    /// session to defer-unload in the first place, unlike `FailingSynth`
    /// below which never loads at all) and fails on the second call,
    /// modelling a model that loads fine but breaks mid-article -- e.g. a
    /// corrupt weight file only `ort` notices once asked to actually run
    /// it.
    #[test]
    fn a_synth_failure_mid_utterance_unloads_a_deferred_model_change() {
        struct FlakySynthesizer {
            calls: usize,
            loaded: bool,
            reconfigured_to: (String, usize),
        }
        impl FlakySynthesizer {
            fn new() -> Self {
                let d = crate::config::Config::default();
                FlakySynthesizer {
                    calls: 0,
                    loaded: false,
                    reconfigured_to: (d.model, d.threads),
                }
            }
        }
        impl crate::synth::Synthesizer for FlakySynthesizer {
            fn phonemize(&mut self, t: &str, _voice: &str) -> String {
                t.to_lowercase()
            }
            fn fits(&mut self, _: &str) -> bool {
                true
            }
            fn synth(
                &mut self,
                phonemes: &str,
                _voice: &str,
                speed: f32,
            ) -> Result<Vec<f32>, String> {
                self.calls += 1;
                if self.calls == 1 {
                    self.loaded = true;
                    let per_char = 24_000.0f32 * 0.08;
                    let n =
                        ((phonemes.chars().count() as f32 * per_char) / speed.max(0.1)) as usize;
                    Ok(vec![0.0; n])
                } else {
                    Err("model exploded mid-utterance".into())
                }
            }
            fn unload(&mut self) {
                self.loaded = false;
            }
            fn is_loaded(&self) -> bool {
                self.loaded
            }
            fn reconfigure(&mut self, cfg: &crate::config::Config) -> bool {
                let next = (cfg.model.clone(), cfg.threads);
                let changed = self.reconfigured_to != next;
                self.reconfigured_to = next;
                changed
            }
        }

        let mut cfg = Config::default();
        cfg.chunking.target_chars = 25;
        let mut e = Engine::new(
            cfg,
            Box::new(FlakySynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("First sentence here. Second sentence here."));
        e.tick(); // chunk 1 of 2 succeeds
        assert!(
            e.is_model_loaded(),
            "setup: the flaky synth should be loaded after its first call"
        );

        let new_model = Config {
            model: "q8".into(),
            ..Config::default()
        };
        e.handle(Command::ApplyConfig(new_model));
        assert!(
            e.is_model_loaded(),
            "setup: a model change must defer while mid-utterance"
        );

        e.tick(); // chunk 2 of 2 fails
        assert_eq!(
            e.snapshot().state,
            State::Error,
            "setup: the synth failure must actually surface"
        );
        assert!(
            !e.is_model_loaded(),
            "a synth failure mid-utterance must apply the deferred unload"
        );
    }

    /// `replace_sink` has its own `apply_pending_unload` call (`engine.rs:
    /// 692-693`): it always discards whatever utterance was in progress
    /// (its own doc comment: "returns the engine to a clean idle state
    /// rather than resuming a half-played utterance whose audio is gone"),
    /// so a pending unload from an `ApplyConfig` that arrived just before a
    /// device failure must not survive the sink swap either.
    #[test]
    fn replace_sink_unloads_a_deferred_model_change() {
        let mut e = mid_utterance_with_deferred_unload(Box::new(VecSink::new(24_000 * 10)));
        e.replace_sink(Box::new(VecSink::new(24_000 * 10)));
        assert!(
            !e.is_model_loaded(),
            "replace_sink must apply the deferred unload"
        );
    }

    /// `tick`'s device-failure branch (`self.sink.take_error()`) has its
    /// own `apply_pending_unload` call (`engine.rs:541-542`), reached
    /// before the `Paused` early return and independent of the
    /// natural-completion and synth-error branches below it. Modelled with
    /// `FaultInjectableSink`, the same double used by
    /// `device_failure_while_paused_unpauses_the_sink_and_reaches_error`,
    /// since a real device failure arrives asynchronously from cpal's
    /// callback rather than from anything `push` observes.
    #[test]
    fn a_device_failure_mid_utterance_unloads_a_deferred_model_change() {
        let sink = FaultInjectableSink::new(24_000 * 10);
        let mut e = mid_utterance_with_deferred_unload(Box::new(sink.clone()));
        sink.inject_failure("audio device disappeared");
        e.tick();
        assert_eq!(
            e.snapshot().state,
            State::Error,
            "setup: the injected device failure must actually surface"
        );
        assert!(
            !e.is_model_loaded(),
            "tick's device-failure branch must apply the deferred unload"
        );
    }

    #[test]
    fn current_utterance_reports_its_own_overridden_voice_and_speed_and_reverts_when_idle() {
        // Finding 1, measured over D-Bus: `Say(text, {voice: "bf_emma"})`
        // spoke in a British voice while `Voice` kept reporting the
        // configured default, and `{speed: 2.0}` left `Speed`/
        // `RemainingSeconds` unchanged. MPRIS's `Rate`/`Metadata` (M3) read
        // these fields directly, so the snapshot must reflect what is
        // actually playing, not `self.cfg`.
        let (mut e, sink) = engine_with_drainable_sink(24_000 * 10);
        e.handle(Command::Say {
            text: "Hello there, this utterance overrides both.".into(),
            opts: SayOpts {
                voice: Some("bf_emma".into()),
                speed: Some(1.8),
                ..Default::default()
            },
        });
        // One tick pops the utterance into `current` and synthesizes its
        // (only, given how short this text is) chunk -- deliberately not
        // more: further ticks would notice `next_chunk >= chunks.len()`,
        // clear `current` and (with a big enough sink) go straight to
        // `Idle`, defeating a test about what the snapshot says *while*
        // this utterance is current.
        e.tick();
        let s = e.snapshot();
        assert_eq!(
            s.state,
            State::Speaking,
            "test is only meaningful while this utterance is current"
        );
        assert_ne!(
            s.current_id, 0,
            "test is only meaningful while this utterance is current"
        );
        assert_eq!(
            s.voice, "bf_emma",
            "must report the utterance's own voice while it plays"
        );
        assert_eq!(
            s.speed, 1.8,
            "must report the utterance's own speed while it plays"
        );

        // Drain the sink first so the following tick sees nothing left to
        // play and can actually reach Idle, not just clear `current` while
        // still draining (see `Engine::go_idle`).
        sink.lock().unwrap().drain(usize::MAX);
        e.tick();
        let idle = e.snapshot();
        assert_eq!(idle.state, State::Idle);
        assert_eq!(
            idle.voice,
            Config::default().voice,
            "must revert to the configured default once nothing is current"
        );
        assert_eq!(
            idle.speed,
            Config::default().speed,
            "must revert to the configured default once nothing is current"
        );
    }

    #[test]
    fn current_id_and_text_fall_back_to_the_queue_head_before_tick_populates_current() {
        // I2: right after `submit`, `state` is already `Speaking` but
        // `current` is still `None` -- `tick` has not run yet. Every
        // consumer of `current_id`/`current_text` (D-Bus `CurrentText`, the
        // tray, MPRIS's `Metadata`) must see the utterance that is about to
        // play during that gap, not a placeholder.
        let mut e = engine();
        e.handle(say("The utterance about to play."));
        // Deliberately no `e.tick()` here: this is exactly the gap I2
        // covers -- `submit` has already run (flipping `state`), but the
        // engine has not ticked, so `current` is genuinely still `None`.
        let s = e.snapshot();
        assert_eq!(
            s.state,
            State::Speaking,
            "submit must flip state synchronously"
        );
        assert_ne!(
            s.current_id, 0,
            "current_id must fall back to the queued utterance's id, not 0/NoTrack"
        );
        assert_eq!(s.current_text, "The utterance about to play.");
    }

    #[test]
    fn current_id_and_text_stay_at_the_no_track_sentinel_when_genuinely_idle() {
        let e = engine();
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle);
        assert_eq!(
            s.current_id, 0,
            "nothing queued or playing: no fallback to give"
        );
        assert_eq!(s.current_text, "");
    }

    #[test]
    fn current_id_and_text_fall_back_correctly_after_next_discards_current() {
        // The other route into "Speaking with current == None": `Next`
        // discards `current` but leaves `state` alone while the queue is
        // still non-empty (see `Command::Next`'s handling).
        let mut e = engine();
        e.handle(say("first"));
        e.handle(say("second"));
        e.tick(); // "first" becomes current
        e.handle(Command::Next);
        let s = e.snapshot();
        assert_eq!(s.state, State::Speaking);
        assert_eq!(
            s.current_text, "second",
            "must show what is now queued next"
        );
    }

    #[test]
    fn configured_speed_reports_the_config_default_even_while_an_override_plays() {
        // I1: unlike `speed` (which deliberately tracks the current
        // utterance's own override -- Finding 1), `configured_speed` must
        // always report `cfg.speed`, since MPRIS's `Rate` needs to reflect
        // what `SetSpeed` controls, not what happens to be playing.
        let mut e = engine();
        e.handle(Command::Say {
            text: "Hello there, this utterance overrides its speed.".into(),
            opts: SayOpts {
                speed: Some(1.8),
                ..Default::default()
            },
        });
        e.tick();
        let s = e.snapshot();
        assert_eq!(s.state, State::Speaking);
        assert_eq!(
            s.speed, 1.8,
            "sanity: the per-utterance override is audible"
        );
        assert_eq!(
            s.configured_speed,
            Config::default().speed,
            "configured_speed must stay at the config default, unaffected by the override"
        );
    }

    #[test]
    fn configured_speed_updates_immediately_on_set_speed_even_while_speaking() {
        let mut e = engine();
        e.handle(say(
            "Something reasonably long to keep this busy for a bit.",
        ));
        e.tick();
        assert_eq!(e.snapshot().state, State::Speaking);
        e.handle(Command::SetSpeed(1.75));
        assert_eq!(
            e.snapshot().configured_speed,
            1.75,
            "SetSpeed must be visible on configured_speed right away, not only \
             once the current utterance finishes"
        );
    }

    #[test]
    fn remaining_seconds_halves_when_the_current_utterances_own_speed_is_doubled() {
        // Companion to `remaining_seconds_halves_at_double_speed` above,
        // which pins this for a *queued* utterance via `cfg.speed`
        // (`SetSpeed`). This pins the same requirement for the *current*
        // utterance's own per-submission override (`SayOpts::speed`) --
        // both `cfg.speed` for both engines stay at the 1.0 default here, so
        // this only passes if `remaining_secs` is actually reading the
        // current utterance's own speed rather than the engine-wide config.
        let mut cfg = Config::default();
        cfg.chunking.target_chars = 20; // force several chunks so words remain uncounted after one tick
        let text = "One two three four five. Six seven eight nine ten. \
                     Eleven twelve thirteen fourteen fifteen.";

        let mut fast = Engine::new(
            cfg.clone(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        fast.handle(Command::Say {
            text: text.into(),
            opts: SayOpts {
                speed: Some(2.0),
                ..Default::default()
            },
        });
        fast.tick(); // pop into current; synthesizes only the first chunk
        let fast_secs = fast.snapshot().remaining_secs;

        let mut normal = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        normal.handle(say(text));
        normal.tick();
        let normal_secs = normal.snapshot().remaining_secs;

        assert!(
            fast_secs < normal_secs * 0.75,
            "fast {fast_secs} vs normal {normal_secs}"
        );
    }

    #[test]
    fn queue_heads_reports_the_first_few_pending_utterances_in_order() {
        let mut e = engine();
        e.handle(say("current"));
        e.handle(say("queued one"));
        e.handle(say("queued two"));
        e.handle(say("queued three"));
        e.tick(); // "current" leaves the queue, three remain

        let s = e.snapshot();
        assert_eq!(
            s.queue_len, 3,
            "queue_len must still count everything pending"
        );
        let texts: Vec<&str> = s.queue_heads.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            texts,
            vec!["queued one", "queued two", "queued three"],
            "heads must appear in play order"
        );
        let ids: Vec<u64> = s.queue_heads.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            e.snapshot_queue_ids(),
            "head ids must match the real queue, in order"
        );
    }

    #[test]
    fn queue_heads_truncates_to_the_head_limit_while_queue_len_counts_every_pending_utterance() {
        let mut e = engine();
        e.handle(say("current"));
        let total_queued = QUEUE_HEAD_LIMIT + 3;
        for i in 0..total_queued {
            e.handle(say(&format!("queued {i}")));
        }
        e.tick(); // "current" leaves the queue

        let s = e.snapshot();
        assert_eq!(
            s.queue_len, total_queued,
            "a queue longer than the head limit must still report its true size"
        );
        assert_eq!(
            s.queue_heads.len(),
            QUEUE_HEAD_LIMIT,
            "the head list must be capped even though more are queued"
        );
        for (i, (_, text)) in s.queue_heads.iter().enumerate() {
            assert_eq!(text, &format!("queued {i}"));
        }
    }

    #[test]
    fn multi_byte_queued_text_does_not_panic_when_truncated_for_queue_heads() {
        // Finding 2: truncation must land on a character boundary. Slicing
        // at a fixed byte offset would panic here, since none of this
        // text's multi-byte characters happen to end exactly at
        // `QUEUE_HEAD_TEXT_CHARS` bytes in.
        let mut e = engine();
        e.handle(say("current"));
        let long_multi_byte = "日本語のテキストです。".repeat(10);
        assert!(
            long_multi_byte.chars().count() > QUEUE_HEAD_TEXT_CHARS,
            "test needs text longer than the truncation limit"
        );
        e.handle(say(&long_multi_byte));
        e.tick(); // "current" leaves the queue

        let s = e.snapshot(); // must not panic
        assert_eq!(s.queue_heads.len(), 1);
        assert!(
            s.queue_heads[0].1.chars().count() <= QUEUE_HEAD_TEXT_CHARS + 1,
            "truncated text (plus a possible ellipsis) must not exceed the limit"
        );
    }

    #[test]
    fn truncate_chars_is_a_no_op_under_the_limit() {
        assert_eq!(truncate_chars("short", 60), "short");
    }

    #[test]
    fn truncate_chars_cuts_multi_byte_text_on_a_character_boundary() {
        let s = "日本語".repeat(5); // 15 chars, 3 bytes each
        let truncated = truncate_chars(&s, 4);
        assert_eq!(truncated.chars().count(), 5); // 4 chars + the appended ellipsis
        assert!(truncated.ends_with('\u{2026}'));
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
        // A real, non-zero delay, reached with `backdate_idle` rather than
        // by sleeping it out. This test used `idle_unload_secs: 0` as a
        // "fire immediately" convenience until `0` was given its documented
        // meaning of *never* unload; using it here would now assert the
        // opposite of what the name says.
        //
        // C1: as in `returns_to_idle_when_the_queue_empties`, actually
        // reaching `Idle` -- the precondition this test is exercising --
        // requires draining the sink first.
        let cfg = Config {
            idle_unload_secs: 600,
            ..Config::default()
        };
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
        e.tick(); // reach Idle, which starts the idle clock
        assert_eq!(e.snapshot().state, State::Idle);
        assert!(
            e.is_model_loaded(),
            "the delay has not elapsed yet, so the model must still be loaded"
        );

        e.backdate_idle(Duration::from_secs(600));
        e.tick();
        assert!(
            !e.is_model_loaded(),
            "expected the model to unload once the delay elapsed"
        );
    }

    /// Spec §8 and §9: "Idle unload -- seconds, 0 to disable", which the
    /// settings window repeats to the user as "0 never unloads". `0` is the
    /// bottom of that spinner's range and one drag away, so getting it
    /// backwards turns the control a user reaches for to *avoid* reload
    /// pauses into the one that guarantees one before every utterance.
    #[test]
    fn idle_unload_of_zero_never_unloads() {
        let cfg = Config {
            idle_unload_secs: 0,
            ..Config::default()
        };
        let sink = Arc::new(Mutex::new(VecSink::new(24_000 * 10)));
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(SharedVecSink(sink.clone())),
        );
        e.handle(say("Hello."));
        run(&mut e, 500);
        sink.lock().unwrap().drain(usize::MAX);
        e.tick();
        assert_eq!(e.snapshot().state, State::Idle);

        // A week idle. Nothing short of an explicit config change may drop
        // the session.
        e.backdate_idle(Duration::from_secs(7 * 24 * 3600));
        e.tick();
        assert!(
            e.is_model_loaded(),
            "idle_unload_secs = 0 must keep the session loaded indefinitely"
        );
    }

    #[test]
    fn model_does_not_unload_while_speaking() {
        let cfg = Config {
            idle_unload_secs: 1,
            ..Config::default()
        };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say(
            "A reasonably long sentence to keep it busy for a while.",
        ));
        e.tick();
        // A no-op by construction: `idle_since` is `None` while an utterance
        // is in flight, which is exactly the guard being tested. Backdating
        // by an hour and still finding the model loaded says the unload
        // depends on being idle, not merely on the delay having passed.
        e.backdate_idle(Duration::from_secs(3600));
        e.tick();
        assert_eq!(e.snapshot().state, State::Speaking, "sanity: still busy");
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
        e.handle(say(
            "First sentence here. Second sentence here. Third one here.",
        ));
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
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
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
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
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
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
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
        assert_eq!(
            s.state,
            State::Speaking,
            "the unrelated playback must continue"
        );
        assert_eq!(s.error, None);
    }

    #[test]
    fn submit_accepted_returns_the_id_that_later_appears_as_current_id() {
        let mut e = engine();
        let outcome = e
            .submit("Hello there. This is a test.".into(), SayOpts::default())
            .expect("well-formed text must be accepted");
        let id = match outcome {
            Submitted::Queued(id) => id,
            other => panic!("well-formed text must be queued, got {other:?}"),
        };
        e.tick();
        assert_eq!(e.snapshot().current_id, id);
    }

    #[test]
    fn submit_returns_discarded_when_muted() {
        let mut e = engine();
        e.handle(Command::SetMuted(true));
        assert_eq!(
            e.submit("nobody hears this".into(), SayOpts::default()),
            Ok(Submitted::Discarded)
        );
    }

    #[test]
    fn submit_returns_discarded_for_text_that_is_empty_after_cleanup() {
        let mut e = engine();
        assert_eq!(
            e.submit("   ".into(), SayOpts::default()),
            Ok(Submitted::Discarded)
        );
    }

    #[test]
    fn submit_returns_a_nonzero_id_when_queued() {
        let mut e = engine();
        let outcome = e
            .submit("hello there.".into(), SayOpts::default())
            .expect("accepted");
        match outcome {
            Submitted::Queued(id) => {
                assert_ne!(id, 0, "id 0 is the nothing-is-playing sentinel")
            }
            other => panic!("expected Queued, got {other:?}"),
        }
    }

    #[test]
    fn submit_still_returns_err_when_rejected() {
        let mut e = Engine::new(
            Config {
                max_chars: 5,
                ..Config::default()
            },
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        );
        assert!(e.submit("far too long".into(), SayOpts::default()).is_err());
    }

    /// A sink that reports a device failure after accepting one push.
    struct FailingSink {
        accepted_once: bool,
        err: Option<String>,
        paused: bool,
    }

    impl FailingSink {
        fn new() -> Self {
            FailingSink {
                accepted_once: false,
                err: None,
                paused: false,
            }
        }
    }

    impl crate::audio::AudioSink for FailingSink {
        fn push(&mut self, samples: &[f32]) -> usize {
            if self.accepted_once {
                self.err = Some("audio device disappeared".into());
                return 0;
            }
            self.accepted_once = true;
            samples.len()
        }
        fn pending(&self) -> usize {
            0
        }
        fn clear(&mut self) {}
        fn set_paused(&mut self, p: bool) {
            self.paused = p
        }
        fn is_paused(&self) -> bool {
            self.paused
        }
        fn capacity(&self) -> usize {
            24_000
        }
        fn total_written(&self) -> usize {
            0
        }
        fn take_error(&mut self) -> Option<String> {
            self.err.take()
        }
    }

    /// `FailingSink` accepts exactly one `push` and fails every one after
    /// that, so the engine needs at least two synthesis chunks to ever see
    /// the failure -- one to consume the free pass, one to hit it. `chunk()`
    /// merges short multi-sentence input (like "one. two. three.") into a
    /// single chunk under the default 400-char `target_chars`, which would
    /// only ever call `push` once and never trigger the failure at all.
    /// This text is long enough to force a second chunk.
    fn text_spanning_multiple_chunks() -> String {
        "This is one sentence in a long batch of text. ".repeat(15)
    }

    #[test]
    fn a_device_failure_surfaces_as_error_rather_than_wedging() {
        let mut e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(FailingSink::new()),
        );
        e.submit(text_spanning_multiple_chunks(), SayOpts::default())
            .expect("accepted");
        for _ in 0..200 {
            e.tick();
        }
        let s = e.snapshot();
        assert_eq!(
            s.state,
            State::Error,
            "a dead device must not leave the engine Speaking"
        );
        assert!(s.error.as_deref().unwrap_or("").contains("device"));
    }

    #[test]
    fn replace_sink_clears_the_error_and_accepts_new_work() {
        let mut e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(FailingSink::new()),
        );
        e.submit(text_spanning_multiple_chunks(), SayOpts::default())
            .expect("accepted");
        for _ in 0..200 {
            e.tick();
        }
        assert_eq!(e.snapshot().state, State::Error);

        e.replace_sink(Box::new(VecSink::new(24_000 * 10)));
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle, "a fresh sink clears the failure");
        assert_eq!(s.error, None);

        e.submit("after recovery.".into(), SayOpts::default())
            .expect("accepted");
        for _ in 0..500 {
            e.tick();
        }
        assert!(
            e.audio_written() > 0,
            "the engine must work again after the sink is replaced"
        );
    }

    #[test]
    fn device_failure_while_paused_unpauses_the_sink_and_reaches_error() {
        // Finding 1: `tick`'s device-failure branch is a route out of
        // `Paused` (`Paused -> Error`) and must unpause the sink in the same
        // step, exactly like `dismiss_error_and_go_idle` already does for
        // its own routes -- otherwise a later `Resume` has no way left to
        // fire (it's gated on `state == Paused`, which is no longer true)
        // and the sink is stranded paused forever. `FailingSink` cannot
        // reproduce this: it only ever fails from inside `push`, and `tick`
        // never calls `push` while paused. `FaultInjectableSink` can, since
        // its failure is set from outside, matching how the real `RingSink`
        // reports a device failure asynchronously from cpal's error
        // callback, independent of whether anything is being pushed.
        let sink = FaultInjectableSink::new(24_000 * 10);
        let mut e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(sink.clone()),
        );
        e.handle(say(
            "Hello there. This keeps it busy for quite a while indeed.",
        ));
        e.tick();
        e.handle(Command::Pause);
        assert_eq!(e.snapshot().state, State::Paused);
        assert!(sink.is_paused());

        sink.inject_failure("audio device disappeared");
        e.tick();

        let s = e.snapshot();
        assert_eq!(
            s.state,
            State::Error,
            "a device failure must surface even while paused"
        );
        assert!(
            !sink.is_paused(),
            "leaving Paused for Error must unpause the sink in the same step"
        );
        assert_eq!(
            s.error.is_some(),
            s.state == State::Error,
            "error={:?} state={:?}",
            s.error,
            s.state
        );
    }

    #[test]
    fn replace_sink_recovers_from_a_device_failure_that_arrived_while_paused() {
        let sink = FaultInjectableSink::new(24_000 * 10);
        let mut e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(sink.clone()),
        );
        e.handle(say(
            "Hello there. This keeps it busy for quite a while indeed.",
        ));
        e.tick();
        e.handle(Command::Pause);
        sink.inject_failure("audio device disappeared");
        e.tick();
        assert_eq!(e.snapshot().state, State::Error);

        e.replace_sink(Box::new(VecSink::new(24_000 * 10)));
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle, "a fresh sink clears the failure");
        assert_eq!(s.error, None);

        e.submit("after recovery.".into(), SayOpts::default())
            .expect("accepted");
        for _ in 0..500 {
            e.tick();
        }
        assert!(
            e.audio_written() > 0,
            "the engine must work again after the sink is replaced, even though the failure \
             arrived while paused"
        );
    }

    /// A `Synthesizer` whose `synth` always fails, modelling a bad models
    /// directory or a corrupt weight file: `sayd-kokoro`/`ort` themselves
    /// report a failure like `onnxruntime: File at '.../model.onnx' does
    /// not exist` from the first `synth` call, not from construction.
    struct FailingSynth;
    impl crate::synth::Synthesizer for FailingSynth {
        fn phonemize(&mut self, t: &str, _voice: &str) -> String {
            t.into()
        }
        fn fits(&mut self, _: &str) -> bool {
            true
        }
        fn synth(&mut self, _: &str, _: &str, _: f32) -> Result<Vec<f32>, String> {
            Err("onnxruntime: File at '/models/model.onnx' does not exist".into())
        }
        fn unload(&mut self) {}
        fn is_loaded(&self) -> bool {
            true
        }
    }

    #[test]
    fn synthesis_failure_persists_and_rejects_new_submissions_instead_of_being_silently_cleared() {
        // C3: `Engine::submit` used to clear *any* stuck `Error`
        // unconditionally on a new submission, and the daemon's recovery
        // loop reacted to *any* `Error` by reacquiring the audio device --
        // for a synthesis failure (bad model path, corrupt weights),
        // neither is right: reacquiring a fine device does nothing about a
        // broken model, and clearing the error on the next submission made
        // the daemon look recovered while every submission kept failing the
        // same way, silently. The design doc requires the error to persist
        // and new submissions to be rejected.
        let mut e = Engine::new(
            Config::default(),
            Box::new(FailingSynth),
            Box::new(VecSink::new(24_000)),
        );
        e.handle(say("Anything."));
        for _ in 0..20 {
            e.tick();
        }
        let s = e.snapshot();
        assert_eq!(s.state, State::Error);
        assert_eq!(
            s.error_kind,
            Some(ErrorKind::Synth),
            "a synthesis failure must be distinguishable from a device failure"
        );

        let result = e.submit("try again.".into(), SayOpts::default());
        assert!(
            result.is_err(),
            "submissions must be rejected while a synthesis error persists, got {result:?}"
        );
        let after = e.snapshot();
        assert_eq!(after.state, State::Error, "the error must persist");
        assert!(after.error.is_some(), "the error must persist");
        assert_eq!(after.error_kind, Some(ErrorKind::Synth));
    }

    #[test]
    fn an_explicit_stop_still_dismisses_a_persistent_synthesis_error() {
        // The design doc's escape hatch: submissions are rejected while a
        // `Synth` error persists, but an explicit "shut up" command must
        // still be able to clear it (e.g. after the operator fixes the
        // models directory and wants the daemon usable again without a
        // restart).
        let mut e = Engine::new(
            Config::default(),
            Box::new(FailingSynth),
            Box::new(VecSink::new(24_000)),
        );
        e.handle(say("Anything."));
        for _ in 0..20 {
            e.tick();
        }
        assert_eq!(e.snapshot().state, State::Error);
        e.handle(Command::Stop);
        let s = e.snapshot();
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.error, None);
        assert_eq!(s.error_kind, None);
    }

    #[test]
    fn device_failure_is_tagged_as_a_sink_error_so_recovery_still_arms() {
        // Mirror of the test above via the other route into `Error`: C3's
        // fix must not undo the earlier fix that lets *any* device failure
        // -- cpal's `StreamInvalidated`/`BufferUnderrun` included, neither
        // of which mention "device" in their message -- arm the daemon's
        // reacquisition loop.
        let mut e = Engine::new(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(FailingSink::new()),
        );
        e.submit(text_spanning_multiple_chunks(), SayOpts::default())
            .expect("accepted");
        for _ in 0..200 {
            e.tick();
        }
        let s = e.snapshot();
        assert_eq!(s.state, State::Error);
        assert_eq!(
            s.error_kind,
            Some(ErrorKind::Sink),
            "a device failure must arm the daemon's recovery loop"
        );
    }

    #[test]
    fn a_rejection_is_tagged_and_a_later_valid_submission_still_clears_it() {
        // Pins `ErrorKind::Rejected`'s own behaviour directly: unlike
        // `Sink`/`Synth`, a rejection is about the submission itself, not
        // the engine, so it must keep auto-clearing on a later valid
        // submission exactly as `a_later_successful_say_clears_the_error`
        // (above) already pins by observable behaviour alone.
        let cfg = Config {
            max_chars: 10,
            ..Config::default()
        };
        let mut e = Engine::new(
            cfg,
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        e.handle(say("this is definitely longer than ten characters"));
        let s = e.snapshot();
        assert_eq!(s.state, State::Error);
        assert_eq!(s.error_kind, Some(ErrorKind::Rejected));

        let outcome = e
            .submit("ok.".into(), SayOpts::default())
            .expect("a valid submission must be accepted");
        let id = match outcome {
            Submitted::Queued(id) => id,
            other => panic!("must be queued, got {other:?}"),
        };
        assert_ne!(id, 0);
        let after = e.snapshot();
        assert_eq!(after.error, None);
        assert_eq!(after.error_kind, None);
    }
}
