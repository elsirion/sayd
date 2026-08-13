//! Sample-rate and tempo adjustment. Pure Rust, no subprocess.

use crate::SAMPLE_RATE;

/// Linear resample from `from_hz` to `to_hz`. Used only when the audio device
/// refuses 24 kHz; it changes pitch, so it must not be used for tempo.
pub fn resample_linear(a: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || a.is_empty() {
        return a.to_vec();
    }
    let factor = from_hz as f32 / to_hz as f32;
    resample_by(a, factor)
}

fn resample_by(a: &[f32], factor: f32) -> Vec<f32> {
    if (factor - 1.0).abs() < 1e-6 || a.is_empty() {
        return a.to_vec();
    }
    let n = (a.len() as f32 / factor) as usize;
    (0..n)
        .map(|i| {
            let x = i as f32 * factor;
            let j = x as usize;
            if j + 1 >= a.len() {
                *a.last().unwrap_or(&0.0)
            } else {
                let t = x - j as f32;
                a[j] * (1.0 - t) + a[j + 1] * t
            }
        })
        .collect()
}

/// WSOLA time-stretch: change tempo without changing pitch.
///
/// Not on sayd's default path -- speed is applied via Kokoro's own `speed`
/// input at synthesis time, which sounds better. Kept because it is the only
/// way to change tempo of already-synthesized audio.
pub fn time_stretch(a: &[f32], factor: f32) -> Vec<f32> {
    if (factor - 1.0).abs() < 1e-6 || a.is_empty() {
        return a.to_vec();
    }
    let sr = SAMPLE_RATE as usize;
    let frame = sr * 40 / 1000; // 40 ms analysis window
    let syn_hop = frame / 2; // 50% overlap on output
    let ana_hop = (syn_hop as f32 * factor) as usize;
    let search = sr * 8 / 1000; // +/- 8 ms alignment search
    if a.len() < frame + search * 2 + ana_hop {
        return resample_by(a, factor);
    }

    let win: Vec<f32> = (0..frame)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / frame as f32).cos())
        .collect();

    let out_len = (a.len() as f32 / factor) as usize + frame;
    let mut out = vec![0f32; out_len];
    let mut norm = vec![0f32; out_len];

    let mut ana = 0usize;
    let mut syn = 0usize;
    let mut target: Vec<f32> = a[..frame].to_vec();

    while ana + frame + search < a.len() && syn + frame < out_len {
        let lo = ana.saturating_sub(search);
        let hi = (ana + search).min(a.len() - frame);
        let mut best = ana;
        let mut best_score = f32::NEG_INFINITY;
        let mut off = lo;
        while off <= hi {
            let mut acc = 0f32;
            let mut i = 0;
            while i < frame {
                acc += a[off + i] * target[i];
                i += 8; // subsample the correlation; 8x cheaper, same pick
            }
            if acc > best_score {
                best_score = acc;
                best = off;
            }
            off += 4;
        }

        for i in 0..frame {
            out[syn + i] += a[best + i] * win[i];
            norm[syn + i] += win[i];
        }

        let nxt = best + syn_hop;
        target = if nxt + frame <= a.len() {
            a[nxt..nxt + frame].to_vec()
        } else {
            vec![0.0; frame]
        };
        ana += ana_hop;
        syn += syn_hop;
    }

    for i in 0..out.len() {
        if norm[i] > 1e-6 {
            out[i] /= norm[i];
        }
    }
    out.truncate(syn.max(1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_is_identity_at_the_same_rate() {
        let a = vec![0.1, 0.2, 0.3];
        assert_eq!(resample_linear(&a, 24_000, 24_000), a);
    }

    #[test]
    fn resample_halves_length_when_doubling_rate_down() {
        let a: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = resample_linear(&a, 48_000, 24_000);
        assert_eq!(out.len(), 50);
    }

    #[test]
    fn time_stretch_is_identity_at_factor_one() {
        let a = vec![0.1, 0.2, 0.3];
        assert_eq!(time_stretch(&a, 1.0), a);
    }

    #[test]
    fn time_stretch_shortens_when_speeding_up() {
        let a: Vec<f32> = (0..48_000).map(|i| (i as f32 / 50.0).sin()).collect();
        let out = time_stretch(&a, 1.5);
        assert!(out.len() < a.len(), "expected shorter, got {} vs {}", out.len(), a.len());
        assert!(out.len() > a.len() / 3, "unreasonably short: {}", out.len());
    }

    #[test]
    fn time_stretch_handles_empty_and_tiny_input() {
        assert!(time_stretch(&[], 1.5).is_empty());
        assert!(!time_stretch(&[0.1, 0.2], 1.5).is_empty());
    }
}
