//! How the engine hands samples to the speakers.
//!
//! The engine only ever pushes and asks how much is outstanding; it never
//! blocks on the audio device. The real implementation lives in the `sayd`
//! binary because it needs cpal; `VecSink` here lets tests assert on exactly
//! what the engine produced.

pub trait AudioSink: Send {
    /// Push as many samples as fit. Returns how many were accepted; the
    /// caller must retain the remainder and try again.
    fn push(&mut self, samples: &[f32]) -> usize;

    /// Samples accepted but not yet played.
    fn pending(&self) -> usize;

    /// Discard everything not yet played.
    fn clear(&mut self);

    fn set_paused(&mut self, paused: bool);

    /// Total buffer size, so the engine knows how far ahead it may run.
    fn capacity(&self) -> usize;

    /// Total samples ever accepted, for tests and diagnostics.
    fn total_written(&self) -> usize;
}

/// Test double. Records every sample ever pushed in `written` while modelling
/// a fixed-capacity buffer that `drain` empties to simulate playback.
pub struct VecSink {
    capacity: usize,
    queued: usize,
    pub written: Vec<f32>,
    pub paused: bool,
}

impl VecSink {
    pub fn new(capacity: usize) -> Self {
        VecSink { capacity, queued: 0, written: Vec::new(), paused: false }
    }

    /// Pretend `n` samples have been played.
    pub fn drain(&mut self, n: usize) {
        self.queued = self.queued.saturating_sub(n);
    }
}

impl AudioSink for VecSink {
    fn push(&mut self, samples: &[f32]) -> usize {
        let room = self.capacity.saturating_sub(self.queued);
        let n = room.min(samples.len());
        self.written.extend_from_slice(&samples[..n]);
        self.queued += n;
        n
    }

    fn pending(&self) -> usize {
        self.queued
    }

    fn clear(&mut self) {
        self.queued = 0;
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn total_written(&self) -> usize {
        self.written.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_sink_accepts_up_to_capacity() {
        let mut s = VecSink::new(4);
        assert_eq!(s.push(&[1.0, 2.0]), 2);
        assert_eq!(s.push(&[3.0, 4.0, 5.0]), 2, "only what fits is accepted");
        assert_eq!(s.pending(), 4);
    }

    #[test]
    fn vec_sink_records_everything_written() {
        let mut s = VecSink::new(100);
        s.push(&[1.0, 2.0]);
        s.push(&[3.0]);
        assert_eq!(s.written, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn vec_sink_clear_empties_pending_but_keeps_the_record() {
        let mut s = VecSink::new(100);
        s.push(&[1.0, 2.0]);
        s.clear();
        assert_eq!(s.pending(), 0);
        assert_eq!(s.written, vec![1.0, 2.0], "written is the test's transcript");
    }

    #[test]
    fn vec_sink_tracks_pause_state() {
        let mut s = VecSink::new(4);
        assert!(!s.paused);
        s.set_paused(true);
        assert!(s.paused);
    }

    #[test]
    fn vec_sink_drain_simulates_playback() {
        let mut s = VecSink::new(10);
        s.push(&[1.0, 2.0, 3.0]);
        s.drain(2);
        assert_eq!(s.pending(), 1);
    }

    #[test]
    fn vec_sink_total_written_survives_clear_and_drain() {
        let mut s = VecSink::new(10);
        s.push(&[1.0, 2.0, 3.0]);
        s.drain(2);
        s.push(&[4.0]);
        s.clear();
        assert_eq!(s.total_written(), 4, "total_written never goes down");
    }
}
