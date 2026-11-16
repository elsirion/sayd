//! The Pocket TTS `Synthesizer`: a voice is a WAV clip in
//! `<models>/pocket/voices`, encoded once and cached by the clip's hash.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sayd_core::config::Config;
use sayd_core::synth::Synthesizer;
use sayd_kokoro::audio::{resample_linear, time_stretch};
use sayd_pocket::{PocketTts, Precision, VoiceEmbedding, VoiceState};
use sha2::{Digest, Sha256};

/// Sample rate Pocket's voice encoder and decoder work at.
// Nothing calls this until the engine selection lands.
#[allow(dead_code)]
const CLIP_RATE: u32 = 24_000;

// Nothing calls this until the engine selection lands.
#[allow(dead_code)]
pub struct PocketSynthesizer {
    models_dir: PathBuf,
    cache_dir: PathBuf,
    precision: Precision,
    threads: usize,
    temperature: f32,
    /// Next sampling seed; counting up makes repeated lines differ the way
    /// the package's own sampling does.
    seed: u64,
    session: Option<PocketTts>,
    /// Conditioned voices of the live session: voice name -> (clip hash, state).
    states: HashMap<String, (String, VoiceState)>,
}

/// `<models>/pocket/voices`: one `<name>.wav` reference clip per voice.
// Nothing calls this until the engine selection lands.
#[allow(dead_code)]
pub fn voices_dir(models_dir: &Path) -> PathBuf {
    models_dir.join("pocket").join("voices")
}

/// Voice names (the `*.wav` stems in the voices directory), sorted.
// Only tests call this until the settings window's Pocket voice list does.
#[allow(dead_code)]
pub fn list_pocket_voices(models_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(voices_dir(models_dir))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "wav"))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    v.sort();
    v
}

/// The files of the downloaded (int8) bundle, relative to
/// `sayd_pocket::bundle_dir`. The one list: the download builds its assets
/// from it and [`bundle_installed`] checks it.
// Nothing calls this until the engine selection lands.
#[allow(dead_code)]
pub const BUNDLE_FILES: [&str; 8] = [
    "bundle.json",
    "tokenizer.model",
    "bos_before_voice.npy",
    "text_conditioner.onnx",
    "mimi_encoder.onnx",
    "flow_lm_main_int8.onnx",
    "flow_lm_flow_int8.onnx",
    "mimi_decoder_int8.onnx",
];

/// Whether every file of the downloaded bundle is present and non-empty
/// below `models_dir`. A directory with only some of them (a cancelled
/// download) is not installed.
// Nothing calls this until the engine selection lands.
#[allow(dead_code)]
pub fn bundle_installed(models_dir: &Path) -> bool {
    let dir = sayd_pocket::bundle_dir(models_dir);
    BUNDLE_FILES
        .iter()
        .all(|f| std::fs::metadata(dir.join(f)).is_ok_and(|m| m.is_file() && m.len() > 0))
}

/// `$XDG_CACHE_HOME/sayd/pocket`, falling back to `~/.cache/sayd/pocket`.
// Nothing calls this until the engine selection lands.
#[allow(dead_code)]
pub fn default_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("sayd").join("pocket")
}

/// Decode WAV bytes (16-bit int or 32-bit float, any rate, mono or
/// multi-channel) to mono f32 at 24 kHz.
// Nothing calls this until the engine selection lands.
#[allow(dead_code)]
fn decode_clip(bytes: &[u8], what: &str) -> Result<Vec<f32>, String> {
    let mut r = hound::WavReader::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("{what}: not a readable WAV: {e}"))?;
    let spec = r.spec();
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => r
            .samples::<i16>()
            .map(|s| s.map(|x| x as f32 / 32768.0))
            .collect::<Result<_, _>>(),
        (hound::SampleFormat::Float, 32) => r.samples::<f32>().collect::<Result<_, _>>(),
        (f, b) => {
            return Err(format!(
                "{what}: unsupported WAV format ({f:?}, {b} bit); use 16-bit PCM or 32-bit float"
            ))
        }
    }
    .map_err(|e| format!("{what}: bad WAV data: {e}"))?;
    let ch = spec.channels.max(1) as usize;
    let mono: Vec<f32> = if ch == 1 {
        interleaved
    } else {
        interleaved
            .chunks_exact(ch)
            .map(|f| f.iter().sum::<f32>() / ch as f32)
            .collect()
    };
    if mono.is_empty() {
        return Err(format!("{what}: the clip has no audio"));
    }
    Ok(resample_linear(&mono, spec.sample_rate, CLIP_RATE))
}

// Nothing calls this until the engine selection lands.
#[allow(dead_code)]
impl PocketSynthesizer {
    pub fn new(models_dir: &Path, cache_dir: &Path, cfg: &Config) -> PocketSynthesizer {
        PocketSynthesizer {
            models_dir: models_dir.to_path_buf(),
            cache_dir: cache_dir.to_path_buf(),
            precision: Precision::from_config(&cfg.pocket.precision),
            threads: cfg.threads,
            temperature: cfg.pocket.temperature,
            seed: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            session: None,
            states: HashMap::new(),
        }
    }

    fn ensure_session(&mut self) -> Result<&mut PocketTts, String> {
        if self.session.is_none() {
            let dir = sayd_pocket::bundle_dir(&self.models_dir);
            if !dir.is_dir() {
                return Err(format!("Pocket TTS bundle not found at {}", dir.display()));
            }
            let t = PocketTts::load(&dir, self.precision, self.threads)
                .map_err(|e| format!("loading Pocket TTS from {}: {e}", dir.display()))?;
            self.session = Some(t);
            self.states.clear();
        }
        self.session
            .as_mut()
            .ok_or_else(|| "session missing".to_string())
    }

    /// The embedding for a clip: from the cache when its bytes were seen
    /// before, otherwise encoded and cached (a failed cache write is not an
    /// error; the next call just encodes again).
    fn embedding(
        &mut self,
        hash: &str,
        bytes: &[u8],
        what: &str,
    ) -> Result<VoiceEmbedding, String> {
        let cached = self.cache_dir.join(format!("{hash}.f32"));
        if let Ok(b) = std::fs::read(&cached) {
            if let Ok(e) = VoiceEmbedding::from_bytes(&b) {
                return Ok(e);
            }
        }
        let clip = decode_clip(bytes, what)?;
        let emb = self
            .ensure_session()?
            .encode_voice(&clip)
            .map_err(|e| format!("{what}: {e}"))?;
        if std::fs::create_dir_all(&self.cache_dir).is_ok() {
            let tmp = cached.with_extension("tmp");
            if std::fs::write(&tmp, emb.to_bytes()).is_ok() {
                let _ = std::fs::rename(&tmp, &cached);
            }
        }
        Ok(emb)
    }
}

impl Synthesizer for PocketSynthesizer {
    fn phonemize(&mut self, text: &str, _voice: &str) -> String {
        text.replace(['\u{2019}', '\u{2018}'], "'")
    }

    /// `PocketTts::generate` splits the text itself, so any length fits.
    fn fits(&mut self, _text: &str) -> bool {
        true
    }

    fn synth(&mut self, text: &str, voice: &str, speed: f32) -> Result<Vec<f32>, String> {
        let path = voices_dir(&self.models_dir).join(format!("{voice}.wav"));
        let bytes =
            std::fs::read(&path).map_err(|e| format!("voice clip {}: {e}", path.display()))?;
        self.ensure_session()?;
        let hash: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        if self.states.get(voice).is_none_or(|(h, _)| *h != hash) {
            let emb = self.embedding(&hash, &bytes, &path.display().to_string())?;
            let state = self
                .ensure_session()?
                .voice_state(&emb)
                .map_err(|e| format!("voice {voice}: {e}"))?;
            self.states.insert(voice.to_string(), (hash, state));
        }

        let seed = self.seed;
        self.seed = self.seed.wrapping_add(1);
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "session missing".to_string())?;
        let (_, state) = &self.states[voice];
        let audio = session
            .generate(state, text, self.temperature, seed)
            .map_err(|e| e.to_string())?;
        Ok(time_stretch(&audio, speed))
    }

    fn unload(&mut self) {
        self.session = None;
        self.states.clear();
    }

    fn is_loaded(&self) -> bool {
        self.session.is_some()
    }

    fn voice_exists(&self, voice: &str) -> bool {
        voices_dir(&self.models_dir)
            .join(format!("{voice}.wav"))
            .is_file()
    }

    fn reconfigure(&mut self, cfg: &Config) -> bool {
        let precision = Precision::from_config(&cfg.pocket.precision);
        let changed = precision != self.precision || cfg.threads != self.threads;
        self.precision = precision;
        self.threads = cfg.threads;
        self.temperature = cfg.pocket.temperature;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models_dir_of(s: &PocketSynthesizer) -> PathBuf {
        s.models_dir.clone()
    }

    use std::path::Path;

    fn bundle() -> Option<PathBuf> {
        std::env::var_os("SAYD_POCKET_BUNDLE").map(PathBuf::from)
    }

    /// FENRIR_WAV as samples at 24 kHz.
    fn fenrir() -> Vec<f32> {
        let mut r = hound::WavReader::new(std::io::Cursor::new(sayd_pocket::FENRIR_WAV)).unwrap();
        r.samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect()
    }

    fn write_wav(path: &Path, samples: &[f32], rate: u32, channels: u16) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            for _ in 0..channels {
                w.write_sample((s * 32767.0) as i16).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    /// A models dir with `pocket/voices` and, when a bundle is available,
    /// the bundle symlinked in.
    fn models_dir(tmp: &Path) -> PathBuf {
        let m = tmp.join("models");
        std::fs::create_dir_all(voices_dir(&m)).unwrap();
        if let Some(b) = bundle() {
            std::os::unix::fs::symlink(b, sayd_pocket::bundle_dir(&m)).unwrap();
        }
        m
    }

    fn synthesizer(tmp: &Path) -> PocketSynthesizer {
        let _ = sayd_pocket::init_environment();
        PocketSynthesizer::new(&models_dir(tmp), &tmp.join("cache"), &Config::default())
    }

    fn cache_files(tmp: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(tmp.join("cache"))
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn voice_exists_reflects_the_voices_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let m = models_dir(tmp.path());
        let s = PocketSynthesizer::new(&m, &tmp.path().join("cache"), &Config::default());
        assert!(!s.voice_exists("a"));
        write_wav(&voices_dir(&m).join("b.wav"), &[0.1; 2400], 24_000, 1);
        write_wav(&voices_dir(&m).join("a.wav"), &[0.1; 2400], 24_000, 1);
        std::fs::write(voices_dir(&m).join("notes.txt"), "x").unwrap();
        assert!(s.voice_exists("a") && s.voice_exists("b"));
        assert!(!s.voice_exists("notes"));
        assert_eq!(list_pocket_voices(&m), vec!["a", "b"]);
        assert!(!bundle_installed(&m) || bundle().is_some());
    }

    #[test]
    fn clip_at_44k1_stereo_is_resampled_and_end_on_pause_applied() {
        if bundle().is_none() {
            eprintln!("skipped: SAYD_POCKET_BUNDLE unset");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut s = synthesizer(tmp.path());
        // Cut mid-speech so the clip ends on a voiced sample.
        let clip = sayd_kokoro::audio::resample_linear(&fenrir()[..24_000 * 5], 24_000, 44_100);
        write_wav(
            &voices_dir(&models_dir_of(&s)).join("v.wav"),
            &clip,
            44_100,
            2,
        );
        let audio = s.synth("Hello there.", "v", 1.0).expect("synth");
        assert!(audio.len() > 24_000 / 2, "got {} samples", audio.len());
        let mut abs: Vec<f32> = audio.iter().map(|x| x.abs()).collect();
        let head = abs[..24_000 * 5 / 1000].iter().cloned().fold(0.0, f32::max);
        abs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p99 = abs[abs.len() * 99 / 100];
        assert!(head < p99, "burst in first 5 ms: {head} vs p99 {p99}");
    }

    #[test]
    fn replaced_clip_is_reencoded_and_deleted_clip_is_reported_missing() {
        if bundle().is_none() {
            eprintln!("skipped: SAYD_POCKET_BUNDLE unset");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut s = synthesizer(tmp.path());
        let path = voices_dir(&models_dir_of(&s)).join("a.wav");
        let f = fenrir();
        write_wav(&path, &f[..24_000 * 4], 24_000, 1);
        s.synth("Hi.", "a", 1.0).expect("first");
        let first = cache_files(tmp.path());
        assert_eq!(first.len(), 1);
        s.synth("Hi again.", "a", 1.0).expect("same clip, cached");
        assert_eq!(cache_files(tmp.path()), first);

        write_wav(&path, &f[24_000 * 2..24_000 * 6], 24_000, 1);
        s.synth("Hi.", "a", 1.0).expect("replaced");
        assert_eq!(cache_files(tmp.path()).len(), 2, "new clip, new cache file");

        std::fs::remove_file(&path).unwrap();
        assert!(!s.voice_exists("a"));
        let e = s.synth("Hi.", "a", 1.0).unwrap_err();
        assert!(e.contains("a.wav"), "error should name the file: {e}");
    }

    #[test]
    fn speed_other_than_one_time_stretches() {
        if bundle().is_none() {
            eprintln!("skipped: SAYD_POCKET_BUNDLE unset");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut s = synthesizer(tmp.path());
        write_wav(
            &voices_dir(&models_dir_of(&s)).join("v.wav"),
            &fenrir()[..24_000 * 5],
            24_000,
            1,
        );
        let text = "This is a sentence long enough to measure.";
        let base = s.synth(text, "v", 1.0).expect("1.0");
        // Same seed would be ideal; lengths vary little across seeds.
        let fast = s.synth(text, "v", 1.5).expect("1.5");
        let ratio = fast.len() as f32 / base.len() as f32;
        assert!(
            (ratio - 1.0 / 1.5).abs() < 0.1 * (1.0 / 1.5) + 0.05,
            "ratio {ratio}"
        );
    }

    #[test]
    fn missing_bundle_error_names_the_bundle_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let m = models_dir(tmp.path());
        write_wav(&voices_dir(&m).join("v.wav"), &[0.1; 24_000], 24_000, 1);
        let mut s = PocketSynthesizer::new(&m, &tmp.path().join("cache"), &Config::default());
        if bundle().is_some() {
            return;
        }
        let e = s.synth("Hi.", "v", 1.0).unwrap_err();
        assert!(e.contains("english_2026-04"), "{e}");
    }

    #[test]
    fn reconfigure_reloads_only_for_precision_or_threads() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = PocketSynthesizer::new(&tmp.path().join("m"), tmp.path(), &Config::default());
        let mut c = Config::default();
        c.pocket.temperature = 0.7;
        assert!(!s.reconfigure(&c));
        c.pocket.precision = "fp32".into();
        assert!(s.reconfigure(&c));
        c.threads += 1;
        assert!(s.reconfigure(&c));
        assert!(!s.reconfigure(&c));
    }

    #[test]
    fn bundle_installed_needs_every_file() {
        let tmp = tempfile::tempdir().unwrap();
        let m = tmp.path();
        let dir = sayd_pocket::bundle_dir(m);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!bundle_installed(m), "an empty directory");
        for f in &BUNDLE_FILES[..BUNDLE_FILES.len() - 1] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert!(!bundle_installed(m), "a cancelled download");
        std::fs::write(dir.join(BUNDLE_FILES[BUNDLE_FILES.len() - 1]), b"x").unwrap();
        assert!(bundle_installed(m));
    }
}
