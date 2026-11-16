//! `bundle.json`: the model's constants and the initial-state manifests.

use std::path::Path;

use serde_json::Value;

use crate::Error;

pub struct Bundle {
    pub sample_rate: u32,
    pub samples_per_frame: usize,
    pub frame_rate: f32,
    pub latent_dim: usize,
    pub conditioning_dim: usize,
    pub max_token_per_chunk: usize,
    pub insert_bos_before_voice: bool,
    /// Frames to keep generating after the end-of-speech signal; `None`
    /// (as in english_2026-04) means the package's per-chunk guess.
    pub model_recommended_frames_after_eos: Option<usize>,
    pub flow_lm_state: Vec<StateEntry>,
    pub mimi_state: Vec<StateEntry>,
}

/// One streaming-state tensor: fed in as `input_name`, read back from
/// `output_name`, initialised from `dtype`, `shape` and `fill`.
pub struct StateEntry {
    pub input_name: String,
    pub output_name: String,
    pub dtype: Dtype,
    pub shape: Vec<usize>,
    pub fill: Fill,
}

/// How a state tensor starts. `Empty` is a zero-length tensor (its shape has a
/// 0 in it); the bundle's mimi manifest also asks for `Ones`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Nan,
    Zeros,
    Ones,
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dtype {
    F32,
    I64,
    Bool,
}

fn err(m: impl Into<String>) -> Error {
    Error::Bundle(m.into())
}

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value, Error> {
    v.get(key).ok_or_else(|| err(format!("missing `{key}`")))
}

fn uint(v: &Value, key: &str) -> Result<usize, Error> {
    field(v, key)?
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| err(format!("`{key}` is not an unsigned integer")))
}

fn string(v: &Value, key: &str) -> Result<String, Error> {
    field(v, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| err(format!("`{key}` is not a string")))
}

fn entry(v: &Value) -> Result<StateEntry, Error> {
    let dtype = match string(v, "dtype")?.as_str() {
        "float32" => Dtype::F32,
        "int64" => Dtype::I64,
        "bool" => Dtype::Bool,
        other => return Err(err(format!("unknown state dtype `{other}`"))),
    };
    let fill = match string(v, "fill")?.as_str() {
        "nan" => Fill::Nan,
        "zeros" => Fill::Zeros,
        "ones" => Fill::Ones,
        "empty" => Fill::Empty,
        other => return Err(err(format!("unknown state fill `{other}`"))),
    };
    let shape = field(v, "shape")?
        .as_array()
        .ok_or_else(|| err("`shape` is not an array"))?
        .iter()
        .map(|d| {
            d.as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| err("`shape` has a non-integer dimension"))
        })
        .collect::<Result<_, _>>()?;
    Ok(StateEntry {
        input_name: string(v, "input_name")?,
        output_name: string(v, "output_name")?,
        dtype,
        shape,
        fill,
    })
}

fn manifest(v: &Value, key: &str) -> Result<Vec<StateEntry>, Error> {
    field(v, key)?
        .as_array()
        .ok_or_else(|| err(format!("`{key}` is not an array")))?
        .iter()
        .map(entry)
        .collect()
}

impl Bundle {
    /// Read `<dir>/bundle.json`.
    pub fn load(dir: &Path) -> Result<Bundle, Error> {
        let raw = std::fs::read(dir.join("bundle.json"))?;
        let v: Value = serde_json::from_slice(&raw).map_err(|e| err(e.to_string()))?;
        Ok(Bundle {
            sample_rate: u32::try_from(uint(&v, "sample_rate")?)
                .map_err(|_| err("`sample_rate` is out of range"))?,
            samples_per_frame: uint(&v, "samples_per_frame")?,
            frame_rate: field(&v, "frame_rate")?
                .as_f64()
                .ok_or_else(|| err("`frame_rate` is not a number"))? as f32,
            latent_dim: uint(&v, "latent_dim")?,
            conditioning_dim: uint(&v, "conditioning_dim")?,
            max_token_per_chunk: uint(&v, "max_token_per_chunk")?,
            insert_bos_before_voice: field(&v, "insert_bos_before_voice")?
                .as_bool()
                .ok_or_else(|| err("`insert_bos_before_voice` is not a bool"))?,
            model_recommended_frames_after_eos: match v.get("model_recommended_frames_after_eos") {
                None | Some(Value::Null) => None,
                Some(_) => Some(uint(&v, "model_recommended_frames_after_eos")?),
            },
            flow_lm_state: manifest(&v, "flow_lm_state_manifest")?,
            mimi_state: manifest(&v, "mimi_state_manifest")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_fixture_parses_spec_values() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let b = Bundle::load(&dir).unwrap();
        assert_eq!(b.sample_rate, 24000);
        assert_eq!(b.samples_per_frame, 1920);
        assert_eq!(b.latent_dim, 32);
        assert_eq!(b.max_token_per_chunk, 50);
        assert!(b.insert_bos_before_voice);
        assert_eq!(b.model_recommended_frames_after_eos, None);
        let e = &b.flow_lm_state[0];
        assert_eq!(e.input_name, "state_0");
        assert_eq!(e.shape, [2, 1, 1000, 16, 64]);
        assert_eq!(e.fill, Fill::Nan);
    }
}
