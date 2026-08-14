//! Kokoro-82M inference: phonemes -> ort session -> 24 kHz f32 samples.
//!
//! Lifted from kokoro-eval's `rust/src/kokoro.rs`. Two changes make it safe
//! for a long-running daemon that accepts arbitrary text: an unloaded voice
//! is an error rather than a panic, and voice packs are length-validated at
//! load time instead of being sliced blind at synthesis time.

pub mod audio;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use ort::session::Session;
use ort::value::Tensor;

/// Load the ONNX Runtime shared library and commit it as the process-wide
/// `ort` environment, once, up front.
///
/// Without this, the first call into `ort` (deep inside [`Kokoro::new`],
/// called from `sayd`'s `KokoroSynthesizer::ensure_session` on first
/// synthesis) discovers a missing or incompatible dylib *lazily*, inside
/// `ort`'s internal `setup_api`, which does `load_dynamic::init(&path)
/// .expect("Failed to load ONNX Runtime dylib")` -- a panic, not a
/// `Result`. That panic kills the engine thread; the daemon's main thread
/// then trips over the resulting poisoned state, and `ort`'s
/// `release_env_on_exit` panics again during process teardown in a context
/// that cannot unwind, producing `SIGABRT` instead of a clean, reportable
/// exit. Under a systemd unit with `Restart=on-failure`, that is a crash
/// loop.
///
/// Calling this explicitly, before anything else in the process touches
/// `ort`, performs the same dylib load `ort` would have attempted lazily,
/// but as an ordinary `Result` the caller can print and exit on cleanly --
/// the same way a missing audio device is already reported.
///
/// Respects `ORT_DYLIB_PATH` exactly as `ort` itself does; falls back to
/// the platform's default dynamic library name (searched via the normal
/// dynamic linker path, e.g. `LD_LIBRARY_PATH` on Linux) when unset.
pub fn init_environment() -> Result<(), String> {
    let path: PathBuf = match std::env::var_os("ORT_DYLIB_PATH") {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => default_dylib_name(),
    };
    ort::init_from(&path)
        .map_err(|e| {
            format!(
                "could not load ONNX Runtime: {e}; set ORT_DYLIB_PATH to the \
                 ONNX Runtime shared library (e.g. /usr/lib/libonnxruntime.so) \
                 if it is not on the dynamic linker's search path"
            )
        })?
        .commit();
    Ok(())
}

/// The dylib name `ort` itself falls back to when `ORT_DYLIB_PATH` is
/// unset, mirrored here so this explicit load looks in exactly the place
/// the lazy fallback inside `ort` would have.
fn default_dylib_name() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from("onnxruntime.dll")
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        PathBuf::from("libonnxruntime.so")
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        PathBuf::from("libonnxruntime.dylib")
    }
}

pub const SAMPLE_RATE: u32 = 24_000;
/// Style packs have exactly `STYLE_ROWS` rows and are indexed by token count,
/// so `STYLE_ROWS - 1` is the usable maximum.
pub const MAX_TOKENS: usize = 509;
pub const STYLE_DIM: usize = 256;
pub const STYLE_ROWS: usize = 510;

#[derive(Debug)]
pub enum Error {
    Ort(ort::Error),
    Io(std::io::Error),
    VoiceNotLoaded(String),
    BadVoicePack {
        name: String,
        floats: usize,
        expected: usize,
    },
    Vocab(String),
    NoOutput,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ort(e) => write!(f, "onnxruntime: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::VoiceNotLoaded(v) => write!(f, "voice {v} is not loaded"),
            Error::BadVoicePack {
                name,
                floats,
                expected,
            } => write!(
                f,
                "voice pack {name} has {floats} floats, expected {expected}"
            ),
            Error::Vocab(m) => write!(f, "tokenizer.json: {m}"),
            Error::NoOutput => write!(f, "model produced no output tensor"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ort::Error> for Error {
    fn from(e: ort::Error) -> Self {
        Error::Ort(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Borrow one 256-float style row, or `None` if the pack is too short.
fn style_row(pack: &[f32], row: usize) -> Option<&[f32]> {
    let off = row.checked_mul(STYLE_DIM)?;
    let end = off.checked_add(STYLE_DIM)?;
    pack.get(off..end)
}

/// Decode a flat little-endian f32 voice pack and validate its length.
///
/// Pulled out of `load_voice` so it can be unit-tested without an ONNX
/// session. `raw.len()` not being a multiple of 4 is itself an error rather
/// than silently truncated by `chunks_exact` -- otherwise a pack that is a
/// few stray bytes short of `expected * 4 + 4` could still decode to exactly
/// `expected` floats and slip past the length check below.
fn decode_pack(raw: &[u8], name: &str) -> Result<Vec<f32>, Error> {
    let expected = STYLE_ROWS * STYLE_DIM;
    if !raw.len().is_multiple_of(4) {
        return Err(Error::BadVoicePack {
            name: name.to_string(),
            floats: raw.len() / 4,
            expected,
        });
    }
    let v: Vec<f32> = raw
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if v.len() != expected {
        return Err(Error::BadVoicePack {
            name: name.to_string(),
            floats: v.len(),
            expected,
        });
    }
    Ok(v)
}

pub struct Kokoro {
    session: Session,
    input_name: String,
    vocab: HashMap<char, i64>,
    voices: HashMap<String, Vec<f32>>,
    models_dir: PathBuf,
}

impl Kokoro {
    pub fn new(models_dir: &Path, model_file: &str, threads: usize) -> Result<Self, Error> {
        let session = Session::builder()?
            .with_intra_threads(threads)
            .map_err(ort::Error::<()>::from)?
            .commit_from_file(models_dir.join(model_file))?;
        let input_name = session.inputs()[0].name().to_string();
        let vocab = load_vocab(&models_dir.join("tokenizer.json"))?;
        Ok(Self {
            session,
            input_name,
            vocab,
            voices: HashMap::new(),
            models_dir: models_dir.to_path_buf(),
        })
    }

    /// Style packs are `STYLE_ROWS * STYLE_DIM` f32, flat on disk. The length
    /// is validated here (see `decode_pack`) so that `synth` can never index
    /// out of bounds.
    pub fn load_voice(&mut self, name: &str) -> Result<(), Error> {
        if self.voices.contains_key(name) {
            return Ok(());
        }
        let raw = std::fs::read(self.models_dir.join("voices").join(format!("{name}.bin")))?;
        let v = decode_pack(&raw, name)?;
        self.voices.insert(name.to_string(), v);
        Ok(())
    }

    pub fn tokenize(&self, phonemes: &str) -> Vec<i64> {
        phonemes
            .chars()
            .filter_map(|c| self.vocab.get(&c).copied())
            .take(MAX_TOKENS)
            .collect()
    }

    pub fn synth(&mut self, phonemes: &str, voice: &str, speed: f32) -> Result<Vec<f32>, Error> {
        let ids = self.tokenize(phonemes);
        let pack = self
            .voices
            .get(voice)
            .ok_or_else(|| Error::VoiceNotLoaded(voice.to_string()))?;
        let style: Vec<f32> = style_row(pack, ids.len())
            .ok_or_else(|| Error::BadVoicePack {
                name: voice.to_string(),
                floats: pack.len(),
                expected: STYLE_ROWS * STYLE_DIM,
            })?
            .to_vec();

        let mut tokens = Vec::with_capacity(ids.len() + 2);
        tokens.push(0i64);
        tokens.extend_from_slice(&ids);
        tokens.push(0i64);

        let t_in = Tensor::from_array(([1usize, tokens.len()], tokens))?;
        let s_in = Tensor::from_array(([1usize, STYLE_DIM], style))?;
        let sp_in = Tensor::from_array(([1usize], vec![speed]))?;

        let outputs = self.session.run(ort::inputs![
            self.input_name.as_str() => t_in,
            "style" => s_in,
            "speed" => sp_in,
        ])?;

        let out = outputs.values().next().ok_or(Error::NoOutput)?;
        let (_shape, data) = out.try_extract_tensor::<f32>()?;
        Ok(data.to_vec())
    }
}

fn load_vocab(path: &Path) -> Result<HashMap<char, i64>, Error> {
    let txt = std::fs::read_to_string(path)?;
    let v: serde_json::Value =
        serde_json::from_str(&txt).map_err(|e| Error::Vocab(e.to_string()))?;
    let obj = v["model"]["vocab"]
        .as_object()
        .ok_or_else(|| Error::Vocab("no model.vocab object".into()))?;
    Ok(obj
        .iter()
        .filter_map(|(k, val)| {
            let mut it = k.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None; // vocab keys are single characters
            }
            Some((c, val.as_i64()?))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_not_loaded_error_formats_with_the_voice_name() {
        // Display-string pin, not a behaviour test: the real regression
        // coverage for "unloaded voice is an error, not a panic" lives in
        // the `models`-gated `synth_on_unloaded_voice_is_an_error` test
        // below, which actually calls `synth`.
        let e = Error::VoiceNotLoaded("am_fenrir".into());
        assert!(e.to_string().contains("am_fenrir"));
    }

    #[test]
    fn bad_voice_pack_error_formats_with_name_and_counts() {
        // Display-string pin only; behaviour coverage is in the
        // `decode_pack_rejects_*` tests below.
        let e = Error::BadVoicePack {
            name: "af_heart".into(),
            floats: 1024,
            expected: 130_560,
        };
        let msg = e.to_string();
        assert!(msg.contains("af_heart"));
        assert!(msg.contains("1024"));
        assert!(msg.contains("130560"));
    }

    #[test]
    fn style_row_bounds_are_checked() {
        // A pack one row short must not be indexable at the last row.
        let pack = vec![0.0f32; (STYLE_ROWS - 1) * STYLE_DIM];
        assert!(style_row(&pack, STYLE_ROWS - 1).is_none());
        assert!(style_row(&pack, 0).is_some());
    }

    #[test]
    fn style_row_returns_the_right_slice() {
        let mut pack = vec![0.0f32; STYLE_ROWS * STYLE_DIM];
        pack[3 * STYLE_DIM] = 42.0;
        let row = style_row(&pack, 3).expect("row 3 exists");
        assert_eq!(row.len(), STYLE_DIM);
        assert_eq!(row[0], 42.0);
    }

    #[test]
    fn decode_pack_of_correct_size_decodes_all_floats() {
        let raw = vec![0u8; STYLE_ROWS * STYLE_DIM * 4];
        let v = decode_pack(&raw, "af_heart").expect("correctly sized pack decodes");
        assert_eq!(v.len(), STYLE_ROWS * STYLE_DIM);
    }

    #[test]
    fn decode_pack_rejects_a_truncated_buffer() {
        let raw = vec![0u8; 1024 * 4]; // 1024 floats, far short of expected
        match decode_pack(&raw, "af_heart") {
            Err(Error::BadVoicePack {
                name,
                floats,
                expected,
            }) => {
                assert_eq!(name, "af_heart");
                assert_eq!(floats, 1024);
                assert_eq!(expected, STYLE_ROWS * STYLE_DIM);
            }
            other => panic!("expected BadVoicePack, got {other:?}"),
        }
    }

    #[test]
    fn decode_pack_rejects_a_length_not_a_multiple_of_four() {
        // Exactly `expected` floats' worth of bytes plus three stray bytes.
        // A naive `chunks_exact(4)` would silently drop the trailing three
        // bytes and this would decode to exactly `expected` floats, passing
        // a length check that only compares float counts. It must still be
        // rejected.
        let expected = STYLE_ROWS * STYLE_DIM;
        let raw = vec![0u8; expected * 4 + 3];
        match decode_pack(&raw, "af_heart") {
            Err(Error::BadVoicePack {
                name,
                floats,
                expected: exp,
            }) => {
                assert_eq!(name, "af_heart");
                assert_eq!(floats, expected);
                assert_eq!(exp, expected);
            }
            other => panic!("expected BadVoicePack, got {other:?}"),
        }
    }

    #[cfg(feature = "models")]
    mod models_tests {
        use super::*;
        use std::path::Path;

        fn models_dir() -> &'static Path {
            Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../models"))
        }

        #[test]
        fn synth_on_unloaded_voice_is_an_error_not_a_panic() {
            let mut k = Kokoro::new(models_dir(), "model.onnx", 1).expect("model loads");
            let err = k
                .synth("kˈOkəɹO", "am_fenrir", 1.0)
                .expect_err("unloaded voice must error");
            match err {
                Error::VoiceNotLoaded(v) => assert_eq!(v, "am_fenrir"),
                other => panic!("expected VoiceNotLoaded, got {other:?}"),
            }
        }

        #[test]
        fn synth_with_a_loaded_voice_produces_non_empty_audio() {
            let mut k = Kokoro::new(models_dir(), "model.onnx", 1).expect("model loads");
            k.load_voice("af_heart").expect("af_heart.bin loads");
            let audio = k
                .synth("kˈOkəɹO", "af_heart", 1.0)
                .expect("synth with a loaded voice succeeds");
            assert!(!audio.is_empty());
        }
    }
}
