//! The generation loop against the ONNX reference wrapper (temperature 0) and
//! against the PyTorch package (pauses), plus the package's audio handling.
//!
//! The real-model tests are `#[ignore]`d; run them with
//! `SAYD_POCKET_BUNDLE=<bundle dir> cargo test --release -p sayd-pocket -- --ignored`.
//! The voice is the crate's `FENRIR_WAV`. `SAYD_POCKET_WAV_OUT` makes the
//! pause test save its first generation.

use std::path::{Path, PathBuf};
use std::time::Instant;

use sayd_pocket::rng::Gauss;
use sayd_pocket::text::{prepare_text_prompt, split_into_best_sentences};
use sayd_pocket::tokenizer::Tokenizer;
use sayd_pocket::{end_on_pause, fade_in, init_environment, npy, PocketTts, Precision, FENRIR_WAV};
use sayd_pocket::{VoiceEmbedding, VoiceState};

const SR: u32 = 24_000;

/// `lines.json`'s "joke" line from `.tts-bench`, the one the spike measured.
const JOKE: &str = "Good news and bad news. Good news: I found the bug! Bad news: it was in \
                    your commit called quick fix. From three weeks ago.";

/// Mean pause total (`pause_total`) of the PyTorch package (pocket-tts 4.0.1,
/// english_2026-04 cloning weights, temperature 0.3) speaking `JOKE` with
/// `FENRIR_WAV`, over `torch.manual_seed(0..10)`. Measured by
/// `tests/fixtures/measure_package_pauses.py`; per seed 2.32, 1.46, 3.06, 1.60,
/// 2.14, 1.78, 2.48, 2.16, 2.82, 1.90 s, mean 2.172 s.
const PACKAGE_MEAN_PAUSE_SECS: f32 = 2.17;

/// The spike's wrapper, fed the clip without `end_on_pause`, paused about 4 s
/// on `JOKE`; a mean above this is that regression back.
const MAX_MEAN_PAUSE_SECS: f32 = 3.0;

/// The first 3.12 s of `FENRIR_WAV` (it pauses there): the voice of the
/// golden fixtures, short enough for `loop_voice.npy` to stay small.
const FIXTURE_CLIP_SAMPLES: usize = 74_880;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The bundle the `#[ignore]`d tests run against. They fail rather than
/// pass silently without it: running them is asking for the real model.
fn bundle() -> PathBuf {
    std::env::var_os("SAYD_POCKET_BUNDLE")
        .map(PathBuf::from)
        .expect("set SAYD_POCKET_BUNDLE to a Pocket TTS bundle directory to run this test")
}

fn load(dir: &Path, p: Precision) -> PocketTts {
    init_environment().unwrap();
    PocketTts::load(dir, p, 4).unwrap()
}

/// Mono 16-bit PCM WAV, as `[-1, 1)` floats.
fn parse_wav(raw: &[u8]) -> (u32, Vec<f32>) {
    assert!(&raw[..4] == b"RIFF" && &raw[8..12] == b"WAVE", "not a WAV");
    let (mut rate, mut pos) = (0, 12);
    while pos + 8 <= raw.len() {
        let id = &raw[pos..pos + 4];
        let len = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &raw[pos + 8..pos + 8 + len];
        if id == b"fmt " {
            let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
            assert_eq!(u16_at(0), 1, "not PCM");
            assert_eq!(u16_at(2), 1, "not mono");
            assert_eq!(u16_at(14), 16, "not 16-bit");
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
        } else if id == b"data" {
            let samples = body
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect();
            return (rate, samples);
        }
        pos += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

fn write_wav(path: &Path, samples: &[f32]) {
    let data: Vec<u8> = samples
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())
        .collect();
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend(b"RIFF");
    out.extend((36 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes()); // PCM
    out.extend(1u16.to_le_bytes()); // mono
    out.extend(SR.to_le_bytes());
    out.extend((SR * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, out).unwrap();
}

/// Seconds of silence: 20 ms frames below -40 dBFS, counted in runs of at
/// least 250 ms.
fn pause_total(audio: &[f32]) -> f32 {
    let frame = SR as usize / 50;
    let mut total = 0;
    let mut run = 0;
    let silent = audio.chunks_exact(frame).map(|f| {
        let rms = (f.iter().map(|s| s * s).sum::<f32>() / frame as f32).sqrt();
        20.0 * (rms + 1e-12).log10() < -40.0
    });
    for s in silent.chain([false]) {
        if s {
            run += 1;
        } else {
            if run * 20 >= 250 {
                total += run;
            }
            run = 0;
        }
    }
    total as f32 * 0.02
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

fn fenrir() -> Vec<f32> {
    let (rate, clip) = parse_wav(FENRIR_WAV);
    assert_eq!(rate, SR);
    clip
}

fn fixture_clip() -> Vec<f32> {
    let mut clip = fenrir();
    clip.truncate(FIXTURE_CLIP_SAMPLES);
    clip
}

fn fixture_voice(tts: &mut PocketTts) -> VoiceState {
    let clip = fixture_clip();
    let e = tts.encode_voice(&clip).unwrap();
    tts.voice_state(&e).unwrap()
}

// --- package audio handling, no model needed ---

#[test]
fn end_on_pause_trims_fades_and_pads() {
    // 0.5 s of tone, then 0.3 s of silence.
    let tone: Vec<f32> = (0..12_000)
        .map(|i| 0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / SR as f32).sin())
        .collect();
    let mut clip = tone.clone();
    clip.extend(std::iter::repeat_n(0.0, 7_200));

    let out = end_on_pause(&clip, SR);
    // Cut after the last loud 20 ms frame, plus 80 ms of zeros.
    assert_eq!(out.len(), 12_000 + 1_920);
    // Untouched up to the last 20 ms, which ramp linearly from 1 to 0.
    assert_eq!(out[..11_520], tone[..11_520]);
    for i in 0..480 {
        let gain = 1.0 - i as f32 / 479.0;
        assert!(
            (out[11_520 + i] - tone[11_520 + i] * gain).abs() < 1e-6,
            "sample {i}"
        );
    }
    assert_eq!(out[11_999], 0.0);
    assert!(out[12_000..].iter().all(|&s| s == 0.0));

    // A clip that ends on speech is only faded and padded.
    let out = end_on_pause(&tone, SR);
    assert_eq!(out.len(), 12_000 + 1_920);
    assert_eq!(out[..11_520], tone[..11_520]);

    // Shorter than one 20 ms frame: left alone.
    assert_eq!(end_on_pause(&tone[..100], SR), tone[..100]);
}

#[test]
fn fade_in_is_5ms_linear() {
    let mut a = vec![1.0f32; 1_000];
    fade_in(&mut a, SR);
    // 5 ms at 24 kHz is 120 samples, ramping 0..=1.
    for (i, &v) in a[..120].iter().enumerate() {
        assert!((v - i as f32 / 119.0).abs() < 1e-6, "sample {i}: {v}");
    }
    assert_eq!(a[0], 0.0);
    assert!(a[119..].iter().all(|&v| v == 1.0));

    let mut short = vec![1.0f32; 50];
    fade_in(&mut short, SR);
    assert!((short[49] - 49.0 / 119.0).abs() < 1e-6);
}

#[test]
fn gauss_is_deterministic_per_seed() {
    let draw = |seed, n, std| {
        let mut v = vec![0.0f32; n];
        Gauss::new(seed).fill(&mut v, std);
        v
    };
    assert_eq!(draw(7, 1_000, 1.0), draw(7, 1_000, 1.0));
    assert_ne!(draw(7, 1_000, 1.0), draw(8, 1_000, 1.0));
    assert!(draw(7, 64, 0.0).iter().all(|&v| v == 0.0));

    // N(0, std²): check the first two moments.
    let std = 0.3f32.sqrt();
    let v = draw(1, 200_000, std);
    let mean = v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
    let var = v.iter().map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / v.len() as f64;
    assert!(mean.abs() < 0.01, "mean {mean}");
    assert!((var - 0.3).abs() < 0.01, "variance {var}");
}

#[test]
fn voice_embedding_round_trips_through_bytes() {
    let e = VoiceEmbedding {
        frames: 2,
        data: (0..6).map(|i| i as f32 * 0.5 - 1.0).collect(),
    };
    let bytes = e.to_bytes();
    let back = VoiceEmbedding::from_bytes(&bytes).unwrap();
    assert_eq!(back.frames, 2);
    assert_eq!(back.data, e.data);
    assert!(VoiceEmbedding::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    assert!(VoiceEmbedding::from_bytes(b"nonsense").is_err());
}

// --- real model ---

#[test]
#[ignore = "needs SAYD_POCKET_BUNDLE pointing at a bundle dir"]
fn voice_embedding_matches_reference() {
    let dir = bundle();
    let mut tts = load(&dir, Precision::Fp32);
    let clip = fixture_clip();
    let e = tts.encode_voice(&clip).unwrap();
    let (shape, want) = npy::read_f32(&fixtures().join("loop_voice.npy")).unwrap();
    assert_eq!(shape, [e.frames, want.len() / e.frames]);
    assert_eq!(e.data.len(), want.len());
    let diff = max_abs_diff(&e.data, &want);
    println!(
        "voice embedding: {} frames, max abs diff {diff:e}",
        e.frames
    );
    assert!(diff < 1e-4, "max abs diff {diff}");
}

#[test]
#[ignore = "needs SAYD_POCKET_BUNDLE pointing at a bundle dir"]
fn latents_match_reference_at_temperature_zero() {
    let dir = bundle();
    let mut tts = load(&dir, Precision::Fp32);
    let voice = fixture_voice(&mut tts);
    let cases: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures().join("loop_cases.json")).unwrap())
            .unwrap();
    for case in cases["sentences"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let (shape, want) =
            npy::read_f32(&fixtures().join(case["latents"].as_str().unwrap())).unwrap();
        let got = tts.latents(&voice, text, 0.0, &mut Gauss::new(0)).unwrap();
        let frames = got.len() / shape[1];
        println!(
            "{text:?}: {frames} frames (reference {}), max abs diff {:e}",
            shape[0],
            max_abs_diff(&got, &want)
        );
        assert_eq!(frames, shape[0], "frame count for {text:?}");
        let diff = max_abs_diff(&got, &want);
        assert!(diff < 1e-3, "{text:?}: max abs diff {diff}");
    }
}

#[test]
#[ignore = "needs SAYD_POCKET_BUNDLE pointing at a bundle dir"]
fn pause_total_matches_package_within_half_a_second() {
    let dir = bundle();
    let mut tts = load(&dir, Precision::Int8);
    let e = tts.encode_voice(&fenrir()).unwrap();
    let voice = tts.voice_state(&e).unwrap();

    // One sample is too noisy: the package itself spans 1.46-3.06 s.
    let mut totals = Vec::new();
    for seed in 0..5 {
        let audio = tts.generate(&voice, JOKE, 0.3, seed).unwrap();
        if seed == 0 {
            if let Some(out) = std::env::var_os("SAYD_POCKET_WAV_OUT") {
                write_wav(Path::new(&out), &audio);
            }
        }
        totals.push(pause_total(&audio));
        println!(
            "joke, seed {seed}: {:.2} s of audio, {:.2} s of pauses",
            audio.len() as f32 / SR as f32,
            totals[seed as usize]
        );
    }
    let mean = totals.iter().sum::<f32>() / totals.len() as f32;
    println!("mean {mean:.2} s of pauses (package {PACKAGE_MEAN_PAUSE_SECS} s)");
    assert!(
        (mean - PACKAGE_MEAN_PAUSE_SECS).abs() <= 0.5,
        "mean {mean:.2} s of pauses against the package's {PACKAGE_MEAN_PAUSE_SECS} s"
    );
    assert!(
        mean < MAX_MEAN_PAUSE_SECS,
        "mean {mean:.2} s of pauses: the spike's long-pause regression"
    );
}

/// Seconds of silence (20 ms frames below -40 dBFS) at the start of `audio`,
/// or at its end when `from_end`.
fn edge_silence(audio: &[f32], from_end: bool) -> f32 {
    let frame = SR as usize / 50;
    let silent = |f: &[f32]| {
        let rms = (f.iter().map(|s| s * s).sum::<f32>() / frame as f32).sqrt();
        20.0 * (rms + 1e-12).log10() < -40.0
    };
    let n = if from_end {
        audio.rchunks_exact(frame).take_while(|f| silent(f)).count()
    } else {
        audio.chunks_exact(frame).take_while(|f| silent(f)).count()
    };
    n as f32 * 0.02
}

#[test]
#[ignore = "needs SAYD_POCKET_BUNDLE pointing at a bundle dir"]
fn generate_runs_each_chunk_from_the_voice_state() {
    let dir = bundle();
    // lines.json's "joke" and "sarcasm": 35 + 26 tokens, over the 50-token budget.
    let text = format!(
        "{JOKE} Oh, wonderful. The tests passed on the first try. I'm sure that won't \
         come back to haunt us at all."
    );
    let mut tts = load(&dir, Precision::Int8);
    let voice = fixture_voice(&mut tts);
    // Short enough that `chunks` is the package's split.
    let tok = Tokenizer::load(&dir.join("tokenizer.model")).unwrap();
    let chunks = tts.chunks(&voice, &text);
    assert_eq!(chunks, split_into_best_sentences(&tok, &text, 50));
    assert!(chunks.len() >= 2, "{chunks:?}");
    println!("chunks: {chunks:?}");
    let audio = tts.generate(&voice, &text, 0.3, 1).unwrap();

    // The same, chunk by chunk, sharing one noise stream.
    let mut rng = Gauss::new(1);
    let mut pieces = Vec::new();
    for c in &chunks {
        let latents = tts.latents(&voice, c, 0.3, &mut rng).unwrap();
        pieces.push(tts.decode(&latents).unwrap());
    }
    assert_eq!(audio, pieces.concat());
    for (i, w) in pieces.windows(2).enumerate() {
        let seam = edge_silence(&w[0], true) + edge_silence(&w[1], false);
        println!(
            "chunks {i}/{}: {:.2} s + {:.2} s of audio, {seam:.2} s of silence at the seam",
            i + 1,
            w[0].len() as f32 / SR as f32,
            w[1].len() as f32 / SR as f32
        );
        // The spike's wrapper left 0.7-1.2 s between sentences.
        assert!(seam < 0.7, "seam {i}: {seam:.2} s of silence");
    }
    println!("{:.2} s of pauses in all", pause_total(&audio));
}

#[test]
#[ignore = "needs SAYD_POCKET_BUNDLE pointing at a bundle dir"]
fn long_unpunctuated_run_is_split_to_fit_and_spoken() {
    let dir = bundle();
    // No `.!?` or `,;:`: the package's splitter keeps it as one chunk, too
    // long for the attention cache after the 9 s fenrir prompt.
    let phrase = "the build server kept running every test in the suite while the team \
                  waited for the results to come back from the cluster and nobody said a word";
    let text = std::iter::repeat_n(phrase, 7)
        .collect::<Vec<_>>()
        .join(" and ");
    let tok = Tokenizer::load(&dir.join("tokenizer.model")).unwrap();
    let tokens = tok.encode(&text).len();
    assert!((230..=280).contains(&tokens), "{tokens} tokens");
    assert_eq!(split_into_best_sentences(&tok, &text, 50).len(), 1);

    let mut tts = load(&dir, Precision::Int8);
    let e = tts.encode_voice(&fenrir()).unwrap();
    let voice = tts.voice_state(&e).unwrap();
    let chunks = tts.chunks(&voice, &text);
    assert!(chunks.len() >= 2, "{chunks:?}");
    // Split at word boundaries only: every word of the prepared text kept, in order.
    let words = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(
        words(&chunks.join(" ")),
        words(&prepare_text_prompt(&text).0)
    );

    let audio = tts.generate(&voice, &text, 0.3, 0).unwrap();
    let secs = audio.len() as f32 / SR as f32;
    println!(
        "{tokens} tokens in {} chunks ({:?} tokens): {secs:.2} s of audio",
        chunks.len(),
        chunks
            .iter()
            .map(|c| tok.encode(c).len())
            .collect::<Vec<_>>()
    );
    // The package speaks one to three repeats of `phrase` at 0.18-0.20 s per
    // token (it runs all seven past its frame limit, without EOS). Well under
    // that means chunks were cut off.
    assert!(
        secs > tokens as f32 * 0.12,
        "{secs:.2} s for {tokens} tokens"
    );
}

#[test]
#[ignore = "needs SAYD_POCKET_BUNDLE pointing at a bundle dir"]
fn realtime_factor_under_one() {
    let dir = bundle();
    // 20 words.
    let text = "The daemon reads a line from the socket, strips the framing, and hands \
                the text to the engine at once.";
    assert_eq!(text.split_whitespace().count(), 20);
    for p in [Precision::Int8, Precision::Fp32] {
        let mut tts = load(&dir, p);
        let voice = fixture_voice(&mut tts);
        tts.generate(&voice, "Warm up.", 0.3, 1).unwrap();
        let t = Instant::now();
        let audio = tts.generate(&voice, text, 0.3, 1).unwrap();
        let gen_secs = t.elapsed().as_secs_f32();
        let audio_secs = audio.len() as f32 / SR as f32;
        let rtf = gen_secs / audio_secs;
        println!("{p:?}: {gen_secs:.2} s for {audio_secs:.2} s of audio, RTF {rtf:.3}");
        if p == Precision::Int8 {
            assert!(rtf < 1.0, "int8 RTF {rtf}");
        }
    }
}
