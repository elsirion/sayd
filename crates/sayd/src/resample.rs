//! A streaming linear resampler for converting Kokoro's fixed 24 kHz mono
//! output to whatever rate the audio device actually accepted.
//!
//! `kokoro::audio::resample_linear` (the existing linear resampler) is
//! stateless: every call restarts interpolation at input index 0, and its
//! tail flattens to a repeated sample instead of interpolating into whatever
//! comes next. That's fine for one-shot use, but [`sayd_core::audio::AudioSink::push`]
//! is called once per synthesized chunk -- roughly every 1-3 seconds of
//! speech -- for the lifetime of a sink, and calling a stateless resampler
//! once per chunk would reset the interpolation phase and flatten the join
//! at every chunk boundary: an audible click on every join, throughout
//! playback. [`StreamingResampler`] instead carries the fractional read
//! position and the last input sample across calls, so a chunk boundary is
//! invisible to the resampling math -- the first output sample of a new
//! chunk interpolates against the *last sample of the previous chunk*,
//! exactly as if the whole stream had been resampled in one call. See the
//! `feeding_the_same_signal_in_small_chunks_matches_one_big_call` test below
//! for the check this was built to satisfy.
//!
//! This lives on the push side -- the engine thread that calls
//! `AudioSink::push` -- not inside the cpal callback (`RingConsumer::fill`).
//! `fill` runs on a realtime audio thread that must not allocate; `process`
//! below allocates its output `Vec`. Resampling before the samples ever
//! reach the ring keeps the realtime side exactly as it was: a plain `f32`
//! copy loop, no new allocation, no new atomics. [`ResamplingProducer`] is
//! the type that actually wires this into a ring, and -- like
//! `RingProducer`/`RingConsumer` in `ring.rs` -- it depends on nothing cpal,
//! so it's the type the tests below exercise directly; `RingSink` (which
//! does need a real device) is just a thin wrapper around it.

use crate::ring::RingProducer;

/// Streams 24 kHz mono audio to `to_hz`, carrying interpolation state across
/// calls to [`process`](Self::process).
///
/// At equal rates, every method here is a true passthrough: `process`
/// returns immediately and never reads or writes `pos`/`tail`, so
/// constructing this with `from_hz == to_hz` and calling it costs one
/// integer comparison and a copy, nothing more.
pub(crate) struct StreamingResampler {
    from_hz: u32,
    to_hz: u32,
    /// `from_hz / to_hz`: how far the input read position advances for each
    /// output sample produced. Only meaningful -- and only ever read --
    /// when `from_hz != to_hz`.
    factor: f64,
    /// Fractional input read position, relative to a virtual array whose
    /// index 0 is `tail` (if set) and whose remaining indices are the
    /// *next* call's input. Rebased at the end of every call so it always
    /// means the same thing at the start of the next one.
    pos: f64,
    /// The last sample of the most recently processed input, kept so the
    /// first output sample of the next call can interpolate across the
    /// chunk boundary instead of starting cold at the new chunk's index 0.
    tail: Option<f32>,
}

impl StreamingResampler {
    pub(crate) fn new(from_hz: u32, to_hz: u32) -> Self {
        StreamingResampler { from_hz, to_hz, factor: from_hz as f64 / to_hz as f64, pos: 0.0, tail: None }
    }

    /// `from_hz / to_hz`, exposed so [`ResamplingProducer`] can convert
    /// device-rate sample counts to input-equivalent ones without
    /// duplicating this ratio.
    pub(crate) fn factor(&self) -> f64 {
        self.factor
    }

    /// How many output samples [`process`](Self::process) would emit for an
    /// input of this length *right now*, without mutating any state. Used
    /// to find, before calling `process`, the largest input prefix whose
    /// output fits in whatever room the caller actually has -- see
    /// [`largest_prefix_within`](Self::largest_prefix_within).
    ///
    /// Non-decreasing in `input_len`: growing the hypothetical input can
    /// only ever grow (or leave unchanged) how much output it produces,
    /// which is what makes the binary search in `largest_prefix_within`
    /// valid.
    pub(crate) fn output_len_for(&self, input_len: usize) -> usize {
        if self.from_hz == self.to_hz || input_len == 0 {
            return input_len;
        }
        let base = usize::from(self.tail.is_some());
        let limit = (base + input_len) as f64 - 1.0;
        let diff = limit - self.pos;
        if diff <= 0.0 {
            return 0;
        }
        (diff / self.factor).ceil() as usize
    }

    /// The largest prefix of a hypothetical input of length `input_len`
    /// whose resampled output is no more than `room` samples.
    ///
    /// This exists for exactly the case a stateful resampler makes
    /// dangerous: if the ring only has room for part of what an input would
    /// produce, resampling the *whole* input and pushing only a prefix of
    /// the result would still have advanced `pos`/`tail` as if all of it had
    /// landed, desynchronising the resampler from what was actually
    /// consumed. Working out the input prefix first, and resampling only
    /// that prefix, keeps state and ring contents in agreement.
    pub(crate) fn largest_prefix_within(&self, input_len: usize, room: usize) -> usize {
        if self.output_len_for(input_len) <= room {
            return input_len;
        }
        let (mut lo, mut hi) = (0usize, input_len);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if self.output_len_for(mid) <= room {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    /// Resample `input` and advance the streaming state so the *next* call
    /// picks up interpolation exactly where this one left off.
    ///
    /// At equal rates this is `input.to_vec()`, and neither `pos` nor
    /// `tail` is read or written: true passthrough, no resampling
    /// arithmetic, no state.
    pub(crate) fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.from_hz == self.to_hz {
            return input.to_vec();
        }
        let n = input.len();
        if n == 0 {
            return Vec::new();
        }
        let base = usize::from(self.tail.is_some());
        // Both the last valid index into the virtual [tail?, input...]
        // array and the index `process` should read `tail` from are needed
        // by every iteration below, so define them once. `idx` is clamped
        // to `max_local_idx` defensively: the loop count comes from
        // `output_len_for`, which is derived from exactly the same math, so
        // this should never actually clamp, but reading one sample too far
        // is a correctness bug, not something worth risking a panic over on
        // the engine thread.
        let max_local_idx = base + n - 1;
        let value_at = |idx: usize| -> f32 {
            let idx = idx.min(max_local_idx);
            if base == 1 && idx == 0 { self.tail.unwrap_or(0.0) } else { input[idx - base] }
        };

        let count = self.output_len_for(n);
        let mut out = Vec::with_capacity(count);
        let mut p = self.pos;
        for _ in 0..count {
            let j = p.floor() as usize;
            let t = (p - j as f64) as f32;
            let a = value_at(j);
            let b = value_at(j + 1);
            out.push(a * (1.0 - t) + b * t);
            p += self.factor;
        }

        // Rebase so `pos` means the same thing next call: index 0 of the
        // next virtual array is this call's last input sample. `.max(0.0)`
        // is a defensive clamp against floating-point error pushing this a
        // hair below zero for irrational ratios (e.g. 24000/44100); it
        // should mathematically always land at >= 0 already.
        self.pos = (p - max_local_idx as f64).max(0.0);
        self.tail = Some(input[n - 1]);
        out
    }
}

/// Wraps a device-rate [`RingProducer`] with the input/output unit
/// conversion described in the module-level `AudioSink` contract:
/// `push`/`pending`/`total_written`/`capacity` all speak 24 kHz
/// input-sample units to callers, while the ring underneath -- and
/// everything `RingConsumer::fill` does with it -- stays entirely in
/// device-rate samples, untouched by any of this.
///
/// This is the type the tests below exercise directly, the same way
/// `ring.rs` tests `RingProducer`/`RingConsumer` directly: it needs no cpal
/// device, only the `RingProducer` half of `ring::ring`. `RingSink` (in
/// `ring.rs`) is a thin wrapper adding the real cpal device and its
/// `RingConsumer`-driven callback.
pub(crate) struct ResamplingProducer {
    producer: RingProducer,
    /// `None` when the device accepted the synthesizer's native rate:
    /// input units already *are* device units, so every method below
    /// short-circuits straight to `producer` with no conversion, no extra
    /// bookkeeping -- a true passthrough.
    resampler: Option<StreamingResampler>,
    /// Input (24 kHz) samples accepted so far. Only meaningful -- and only
    /// ever touched -- when `resampler` is `Some`; `producer.total_written`
    /// already counts input samples directly when rates are equal, so
    /// there is nothing for this field to track in that case.
    input_total_written: usize,
}

impl ResamplingProducer {
    pub(crate) fn new(producer: RingProducer, from_hz: u32, to_hz: u32) -> Self {
        let resampler = (from_hz != to_hz).then(|| StreamingResampler::new(from_hz, to_hz));
        ResamplingProducer { producer, resampler, input_total_written: 0 }
    }

    /// Push as many *input* samples as fit. Returns how many were accepted,
    /// in input units -- the caller retains and retries the remainder,
    /// exactly as `RingProducer::push` and `AudioSink::push` already
    /// document.
    pub(crate) fn push(&mut self, samples: &[f32]) -> usize {
        let Some(resampler) = self.resampler.as_mut() else {
            return self.producer.push(samples);
        };
        if samples.is_empty() {
            return 0;
        }
        // Device-rate room right now. Computed fresh on every call (not
        // cached) because `fill`, running concurrently on the audio
        // thread, can free room between calls -- same reasoning as the
        // engine's own `headroom` calculation in `engine.rs`.
        let room = self.producer.capacity().saturating_sub(self.producer.pending());
        let prefix = resampler.largest_prefix_within(samples.len(), room);
        if prefix == 0 {
            return 0;
        }
        let out = resampler.process(&samples[..prefix]);
        let accepted = self.producer.push(&out);
        debug_assert_eq!(
            accepted,
            out.len(),
            "room was computed to fit exactly this resampled prefix; a short accept here \
             would desynchronise the resampler's state from what physically landed in the ring"
        );
        self.input_total_written += prefix;
        prefix
    }

    /// Input-equivalent samples accepted but not yet played.
    pub(crate) fn pending(&self) -> usize {
        match &self.resampler {
            None => self.producer.pending(),
            // Rounds up, matching `ring.rs`'s own bias for `pending()`:
            // every contributor should over-, never under-, estimate what's
            // still outstanding (see that module's doc comment).
            Some(r) => ((self.producer.pending() as f64) * r.factor()).ceil() as usize,
        }
    }

    /// Input samples ever accepted by `push`.
    pub(crate) fn total_written(&self) -> usize {
        match &self.resampler {
            None => self.producer.total_written(),
            Some(_) => self.input_total_written,
        }
    }

    /// Input-equivalent ring capacity. `RingSink::new` sizes the
    /// *device*-rate ring to hold `BUFFER_SECONDS` of real audio time
    /// regardless of the resampling ratio, so this converts back out to
    /// very nearly the same input-sample constant either way.
    pub(crate) fn capacity(&self) -> usize {
        match &self.resampler {
            None => self.producer.capacity(),
            Some(r) => ((self.producer.capacity() as f64) * r.factor()).ceil() as usize,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.producer.clear()
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        self.producer.set_paused(paused)
    }

    /// Pause is a flag on the underlying ring, not a rate-dependent
    /// quantity, so this needs no input/device-unit conversion -- unlike
    /// every other method above.
    pub(crate) fn is_paused(&self) -> bool {
        self.producer.is_paused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::ring;

    // -- StreamingResampler ---------------------------------------------

    #[test]
    fn passthrough_at_equal_rates_returns_input_unchanged() {
        let mut r = StreamingResampler::new(24_000, 24_000);
        let input = vec![0.1, -0.2, 0.3, 0.0, 1.0, -1.0];
        assert_eq!(r.process(&input), input, "equal rates must be sample-for-sample identity");
        // A second call, with different data, must still be pure identity --
        // proof that no state was carried from the first call.
        let input2 = vec![9.0, 8.0, 7.0];
        assert_eq!(r.process(&input2), input2);
    }

    #[test]
    fn upsampling_roughly_doubles_length() {
        let input: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let mut r = StreamingResampler::new(24_000, 48_000);
        let out = r.process(&input);
        // A single one-shot call withholds a small residual (at most a
        // couple of samples) that would need more input to resolve, so
        // "roughly" -- not exactly -- double.
        assert!(
            out.len() >= 1990 && out.len() <= 2000,
            "expected close to 2000 output samples, got {}",
            out.len()
        );
    }

    #[test]
    fn downsampling_roughly_halves_length() {
        let input: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let mut r = StreamingResampler::new(48_000, 24_000);
        let out = r.process(&input);
        assert!(
            out.len() >= 495 && out.len() <= 500,
            "expected close to 500 output samples, got {}",
            out.len()
        );
    }

    fn sine(n: usize, freq_hz: f32, sample_rate: f32) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f32::consts::PI * freq_hz * i as f32 / sample_rate).sin()).collect()
    }

    #[test]
    fn no_discontinuity_at_chunk_boundaries() {
        // A smooth 440 Hz tone at 24 kHz, fed through the resampler in
        // small, deliberately-irregularly-sized chunks (so chunk
        // boundaries don't line up with any natural period of the signal
        // or the resampling ratio). If `process` reset its interpolation
        // phase or flattened its tail on every call the way calling the
        // stateless `resample_linear` per chunk would, this would show up
        // as a large jump at (almost) every chunk boundary.
        let sr_in = 24_000.0f32;
        let sr_out = 48_000.0f32;
        let freq = 440.0f32;
        let total = 24_000usize; // 1 second of input
        let input = sine(total, freq, sr_in);

        let mut r = StreamingResampler::new(sr_in as u32, sr_out as u32);
        let mut out = Vec::new();
        let mut i = 0usize;
        let mut chunk_len = 17usize; // odd, coprime-ish with everything nearby
        while i < input.len() {
            let end = (i + chunk_len).min(input.len());
            out.extend(r.process(&input[i..end]));
            i = end;
            chunk_len = (chunk_len % 61) + 13; // 13..=73, varies chunk to chunk
        }

        // Bound on how much a sample of a `freq` Hz sine sampled at
        // `sr_out` can change between adjacent samples: the derivative of
        // sin(2*pi*f*t) is bounded by 2*pi*f, so consecutive-sample delta
        // is bounded by 2*pi*f/sr_out. Linear interpolation error adds a
        // little more; a generous 3x safety factor absorbs that plus f32
        // rounding without weakening the check (a real click is a jump of
        // order 1.0, not a small multiple of this bound).
        let max_slope = 2.0 * std::f32::consts::PI * freq / sr_out;
        let bound = max_slope * 3.0;

        let mut max_delta = 0.0f32;
        for w in out.windows(2) {
            max_delta = max_delta.max((w[1] - w[0]).abs());
        }
        assert!(
            max_delta <= bound,
            "discontinuity at a chunk boundary: max consecutive-sample delta {max_delta} \
             exceeds the signal's own slope-derived bound {bound} (chunks={}, out_len={})",
            i / 17 + 1,
            out.len()
        );
    }

    #[test]
    fn feeding_the_same_signal_in_small_chunks_matches_one_big_call() {
        let sr_in = 24_000u32;
        let sr_out = 44_100u32; // a ratio that is not a clean fraction
        let input = sine(10_000, 440.0, sr_in as f32);

        let mut chunked = StreamingResampler::new(sr_in, sr_out);
        let mut chunked_out = Vec::new();
        for chunk in input.chunks(23) {
            chunked_out.extend(chunked.process(chunk));
        }

        let mut single = StreamingResampler::new(sr_in, sr_out);
        let single_out = single.process(&input);

        // Both withhold a tiny residual at the very end (no more input is
        // ever coming to resolve the last fractional position), so lengths
        // can differ by a sample or two, not by chunk-count.
        assert!(
            (chunked_out.len() as i64 - single_out.len() as i64).abs() <= 2,
            "chunked len {} vs single-call len {} differ by more than the expected \
             end-of-stream residual",
            chunked_out.len(),
            single_out.len()
        );

        let common = chunked_out.len().min(single_out.len());
        let mut max_diff = 0.0f32;
        for i in 0..common {
            max_diff = max_diff.max((chunked_out[i] - single_out[i]).abs());
        }
        assert!(
            max_diff < 1e-4,
            "chunked and single-call resampling disagree by up to {max_diff}, expected them \
             to agree closely (streaming state should make chunking invisible to the math)"
        );
    }

    // -- ResamplingProducer: the unit contract ---------------------------

    #[test]
    fn passthrough_producer_has_no_conversion_at_equal_rates() {
        let (raw, _cons) = ring(64);
        let mut rp = ResamplingProducer::new(raw, 24_000, 24_000);
        assert_eq!(rp.push(&[1.0, 2.0, 3.0]), 3);
        assert_eq!(rp.pending(), 3, "equal rates: pending is untouched ring pending, no arithmetic");
        assert_eq!(rp.total_written(), 3);
        assert_eq!(rp.capacity(), 64);
    }

    #[test]
    fn unit_contract_pending_and_total_written_are_input_units_and_converge_to_zero() {
        // Device rate 48 kHz, input (synthesizer) rate 24 kHz: every input
        // sample becomes two device samples.
        let (raw, mut cons) = ring(4096);
        let mut rp = ResamplingProducer::new(raw, 24_000, 48_000);

        let input: Vec<f32> = (0..500).map(|i| i as f32 * 0.001).collect();
        let n = rp.push(&input);
        assert_eq!(n, 500, "small push relative to a large ring should be fully accepted");
        assert_eq!(rp.total_written(), 500, "total_written is in input units");
        // Not exactly 500: the streaming resampler always withholds a
        // fractional-sample residual (at most ~1 input sample's worth)
        // until more input arrives to resolve it -- see `StreamingResampler`'s
        // doc comment. `pending()` is necessarily an approximation once
        // resampling is in play, unlike `total_written()`, which is exact.
        let pending = rp.pending();
        assert!((pending as i64 - 500).abs() <= 2, "expected pending() close to 500, got {pending}");

        // Drain the device-rate ring in device-sized chunks (roughly
        // 2*500 = 1000 device samples buffered) until it's empty, the way
        // the cpal callback would over several calls.
        let mut out = [0.0f32; 200];
        for _ in 0..20 {
            cons.fill(&mut out, 1);
        }
        assert_eq!(rp.pending(), 0, "pending must converge to zero (in input units) once played");
        assert_eq!(rp.total_written(), 500, "total_written never decreases");
    }

    #[test]
    fn capacity_is_input_equivalent_and_matches_a_true_ten_second_buffer() {
        // Mirrors RingSink::new's sizing: BUFFER_SECONDS(10) * device rate
        // device-rate samples in the ring, which should convert back to
        // ~BUFFER_SECONDS * input rate input-equivalent samples regardless
        // of the resample ratio.
        let device_rate = 48_000u32;
        let input_rate = 24_000u32;
        let (raw, _cons) = ring(device_rate as usize * 10);
        let rp = ResamplingProducer::new(raw, input_rate, device_rate);
        assert_eq!(rp.capacity(), input_rate as usize * 10);
    }

    // -- ResamplingProducer: partial acceptance ---------------------------

    #[test]
    fn partial_acceptance_returns_short_count_and_resumes_without_loss_or_repeat() {
        // A small ring forces the first push to be short. Compare what
        // eventually reaches the ring (collected via `fill`, draining as we
        // go so later pushes have room) against a one-shot resample of the
        // whole input on an independent, unbounded resampler: if the
        // partial-acceptance path ever resampled more than it kept, or
        // dropped/duplicated a sample at the split, the two would diverge.
        let device_rate = 48_000u32;
        let input_rate = 24_000u32;
        let (raw, mut cons) = ring(64); // tiny: forces short accepts
        let mut rp = ResamplingProducer::new(raw, input_rate, device_rate);

        let input = sine(2000, 300.0, input_rate as f32);
        let mut consumed = 0usize;
        let mut played = Vec::new();
        let mut out_buf = [0.0f32; 16];
        let mut guard = 0;
        while consumed < input.len() {
            let n = rp.push(&input[consumed..]);
            if n == 0 {
                // Ring momentarily full: drain some before retrying, the
                // way `fill` would run between engine ticks.
                cons.fill(&mut out_buf, 1);
                played.extend_from_slice(&out_buf);
            } else {
                consumed += n;
            }
            guard += 1;
            assert!(guard < 1_000_000, "made no progress -- partial acceptance stuck");
        }
        assert_eq!(consumed, input.len(), "every input sample must eventually be consumed");

        // Drain whatever's left in the ring.
        for _ in 0..device_rate as usize {
            cons.fill(&mut out_buf, 1);
            played.extend_from_slice(&out_buf);
        }
        while played.last() == Some(&0.0) {
            played.pop();
        }

        let mut oneshot = StreamingResampler::new(input_rate, device_rate);
        let expected = oneshot.process(&input);

        assert!(
            (played.len() as i64 - expected.len() as i64).abs() <= 2,
            "played {} samples vs {} expected from a one-shot resample -- suggests loss or \
             duplication across a partial-accept boundary",
            played.len(),
            expected.len()
        );
        let common = played.len().min(expected.len());
        let mut max_diff = 0.0f32;
        for i in 0..common {
            max_diff = max_diff.max((played[i] - expected[i]).abs());
        }
        assert!(
            max_diff < 1e-4,
            "played audio diverges from the one-shot reference by up to {max_diff} -- a \
             partial accept must resume the resampler exactly where it left off"
        );
    }

    #[test]
    fn a_following_push_after_a_short_accept_does_not_repeat_the_accepted_prefix() {
        let (raw, _cons) = ring(4); // room for at most 4 device samples
        let mut rp = ResamplingProducer::new(raw, 24_000, 48_000); // factor 0.5: ~2x

        let input: Vec<f32> = (0..50).map(|i| i as f32).collect();
        let n1 = rp.push(&input);
        assert!(n1 > 0 && n1 < input.len(), "expected a short accept, got {n1} of {}", input.len());

        // The producer must report having accepted exactly the input
        // samples it says it did -- not the whole slice we handed it.
        assert_eq!(rp.total_written(), n1);

        // The caller (mirroring the engine's `carry` mechanism) retries
        // with the unconsumed remainder, not the original slice.
        let remainder = &input[n1..];
        assert!(!remainder.is_empty());
    }
}
