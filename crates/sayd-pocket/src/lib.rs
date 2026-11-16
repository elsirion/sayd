//! Pocket TTS (Kyutai, 100M) inference: bundle metadata, `.npy` tensors and
//! the five ONNX sessions. The tokenizer and generation loop build on these.

pub mod bundle;
pub mod npy;

use std::fmt;
use std::path::{Path, PathBuf};

use ort::session::Session;

pub use bundle::{Bundle, Dtype, Fill, StateEntry};

/// Directory name of the one bundle this crate understands, below
/// `<models>/pocket`.
const BUNDLE_NAME: &str = "english_2026-04";

/// ONNX Runtime stops scaling past this many intra-op threads for these
/// graphs; the reference wrapper caps it the same way.
const MAX_INTRA_THREADS: usize = 4;

/// Load the ONNX Runtime shared library and commit it as the process-wide
/// `ort` environment, so a missing dylib is a `Result` rather than a panic
/// inside `ort`'s lazy loader. See `sayd_kokoro::init_environment`.
pub fn init_environment() -> Result<(), String> {
    let path = match std::env::var_os("ORT_DYLIB_PATH") {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => PathBuf::from(if cfg!(windows) {
            "onnxruntime.dll"
        } else if cfg!(target_os = "macos") {
            "libonnxruntime.dylib"
        } else {
            "libonnxruntime.so"
        }),
    };
    ort::init_from(&path)
        .map_err(|e| {
            format!(
                "could not load ONNX Runtime: {e}; set ORT_DYLIB_PATH to the \
                 ONNX Runtime shared library"
            )
        })?
        .commit();
    Ok(())
}

#[derive(Debug)]
pub enum Error {
    Ort(ort::Error),
    Io(std::io::Error),
    /// `bundle.json` is unreadable, malformed or has an unknown value.
    Bundle(String),
    /// A `.npy` file this crate cannot read.
    Npy(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ort(e) => write!(f, "onnxruntime: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Bundle(m) => write!(f, "bundle.json: {m}"),
            Error::Npy(m) => write!(f, "npy: {m}"),
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

/// Which weights to run. The bundle ships int8 files for three of the five
/// models; the other two are fp32 either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precision {
    Int8,
    Fp32,
}

impl Precision {
    /// `"fp32"` selects fp32; anything else is int8, the default.
    pub fn from_config(s: &str) -> Precision {
        if s == "fp32" {
            Precision::Fp32
        } else {
            Precision::Int8
        }
    }
}

/// The five models of one Pocket TTS bundle.
pub struct Sessions {
    pub text_conditioner: Session,
    pub mimi_encoder: Session,
    pub flow_lm_main: Session,
    pub flow_lm_flow: Session,
    pub mimi_decoder: Session,
}

impl Sessions {
    pub fn load(dir: &Path, p: Precision, threads: usize) -> Result<Sessions, Error> {
        let open = |file: String| -> Result<Session, Error> {
            Ok(Session::builder()?
                .with_intra_threads(threads.clamp(1, MAX_INTRA_THREADS))
                .map_err(ort::Error::<()>::from)?
                .with_inter_threads(1)
                .map_err(ort::Error::<()>::from)?
                .commit_from_file(dir.join(file))?)
        };
        // int8 only where the bundle has a quantized file of that name.
        let pick = |stem: &str| -> String {
            let int8 = format!("{stem}_int8.onnx");
            if p == Precision::Int8 && dir.join(&int8).is_file() {
                int8
            } else {
                format!("{stem}.onnx")
            }
        };
        Ok(Sessions {
            text_conditioner: open("text_conditioner.onnx".into())?,
            mimi_encoder: open("mimi_encoder.onnx".into())?,
            flow_lm_main: open(pick("flow_lm_main"))?,
            flow_lm_flow: open(pick("flow_lm_flow"))?,
            mimi_decoder: open(pick("mimi_decoder"))?,
        })
    }
}

/// Where the bundle lives below the models directory.
pub fn bundle_dir(models_dir: &Path) -> PathBuf {
    models_dir.join("pocket").join(BUNDLE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs SAYD_POCKET_BUNDLE pointing at a bundle dir"]
    fn sessions_load_when_bundle_present() {
        let Some(dir) = std::env::var_os("SAYD_POCKET_BUNDLE") else {
            return;
        };
        let dir = Path::new(&dir);
        let bundle = Bundle::load(dir).unwrap();
        let (shape, _) = npy::read_f32(&dir.join("bos_before_voice.npy")).unwrap();
        assert_eq!(*shape.last().unwrap(), bundle.conditioning_dim);
        init_environment().unwrap();
        for p in [Precision::Int8, Precision::Fp32] {
            Sessions::load(dir, p, 4).unwrap();
        }
    }

    #[test]
    fn precision_defaults_to_int8() {
        assert!(matches!(Precision::from_config("fp32"), Precision::Fp32));
        assert!(matches!(Precision::from_config("int8"), Precision::Int8));
        assert!(matches!(Precision::from_config("bogus"), Precision::Int8));
    }

    #[test]
    fn bundle_dir_is_under_pocket() {
        assert_eq!(
            bundle_dir(Path::new("/m")),
            Path::new("/m/pocket/english_2026-04")
        );
    }
}
