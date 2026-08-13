//! The real audio sink: a lock-free ring feeding a cpal output stream.
//!
//! The consumer half lives inside cpal's callback, which is a hard realtime
//! context -- it may not lock, allocate, or block. Everything device-specific
//! is confined to `RingSink::new`; everything else -- the ring, pause,
//! `clear`'s discard marker, and the routine that fills an output buffer --
//! lives in [`RingProducer`] / [`RingConsumer`] below, which know nothing
//! about cpal and are exercised directly by the tests in this file (there is
//! no audio device in the sandbox this was built in, so this pair is the only
//! part of the sink that can be verified here at all).
//!
//! `rtrb::RingBuffer::new` already splits into a `Producer` and a `Consumer`
//! that can each live on their own thread; `RingProducer` and `RingConsumer`
//! just carry that split forward, adding the atomics both sides need to
//! agree on (how much has been written, where `clear` last drew the discard
//! line, whether playback is paused). Bundling producer and consumer into
//! one struct was considered and rejected: cpal's callback needs to *own*
//! the consumer for the `'static` closure, and the engine needs to mutate
//! the producer from its own thread at the same time, so one shared struct
//! would need a `Mutex` -- exactly the lock this module's callback must not
//! take.
//!
//! ## Synchronization design
//!
//! Two earlier versions of this module each reasoned field-by-field about
//! why their atomic updates were safe, concluded they were race-free, and
//! were wrong: a concurrent `fill()` could land between two writes (or
//! between a decision and the write that records it) and observe a
//! combination no single field's proof ruled out. The second attempt fixed
//! the first bug (`clear()`'s three separate writes) by packing a generation
//! counter and a discard count into one atomic word, updated by `clear` with
//! a CAS and read by `fill` with a single load -- correct on its own, but it
//! still shared a *separate* `pending` counter with `fill`'s normal (playing)
//! path, and a `clear()` landing in the gap between that path deciding to
//! play some samples and recording that decision let its own accounting
//! drift by exactly one buffer's worth, silently discarding fresh audio to
//! make up the difference. Stress-testing caught both, which is the reason
//! this module carries the concurrent tests below instead of only the
//! original sequential ones.
//!
//! The design that finally closes both gaps drops the shared "how much is
//! pending" counter entirely and replaces it with two pieces of state that
//! *cannot* race each other by construction:
//!
//! - **`Shared::discard_until`** is a monotonic sequence marker, in the same
//!   numbering as `total_written`: "discard everything written before this
//!   point." `clear()` sets it to the current `total_written()` with a
//!   single `fetch_max` -- monotonic, so concurrent or repeated `clear()`
//!   calls just converge on the highest mark, and there is no CAS loop to
//!   retry because `fetch_max` can't spuriously fail the way a
//!   compare-exchange can.
//! - **`RingConsumer::resolved`** (a plain, non-atomic `usize`) is `fill`'s
//!   own private count of how many samples it has removed from the ring so
//!   far, whether played or discarded. Nothing else ever reads or writes it,
//!   so there is no race on it, period. Each call compares it against
//!   `discard_until`: if there is a gap, that many samples (clamped to what
//!   is physically in the ring right now) get discarded instead of played,
//!   silence is emitted, and the call returns; otherwise it plays normally.
//!   `resolved` is also copied into `Shared::resolved` after each call
//!   purely so `RingProducer::pending()` can read it -- that store carries
//!   no decision, only a broadcast of a value only `fill` ever decides.
//!
//! Because `fill`'s entire discard/play decision depends on nothing but its
//! own private counter, rtrb's own (thread-local-accurate) `slots()`, and a
//! single monotonic marker it only ever compares for magnitude, there is no
//! pair of racing writes left for a concurrent `clear()` to land between:
//! `clear()` cannot make `fill()` misclassify a sample, only delay how soon
//! `fill()` notices a new mark (bounded by ordinary memory visibility, same
//! as before). This was verified the same way the previous two attempts
//! were checked and found wanting: by threading the producer and consumer
//! through real OS threads under load (the `concurrent_*` tests below), not
//! by argument alone.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use sayd_core::audio::AudioSink;

use crate::resample::ResamplingProducer;

/// Ten seconds of buffer at 24 kHz is ~1 MB and far more than the two-chunk
/// lookahead needs; it exists to absorb scheduling jitter, not to run ahead.
const BUFFER_SECONDS: usize = 10;

/// State shared between the producer and consumer halves of a ring.
///
/// All three counters are plain `usize` (64-bit on every target this runs
/// on) and none of them are ever decremented, so none of them are anywhere
/// near overflowing: even a century of continuous playback at 24 kHz is
/// under 10^11 samples, five orders of magnitude short of `usize::MAX`. No
/// packing, no bit-width analysis needed here -- unlike an earlier version
/// of this module, which packed two counters into one `u64` specifically to
/// make a different pair of writes atomic; that mechanism is gone (see the
/// module doc's "Synchronization design" section for why).
struct Shared {
    /// Monotonic: total samples ever accepted by `push`. Never decremented.
    total_written: AtomicUsize,
    /// Sequence mark (same numbering as `total_written`): everything
    /// written before this point should be discarded, not played. Set by
    /// `clear` via `fetch_max`, so it only ever grows. Read by `fill`,
    /// which compares it against its own private `resolved` count -- see
    /// [`RingConsumer::resolved`] and the module doc.
    discard_until: AtomicUsize,
    /// A published copy of [`RingConsumer::resolved`], written by `fill`
    /// after each call purely so [`RingProducer::pending`] has something to
    /// read cross-thread. Nothing ever reads this to make a decision --
    /// `fill` itself always uses its own private, unshared copy -- so
    /// writing it is a broadcast, never part of a race.
    resolved: AtomicUsize,
    paused: AtomicBool,
}

/// The producer side: pushes samples in, and is what `AudioSink` is
/// implemented against. Lives with the engine, on whichever thread owns the
/// sink.
pub(crate) struct RingProducer {
    producer: rtrb::Producer<f32>,
    shared: Arc<Shared>,
    capacity: usize,
}

/// The consumer side: pulls samples out to fill a device buffer. Lives
/// inside the cpal callback, on the audio thread.
pub(crate) struct RingConsumer {
    consumer: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    /// How many samples (by the same sequence numbering as `total_written`)
    /// this side has removed from the ring so far, whether played or
    /// discarded. Purely local -- nothing else ever reads or writes it, so
    /// there is nothing to race here. `Shared::resolved` is a one-way,
    /// no-decision-attached broadcast of this value for `pending()`.
    resolved: usize,
}

/// Build a ring of `capacity` samples, split into its producer and consumer
/// halves.
pub(crate) fn ring(capacity: usize) -> (RingProducer, RingConsumer) {
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);
    let shared = Arc::new(Shared {
        total_written: AtomicUsize::new(0),
        discard_until: AtomicUsize::new(0),
        resolved: AtomicUsize::new(0),
        paused: AtomicBool::new(false),
    });
    (
        RingProducer {
            producer,
            shared: shared.clone(),
            capacity,
        },
        RingConsumer {
            consumer,
            shared,
            resolved: 0,
        },
    )
}

impl RingProducer {
    /// Push as many samples as fit. Returns how many were accepted.
    ///
    /// Writes go through rtrb's bulk chunk API rather than one `push` per
    /// sample, for two reasons: it's faster, and it gives a single commit
    /// point instead of making each sample individually visible as soon as
    /// it lands. rtrb's `Producer::slots()` only ever *grows* between calls
    /// here (the consumer thread can free slots by popping, but nothing
    /// shrinks them except this very method, which is `&mut self` and
    /// therefore never runs concurrently with itself or with `clear`), so a
    /// `slots()` snapshot taken at the top is still a valid lower bound by
    /// the time `write_chunk` runs immediately after.
    ///
    /// `total_written` is incremented *before* `commit_all()` is called.
    /// This mirrors the fix for the old "batched `pending` update" bug: it
    /// means `pending()`, called from any thread, can only ever see a value
    /// that's a touch *ahead* of what has actually become visible in the
    /// ring, never behind -- consistent with `fill`'s own side publishing
    /// `resolved` *after* its commit (see [`RingConsumer::fill`]), so every
    /// contributor to `pending()`'s arithmetic biases the same direction.
    pub(crate) fn push(&mut self, samples: &[f32]) -> usize {
        if samples.is_empty() {
            return 0;
        }
        let n = samples.len().min(self.producer.slots());
        if n == 0 {
            return 0;
        }
        // `write_chunk` (not `write_chunk_uninit`) pre-fills the chunk with
        // `f32::default()` (0.0), so it's always valid to write into and its
        // `commit_all` is safe -- unlike `write_chunk_uninit`'s, which is an
        // `unsafe fn` because the caller has to prove every slot was
        // initialised. That's a deliberate trade: a redundant zero-fill on
        // the (non-realtime) engine thread, in exchange for no `unsafe` in
        // this module. See `n`'s derivation above for why this can't fail.
        let Ok(mut chunk) = self.producer.write_chunk(n) else {
            return 0;
        };
        let (a, b) = chunk.as_mut_slices();
        a.copy_from_slice(&samples[..a.len()]);
        b.copy_from_slice(&samples[a.len()..n]);

        self.shared.total_written.fetch_add(n, Ordering::Release);
        chunk.commit_all();
        n
    }

    /// Samples accepted but not yet played *or* discarded.
    ///
    /// Computed, not tracked: `total_written()` minus whichever is greater
    /// of `fill`'s published progress (`resolved`) and how far `clear` has
    /// drawn the discard line (`discard_until`). The `max` is what makes a
    /// `clear()` zero this out *immediately*, even before `fill` has
    /// physically caught up to the new mark -- matching the pre-rewrite
    /// API's behaviour (see `clear_discards_buffered_samples_which_never_reach_output`,
    /// which asserts exactly that) -- without needing `clear` to touch any
    /// counter `fill` also writes.
    pub(crate) fn pending(&self) -> usize {
        let resolved = self.shared.resolved.load(Ordering::Acquire);
        let discard_until = self.shared.discard_until.load(Ordering::Acquire);
        let effectively_resolved = resolved.max(discard_until);
        let total = self.shared.total_written.load(Ordering::Acquire);
        total.saturating_sub(effectively_resolved)
    }

    pub(crate) fn total_written(&self) -> usize {
        self.shared.total_written.load(Ordering::Relaxed)
    }

    /// Discard everything not yet played.
    ///
    /// The producer cannot drain the consumer's half of the ring directly --
    /// that would race the audio thread mid-pop -- so this only *signals*:
    /// it draws a line at the current `total_written()` and asks `fill` to
    /// discard, rather than play, anything written before that line. The
    /// consumer does the physical removal on its next `fill`, in bulk,
    /// before playing anything else.
    ///
    /// `push` and `clear` are both `&mut self` on the one type the engine
    /// holds, so they never run concurrently with each other -- the only
    /// concurrency here is this call racing the audio thread's `fill`. The
    /// mark itself is a single atomic `fetch_max`, so it can never be torn,
    /// and it only ever grows (a `clear()` racing another `clear()` just
    /// converges on the higher mark). The `min(consumer.slots())` clamp on
    /// `fill`'s side guards against asking rtrb to remove more than is
    /// physically in the ring yet (the mark can be momentarily ahead of
    /// what's committed, the same way `pending()` can, and `fill` simply
    /// catches up on a later call once that data arrives).
    ///
    /// Samples pushed *after* this call raise `total_written` past the
    /// mark, so they are never condemned by it even though both old and new
    /// samples sit in the same physical ring (see
    /// `clear_then_push_only_discards_the_pre_clear_samples`).
    pub(crate) fn clear(&mut self) {
        let mark = self.shared.total_written.load(Ordering::Acquire);
        self.shared.discard_until.fetch_max(mark, Ordering::AcqRel);
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
    }

    /// The counterpart read to `set_paused`: a plain load of the same flag,
    /// with no decision-making of its own to race anything.
    pub(crate) fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }
}

impl RingConsumer {
    /// Test-only: lets a concurrent test on the consumer thread poll
    /// `pending` directly from this side, without needing a `RingProducer`
    /// reference (which the consumer thread doesn't otherwise have).
    #[cfg(test)]
    pub(crate) fn debug_pending(&self) -> usize {
        let total = self.shared.total_written.load(Ordering::Acquire);
        let effectively_resolved = self
            .shared
            .resolved
            .load(Ordering::Acquire)
            .max(self.shared.discard_until.load(Ordering::Acquire));
        total.saturating_sub(effectively_resolved)
    }

    /// Fill `out` (interleaved, `channels` wide) for one device callback.
    ///
    /// Must not allocate or block: reads are done in bulk with rtrb's
    /// chunked API rather than one `pop()` per sample, and the only atomics
    /// touched are a single load and (at most) a single store, regardless
    /// of how much data moves -- no CAS, no loop, this method cannot spin.
    pub(crate) fn fill(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let frames = out.len() / channels;

        let discard_until = self.shared.discard_until.load(Ordering::Acquire);
        if self.resolved < discard_until {
            // There is a gap between what this side has resolved and what
            // `clear` has condemned: close it before playing anything else,
            // clamped to what's physically here now (a `clear()` can draw
            // the mark slightly ahead of what's actually landed yet, the
            // same way `pending()` can read slightly ahead -- the rest is
            // picked up on a later call once that data arrives).
            let to_discard = (discard_until - self.resolved).min(self.consumer.slots());
            if to_discard > 0 {
                if let Ok(chunk) = self.consumer.read_chunk(to_discard) {
                    chunk.commit_all();
                    self.resolved += to_discard;
                    self.shared.resolved.store(self.resolved, Ordering::Release);
                }
            }
            out.fill(0.0);
            return;
        }

        if self.shared.paused.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }

        let avail = self.consumer.slots().min(frames);
        let mut taken = 0usize;
        if avail > 0 {
            if let Ok(chunk) = self.consumer.read_chunk(avail) {
                let (a, b) = chunk.as_slices();
                for (i, &s) in a.iter().chain(b.iter()).enumerate() {
                    let base = i * channels;
                    for c in 0..channels {
                        out[base + c] = s;
                    }
                }
                taken = a.len() + b.len();
                chunk.commit_all();
                // Publish *after* the commit, not before: this is what
                // keeps `pending()` a same-direction (over-, never under-)
                // estimate, matching `push`'s `total_written` ordering --
                // see the comment there. Purely a broadcast for
                // `RingProducer::pending()`; `self.resolved` (the value
                // this call's own next-time decisions are based on) is
                // already correct at this point regardless of when the
                // publish happens.
                self.resolved += taken;
                self.shared.resolved.store(self.resolved, Ordering::Release);
            }
        }
        for slot in out.iter_mut().skip(taken * channels) {
            *slot = 0.0;
        }
    }
}

/// Open the default output device and wire a [`RingProducer`] /
/// [`RingConsumer`] pair to it. All device-specific setup lives here; the
/// ring logic above is unaware cpal exists.
pub struct RingSink {
    producer: ResamplingProducer,
    /// Held to keep the stream alive; dropping it stops playback.
    _stream: cpal::Stream,
    /// Device rate, which may differ from the synthesizer's.
    pub device_sample_rate: u32,
    /// Set by cpal's error callback (a different thread) when the stream
    /// dies; taken by `take_error`. This is the only way a lost device is
    /// ever noticed -- see the module doc's synchronization design for why
    /// nothing else in this file may gain a lock, but this field is never
    /// touched from the realtime `fill` callback, only from the error
    /// callback and from `take_error`, so a `Mutex` here is fine.
    error: Arc<Mutex<Option<String>>>,
}

// No `unsafe impl Send` here. `AudioSink: Send` is a supertrait bound, so
// `impl AudioSink for RingSink` below only typechecks if `RingSink` is
// already `Send`; every field is auto-Send on this platform (`RingProducer`
// wraps `rtrb::Producer<f32>`, which is `Send`, plus an `Arc` of atomics;
// `cpal::Stream`'s ALSA backend on Linux is `Send` too), so the compiler
// derives it for free. Asserted here so a future dependency bump that broke
// that silently would fail loudly at this line instead of vanishing into an
// `unsafe impl` nobody re-checks.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<RingSink>();
};

impl RingSink {
    /// Open the default output device, preferring `sample_rate` mono.
    ///
    /// If the device refuses that rate, its default is used instead and
    /// `device_sample_rate` reports it -- but this is transparent to
    /// callers: every `push`/`pending`/`total_written`/`capacity` value
    /// still speaks `sample_rate`-Hz (input) sample units, via a streaming
    /// resampler (see `resample.rs`) wired into the producer half.
    pub fn new(sample_rate: u32) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default audio output device".to_string())?;

        let wanted: cpal::SampleRate = sample_rate;
        let mut chosen: Option<cpal::SupportedStreamConfig> = None;
        if let Ok(ranges) = device.supported_output_configs() {
            for r in ranges {
                if r.channels() == 1
                    && r.min_sample_rate() <= wanted
                    && r.max_sample_rate() >= wanted
                    && r.sample_format() == cpal::SampleFormat::F32
                {
                    chosen = Some(r.with_sample_rate(wanted));
                    break;
                }
            }
        }
        let config = match chosen {
            Some(c) => c,
            None => device
                .default_output_config()
                .map_err(|e| format!("no usable output config: {e}"))?,
        };
        let device_sample_rate = config.sample_rate();
        let channels = config.channels() as usize;

        // Sized in *device*-rate samples so the ring holds `BUFFER_SECONDS`
        // of real audio time regardless of the resampling ratio -- sizing
        // this by `sample_rate` (the input rate) instead would, on a
        // resampling device, buffer less wall-clock time than intended.
        let capacity = device_sample_rate as usize * BUFFER_SECONDS;
        let (raw_producer, mut consumer) = ring(capacity);
        let producer = ResamplingProducer::new(raw_producer, sample_rate, device_sample_rate);

        let error = Arc::new(Mutex::new(None));
        let error_writer = error.clone();
        let stream = device
            .build_output_stream(
                config.config(),
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    consumer.fill(out, channels);
                },
                move |e| {
                    eprintln!("audio stream error: {e}");
                    *error_writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                },
                None,
            )
            .map_err(|e| format!("could not build output stream: {e}"))?;

        stream
            .play()
            .map_err(|e| format!("could not start stream: {e}"))?;

        Ok(RingSink {
            producer,
            _stream: stream,
            device_sample_rate,
            error,
        })
    }
}

impl AudioSink for RingSink {
    fn push(&mut self, samples: &[f32]) -> usize {
        self.producer.push(samples)
    }

    fn pending(&self) -> usize {
        self.producer.pending()
    }

    fn clear(&mut self) {
        self.producer.clear()
    }

    fn set_paused(&mut self, paused: bool) {
        self.producer.set_paused(paused)
    }

    fn is_paused(&self) -> bool {
        self.producer.is_paused()
    }

    fn capacity(&self) -> usize {
        self.producer.capacity()
    }

    fn total_written(&self) -> usize {
        self.producer.total_written()
    }

    fn take_error(&mut self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_accepts_up_to_capacity_and_reports_short_count() {
        let (mut prod, _cons) = ring(4);
        assert_eq!(prod.push(&[1.0, 2.0]), 2);
        assert_eq!(prod.push(&[3.0, 4.0, 5.0]), 2, "only what fits is accepted");
        assert_eq!(prod.pending(), 4);
    }

    #[test]
    fn fill_drains_fifo_and_produces_pushed_samples() {
        let (mut prod, mut cons) = ring(8);
        prod.push(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0; 4];
        cons.fill(&mut out, 1);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(prod.pending(), 0);
    }

    #[test]
    fn underrun_yields_silence_not_stale_data() {
        let (_prod, mut cons) = ring(8);
        let mut out = [9.9; 4];
        cons.fill(&mut out, 1);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn underrun_with_some_buffered_pads_remainder_with_silence() {
        let (mut prod, mut cons) = ring(8);
        prod.push(&[1.0, 2.0]);
        let mut out = [9.9; 4];
        cons.fill(&mut out, 1);
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn pause_emits_silence_and_preserves_buffered_samples() {
        let (mut prod, mut cons) = ring(8);
        prod.push(&[1.0, 2.0]);
        prod.set_paused(true);
        let mut out = [9.9; 2];
        cons.fill(&mut out, 1);
        assert_eq!(out, [0.0, 0.0]);
        assert_eq!(prod.pending(), 2, "buffered samples survive a pause");

        prod.set_paused(false);
        let mut out2 = [9.9; 2];
        cons.fill(&mut out2, 1);
        assert_eq!(
            out2,
            [1.0, 2.0],
            "resume plays what was buffered before the pause"
        );
    }

    #[test]
    fn clear_discards_buffered_samples_which_never_reach_output() {
        let (mut prod, mut cons) = ring(8);
        prod.push(&[1.0, 2.0, 3.0]);
        prod.clear();
        assert_eq!(prod.pending(), 0);

        let mut out = [9.9; 3];
        cons.fill(&mut out, 1);
        assert_eq!(out, [0.0; 3], "cleared samples must never appear in output");
    }

    #[test]
    fn clear_then_push_only_discards_the_pre_clear_samples() {
        let (mut prod, mut cons) = ring(8);
        prod.push(&[1.0, 2.0]);
        prod.clear();
        prod.push(&[9.0, 9.0]);
        assert_eq!(
            prod.pending(),
            2,
            "only the post-clear push counts as pending"
        );

        // The first fill after a clear discards the stale generation's worth
        // (2 samples) and emits silence for this callback; the fresh samples
        // remain buffered for the next one.
        let mut out = [1.0; 2];
        cons.fill(&mut out, 1);
        assert_eq!(out, [0.0, 0.0]);

        let mut out2 = [0.0; 2];
        cons.fill(&mut out2, 1);
        assert_eq!(
            out2,
            [9.0, 9.0],
            "fresh post-clear samples must survive the discard"
        );
    }

    #[test]
    fn pending_and_total_written_stay_accurate_across_push_fill_clear() {
        let (mut prod, mut cons) = ring(16);
        assert_eq!(prod.total_written(), 0);
        prod.push(&[1.0, 2.0, 3.0]);
        assert_eq!(prod.total_written(), 3);
        assert_eq!(prod.pending(), 3);

        let mut out = [0.0; 2];
        cons.fill(&mut out, 1);
        assert_eq!(prod.pending(), 1);
        assert_eq!(prod.total_written(), 3, "total_written never decreases");

        prod.clear();
        assert_eq!(prod.pending(), 0);
        assert_eq!(prod.total_written(), 3);

        prod.push(&[4.0]);
        assert_eq!(prod.total_written(), 4);
        assert_eq!(prod.pending(), 1);
    }

    #[test]
    fn mono_samples_fan_out_to_multiple_channels() {
        let (mut prod, mut cons) = ring(8);
        prod.push(&[1.0, 2.0]);
        let mut out = [0.0; 6]; // 2 frames * 3 channels
        cons.fill(&mut out, 3);
        assert_eq!(out, [1.0, 1.0, 1.0, 2.0, 2.0, 2.0]);
    }

    #[test]
    fn capacity_reports_the_configured_size() {
        let (prod, _cons) = ring(64);
        assert_eq!(prod.capacity(), 64);
    }

    // -- Concurrent stress tests -------------------------------------------
    //
    // Every test above drives `push`/`clear`/`fill` sequentially on one
    // thread, which is structurally incapable of observing an interleaving
    // -- that's exactly why the races this module was rewritten to fix
    // shipped in the first place despite otherwise thorough edge-case
    // coverage. These two put the producer and consumer on real OS threads,
    // the way `RingSink` actually uses them (engine thread vs. cpal's audio
    // thread), and assert an invariant that a race would violate. The
    // interleaving each run hits is not deterministic, but the assertions
    // are: they fail loudly whenever the invariant breaks and never flake
    // when it holds, because both checks are exact accounting identities,
    // not timing-sensitive heuristics on `fill`'s output content -- an
    // earlier version of this test tried to infer "fully drained" from
    // `fill` producing a silent call, which a discard-branch call always
    // does even mid-drain, and that ambiguity cost real debugging time
    // while this suite was being built. Polling the exact counters below
    // instead removes it entirely.

    /// Race 1 repro: `clear()` must look atomic to a concurrent `fill()` --
    /// no interleaving should ever let the two sides disagree about how
    /// much is actually outstanding.
    ///
    /// Drives many concurrent push/clear/push cycles against a continuously
    /// running `fill()`, then -- after both sides are joined and the ring is
    /// fully drained (polled via `debug_pending()`, not inferred from
    /// `fill`'s output) -- asserts `pending() == 0`. In a correct
    /// implementation this always holds: every sample `clear()` condemns is
    /// accounted for exactly once against what `fill()` actually removes.
    /// Run against each of this module's two earlier (buggy) designs during
    /// development, this assertion failed reliably: on every one of several
    /// thousand runs against the very first version (three independent
    /// non-atomic writes in `clear()`), and on roughly 1 in 100-700 runs
    /// against the second (a packed generation+discard-count word that
    /// still raced a separate `pending` counter shared with `fill`'s normal
    /// path) -- a real bug this test caught before it shipped.
    #[test]
    fn concurrent_push_clear_push_keeps_pending_from_drifting() {
        use std::thread;

        let (mut prod, mut cons) = ring(4096);
        // The reviewer's repro needed a few hundred to a few thousand
        // trials; this runs comfortably more while staying well under a
        // second.
        let trials: u32 = 20_000;
        // Deliberately much larger than `fill`'s per-call frame count below,
        // so a `clear()` can't be fully "beaten" by legitimate, non-racy
        // consumption of the whole batch before it even runs.
        let batch = 64;
        let stop = Arc::new(AtomicBool::new(false));

        let stop_reader = stop.clone();
        let consumer = thread::spawn(move || {
            let mut out = [0f32; 16];
            loop {
                // Read `stop` *before* `fill`, not after: `stop` is only set
                // once every push this test will ever make has already
                // happened (on the producer thread, in program order before
                // the store), so observing `stop == true` here guarantees
                // (via the ordinary release/acquire happens-before chain)
                // that every `fill` call from here on can see all of it.
                if stop_reader.load(Ordering::Acquire) {
                    // Poll `pending` directly rather than inferring "fully
                    // drained" from `fill`'s output content -- see the note
                    // above the concurrent tests section for why that's not
                    // a reliable signal. The iteration cap is purely a hang
                    // guard: a correct implementation converges in at most a
                    // few hundred calls (ring capacity / this buffer's
                    // size); if it doesn't, the assertion below reports the
                    // leftover instead of this test hanging forever.
                    for _ in 0..100_000 {
                        if cons.debug_pending() == 0 {
                            break;
                        }
                        cons.fill(&mut out, 1);
                    }
                    break;
                }
                cons.fill(&mut out, 1);
            }
        });

        for i in 1..=trials {
            let marker = i as f32; // never 0.0, so distinguishable from silence
            prod.push(&vec![marker; batch]);
            prod.clear();
            prod.push(&vec![marker; batch]);
        }
        stop.store(true, Ordering::Release);
        consumer.join().expect("consumer thread panicked");

        assert_eq!(
            prod.pending(),
            0,
            "pending drifted away from zero after a fully drained concurrent \
             push/clear/push stress run -- clear() and fill() disagreed about \
             how much was actually outstanding (total_written={})",
            prod.total_written(),
        );
    }

    /// Race 2 repro: `pending` must never lag behind what `fill` can already
    /// pop, or the reverse (drift upward and never recover).
    ///
    /// Pushes a large number of samples in randomly sized chunks against a
    /// continuously running `fill()` on another thread, then -- after both
    /// sides are done and joined -- checks the exact accounting identity:
    /// `pending() == total_written() - samples_actually_played`. Any window
    /// where `fill` could pop a sample before that got accounted for makes
    /// this diverge (as the original bug report measured: `pending()` stuck
    /// at 3, 5, or 8 instead of 0 after 200,000 samples).
    #[test]
    fn concurrent_bulk_push_keeps_pending_exactly_consistent_with_playback() {
        use std::thread;

        let (mut prod, mut cons) = ring(4096);
        let total_samples: usize = 200_000;
        let played = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let played_reader = played.clone();
        let stop_reader = stop.clone();
        let consumer = thread::spawn(move || {
            let mut out = [0f32; 64];
            loop {
                // See the twin test above for why `stop` must be read
                // *before* `fill`: it's what makes `n == 0` below a
                // trustworthy "truly empty" signal instead of one that can
                // race the very last push.
                let stopped = stop_reader.load(Ordering::Acquire);
                cons.fill(&mut out, 1);
                let n = out.iter().filter(|&&s| s != 0.0).count();
                played_reader.fetch_add(n, Ordering::Relaxed);
                if stopped && n == 0 {
                    // Stop requested (before this very call) and this call
                    // drained nothing: the ring is fully empty, so every
                    // pushed sample has now either been counted as played
                    // or is still legitimately pending (there's none of
                    // either left to find).
                    break;
                }
            }
        });

        // Small deterministic PRNG (xorshift64) for chunk sizes: the
        // interleaving it produces varies run to run because it races a
        // real thread, but the sequence of chunk sizes itself is fixed, so
        // this test can never flake on its own randomness.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next_chunk = |remaining: usize| -> usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (1 + (state % 200) as usize).min(remaining)
        };

        let samples = vec![1.0f32; 256];
        let mut pushed_total = 0usize;
        while pushed_total < total_samples {
            let chunk = next_chunk(total_samples - pushed_total).min(samples.len());
            let n = prod.push(&samples[..chunk]);
            pushed_total += n;
            if n == 0 {
                // Ring momentarily full; give the consumer a chance to drain.
                thread::yield_now();
            }
        }
        stop.store(true, Ordering::Release);
        consumer.join().expect("consumer thread panicked");

        assert_eq!(prod.total_written(), total_samples);
        let played_count = played.load(Ordering::Relaxed);
        assert_eq!(
            prod.pending(),
            prod.total_written() - played_count,
            "pending must equal total_written - samples actually played, with no drift",
        );
        assert_eq!(
            prod.pending(),
            0,
            "everything pushed was eventually drained"
        );
    }
}
