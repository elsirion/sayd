//! The real audio sink: a lock-free ring feeding a cpal output stream.
//!
//! The consumer half lives inside cpal's callback, which is a hard realtime
//! context -- it may not lock, allocate, or block. Everything device-specific
//! is confined to `RingSink::new`; everything else -- the ring, pause,
//! `clear`'s generation counter, and the routine that fills an output buffer
//! -- lives in [`RingProducer`] / [`RingConsumer`] below, which know nothing
//! about cpal and are exercised directly by the tests in this file (there is
//! no audio device in the sandbox this was built in, so this pair is the only
//! part of the sink that can be verified here at all).
//!
//! `rtrb::RingBuffer::new` already splits into a `Producer` and a `Consumer`
//! that can each live on their own thread; `RingProducer` and `RingConsumer`
//! just carry that split forward, adding the atomics both sides need to
//! agree on (how much is pending, what generation `clear` last bumped to,
//! whether playback is paused). Bundling producer and consumer into one
//! struct was considered and rejected: cpal's callback needs to *own* the
//! consumer for the `'static` closure, and the engine needs to mutate the
//! producer from its own thread at the same time, so one shared struct would
//! need a `Mutex` -- exactly the lock this module's callback must not take.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use sayd_core::audio::AudioSink;

/// Ten seconds of buffer at 24 kHz is ~1 MB and far more than the two-chunk
/// lookahead needs; it exists to absorb scheduling jitter, not to run ahead.
const BUFFER_SECONDS: usize = 10;

/// State shared between the producer and consumer halves of a ring.
///
/// Every field here is written by exactly one side and read by the other, so
/// there is never a "who wins" race on a single field -- see the accounting
/// argument on [`RingProducer::clear`] and [`RingConsumer::fill`] for why
/// `pending` in particular cannot drift or go negative.
struct Shared {
    /// Samples accepted but not yet played *or* discarded.
    pending: AtomicUsize,
    /// Samples `clear` has condemned but the consumer has not yet physically
    /// removed from the ring. Set by `clear` (via `swap`, so a concurrent
    /// `fill` never observes a half-written value), consumed by the first
    /// `fill` that notices the generation changed.
    to_discard: AtomicUsize,
    /// Monotonic: total samples ever accepted by `push`. Never decremented.
    total_written: AtomicUsize,
    /// Bumped by `clear`. The consumer remembers the last generation it
    /// acted on and, on a mismatch, discards whatever is currently in the
    /// ring before resuming normal playback.
    generation: AtomicUsize,
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
    /// The generation this side last acted on. Purely local -- nothing else
    /// ever reads or writes it -- so no atomic is needed for it.
    seen_generation: usize,
}

/// Build a ring of `capacity` samples, split into its producer and consumer
/// halves.
pub(crate) fn ring(capacity: usize) -> (RingProducer, RingConsumer) {
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);
    let shared = Arc::new(Shared {
        pending: AtomicUsize::new(0),
        to_discard: AtomicUsize::new(0),
        total_written: AtomicUsize::new(0),
        generation: AtomicUsize::new(0),
        paused: AtomicBool::new(false),
    });
    (
        RingProducer { producer, shared: shared.clone(), capacity },
        RingConsumer { consumer, shared, seen_generation: 0 },
    )
}

impl RingProducer {
    /// Push as many samples as fit. Returns how many were accepted.
    pub(crate) fn push(&mut self, samples: &[f32]) -> usize {
        let mut n = 0;
        for &s in samples {
            if self.producer.push(s).is_err() {
                break;
            }
            n += 1;
        }
        if n > 0 {
            self.shared.pending.fetch_add(n, Ordering::AcqRel);
            self.shared.total_written.fetch_add(n, Ordering::Relaxed);
        }
        n
    }

    pub(crate) fn pending(&self) -> usize {
        self.shared.pending.load(Ordering::Acquire)
    }

    pub(crate) fn total_written(&self) -> usize {
        self.shared.total_written.load(Ordering::Relaxed)
    }

    /// Discard everything not yet played.
    ///
    /// The producer cannot drain the consumer's half of the ring directly --
    /// that would race the audio thread mid-pop -- so this only *signals*:
    /// it snapshots how much is currently pending, moves that count into
    /// `to_discard`, and bumps `generation`. The consumer does the physical
    /// removal on its next `fill`, in bulk, before playing anything else.
    ///
    /// `push` and `clear` are both `&mut self` on the one type the engine
    /// holds, so they never run concurrently with each other -- the only
    /// concurrency here is this call racing the audio thread's `fill`. The
    /// `pending.swap(0, ..)` is a single atomic read-modify-write, so it
    /// reads some real value that was actually in `pending` at some instant
    /// (never a torn one), and it can only ever be *equal to or smaller*
    /// than what is physically still in the ring: nothing but `fill` ever
    /// removes items, and `fill` only removes items after decrementing
    /// `pending` (see below) or after this very swap already zeroed it. So
    /// `to_discard` can never ask the consumer to remove more than is
    /// actually there; `fill` additionally clamps with `.min(slots())` as a
    /// second line of defence.
    ///
    /// Samples pushed *after* this call add to `pending` normally and are
    /// never part of `to_discard`, so a `clear` immediately followed by a
    /// `push` does not lose the new audio, even though both old and new
    /// samples sit in the same physical ring (see
    /// `clear_then_push_only_discards_the_pre_clear_samples`).
    pub(crate) fn clear(&mut self) {
        let prior = self.shared.pending.swap(0, Ordering::AcqRel);
        self.shared.to_discard.fetch_add(prior, Ordering::AcqRel);
        self.shared.generation.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }
}

impl RingConsumer {
    /// Fill `out` (interleaved, `channels` wide) for one device callback.
    ///
    /// Must not allocate or block: reads are done in bulk with rtrb's
    /// chunked API rather than one `pop()` per sample, and the only atomics
    /// touched are single fixed-cost operations regardless of how much data
    /// moves.
    pub(crate) fn fill(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let frames = out.len() / channels;

        let generation = self.shared.generation.load(Ordering::Acquire);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            let target = self.shared.to_discard.swap(0, Ordering::AcqRel);
            let n = target.min(self.consumer.slots());
            if n > 0 {
                if let Ok(chunk) = self.consumer.read_chunk(n) {
                    chunk.commit_all();
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
            }
        }
        for slot in out.iter_mut().skip(taken * channels) {
            *slot = 0.0;
        }

        if taken > 0 {
            // Saturating rather than a raw `fetch_sub`: `taken` can only ever
            // be samples this call itself just popped, which by construction
            // were counted in `pending`, so this cannot legitimately
            // underflow -- but a raw `fetch_sub` on `usize` would panic
            // rather than degrade if some future change broke that
            // invariant, and panicking on the audio thread is worse than a
            // wrong-but-harmless diagnostic counter.
            let _ = self.shared.pending.fetch_update(Ordering::AcqRel, Ordering::Acquire, |p| {
                Some(p.saturating_sub(taken))
            });
        }
    }
}

/// Open the default output device and wire a [`RingProducer`] /
/// [`RingConsumer`] pair to it. All device-specific setup lives here; the
/// ring logic above is unaware cpal exists.
pub struct RingSink {
    producer: RingProducer,
    /// Held to keep the stream alive; dropping it stops playback.
    _stream: cpal::Stream,
    /// Device rate, which may differ from the synthesizer's.
    pub device_sample_rate: u32,
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
    /// If the device refuses that rate, its default is used and
    /// `device_sample_rate` reports it; the caller must resample.
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

        let capacity = sample_rate as usize * BUFFER_SECONDS;
        let (producer, mut consumer) = ring(capacity);

        let stream = device
            .build_output_stream(
                config.config(),
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    consumer.fill(out, channels);
                },
                move |e| eprintln!("audio stream error: {e}"),
                None,
            )
            .map_err(|e| format!("could not build output stream: {e}"))?;

        stream.play().map_err(|e| format!("could not start stream: {e}"))?;

        Ok(RingSink { producer, _stream: stream, device_sample_rate })
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

    fn capacity(&self) -> usize {
        self.producer.capacity()
    }

    fn total_written(&self) -> usize {
        self.producer.total_written()
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
        assert_eq!(out2, [1.0, 2.0], "resume plays what was buffered before the pause");
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
        assert_eq!(prod.pending(), 2, "only the post-clear push counts as pending");

        // The first fill after a clear discards the stale generation's worth
        // (2 samples) and emits silence for this callback; the fresh samples
        // remain buffered for the next one.
        let mut out = [1.0; 2];
        cons.fill(&mut out, 1);
        assert_eq!(out, [0.0, 0.0]);

        let mut out2 = [0.0; 2];
        cons.fill(&mut out2, 1);
        assert_eq!(out2, [9.0, 9.0], "fresh post-clear samples must survive the discard");
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
}
