//! The generation loop over the five graphs, ported from the export's
//! `pocket_tts_onnx.py`, with the PyTorch package's (4.0.1) behaviour where
//! the two differ: `end_on_pause` on the voice clip, no EOS in the first six
//! frames, `frames_after_eos`, and a 5 ms fade-in on every chunk.

use std::borrow::Cow;
use std::path::Path;

use ort::session::{Session, SessionInputValue};
use ort::value::{DynValue, Tensor};

use crate::rng::Gauss;
use crate::text::{prepare_text_prompt, split_into_best_sentences};
use crate::tokenizer::Tokenizer;
use crate::{npy, Bundle, Dtype, Error, Fill, Precision, Sessions, StateEntry};

/// `default_eos_threshold` of the package's model config.
const EOS_THRESHOLD: f32 = -4.0;
/// `TTSModel._MIN_FRAMES_BEFORE_EOS`: before speech starts the EOS logit of
/// some voices crosses the threshold, so the first frames ignore it.
const MIN_FRAMES_BEFORE_EOS: usize = 6;
/// Euler steps of the flow head (`lsd_decode_steps`, the package default).
const LSD_STEPS: usize = 1;
/// `TTSModel._estimate_max_gen_len`'s constants.
const TOKENS_PER_SECOND_ESTIMATE: f64 = 3.0;
const GEN_SECONDS_PADDING: f64 = 2.0;
/// Latent frames per `mimi_decoder` call, as the wrapper's `decode_latents`.
const DECODE_CHUNK_FRAMES: usize = 15;

/// `end_on_pause`'s parameters (`pocket_tts/data/audio_utils.py`).
const PAUSE_FRAME_SEC: f64 = 0.02;
const PAUSE_SEC: f64 = 0.08;
const PAUSE_FADE_SEC: f64 = 0.02;
const PAUSE_FLOOR_DB: f32 = 35.0;

/// A voice clip through `mimi_encoder`: `frames` rows of `conditioning_dim`.
pub struct VoiceEmbedding {
    pub frames: usize,
    pub data: Vec<f32>,
}

const EMBEDDING_MAGIC: &[u8; 4] = b"PVE1";

impl VoiceEmbedding {
    /// `PVE1`, then `frames` and the row width as little-endian `u32`, then
    /// the floats, little-endian.
    pub fn to_bytes(&self) -> Vec<u8> {
        let dim = self.data.len().checked_div(self.frames).unwrap_or(0);
        let mut out = Vec::with_capacity(12 + 4 * self.data.len());
        out.extend(EMBEDDING_MAGIC);
        out.extend((self.frames as u32).to_le_bytes());
        out.extend((dim as u32).to_le_bytes());
        for v in &self.data {
            out.extend(v.to_le_bytes());
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, Error> {
        let bad = |m: &str| Error::Voice(format!("cached embedding: {m}"));
        if b.len() < 12 || &b[..4] != EMBEDDING_MAGIC {
            return Err(bad("not a voice embedding"));
        }
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let (frames, dim) = (word(4), word(8));
        let body = &b[12..];
        if frames.checked_mul(dim).and_then(|n| n.checked_mul(4)) != Some(body.len()) {
            return Err(bad("length does not match its header"));
        }
        let data = body
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Ok(VoiceEmbedding { frames, data })
    }
}

/// `flow_lm_main`'s state after the voice prompt. ONNX Runtime never writes
/// to its inputs, so every chunk starts from this same state unchanged.
pub struct VoiceState {
    state: Vec<DynValue>,
    /// Positions of the attention cache the prompt (BOS + voice) fills.
    prompt_len: usize,
}

pub struct PocketTts {
    bundle: Bundle,
    sessions: Sessions,
    tokenizer: Tokenizer,
    /// One `conditioning_dim` row put before the voice, when the bundle asks.
    bos_before_voice: Option<Vec<f32>>,
    /// Length of `flow_lm_main`'s attention cache, if the manifest shows one.
    kv_capacity: Option<usize>,
}

// Task 5 moves these into the daemon's synth thread.
const _: () = {
    const fn send<T: Send>() {}
    const fn send_sync<T: Send + Sync>() {}
    send::<PocketTts>();
    send_sync::<VoiceState>();
};

/// `TTSModel._estimate_max_gen_len`: a frame budget from the token count.
fn max_gen_len(tokens: usize, frame_rate: f32) -> usize {
    let secs = tokens as f64 / TOKENS_PER_SECOND_ESTIMATE + GEN_SECONDS_PADDING;
    (secs * frame_rate as f64).ceil() as usize
}

/// Frames that speaking `tokens` takes at the package's estimated rate,
/// without its padding. Measured speech runs 2.2-3.3 frames per token; this
/// is 4.2, so a chunk granted it rarely stops before its EOS.
fn speech_frames(tokens: usize, frame_rate: f32) -> usize {
    (tokens as f64 / TOKENS_PER_SECOND_ESTIMATE * frame_rate as f64).ceil() as usize
}

/// How many frames a chunk of `tokens` may generate after a prompt of
/// `prompt_len` positions: the package's budget, clamped to what is left of
/// an attention cache of `cap` positions. `None` when what is left is less
/// than `speech_frames`, so the chunk has to be split.
fn frame_budget(
    prompt_len: usize,
    tokens: usize,
    frame_rate: f32,
    cap: Option<usize>,
) -> Option<usize> {
    let budget = max_gen_len(tokens, frame_rate);
    let Some(cap) = cap else {
        return Some(budget);
    };
    let room = cap.checked_sub(prompt_len + tokens)?;
    (room >= speech_frames(tokens, frame_rate)).then_some(budget.min(room))
}

/// Append `chunk` to `out` if `fits` it, or else its halves, split at the
/// middle word, each handled the same way. A single word that does not fit
/// is halved by characters.
fn split_to_fit(chunk: &str, fits: &mut dyn FnMut(&str) -> bool, out: &mut Vec<String>) {
    let chunk = chunk.trim();
    if chunk.is_empty() {
        return;
    }
    if fits(chunk) {
        out.push(chunk.to_owned());
        return;
    }
    let words: Vec<&str> = chunk.split_whitespace().collect();
    let (a, b) = if words.len() > 1 {
        let mid = words.len() / 2;
        (words[..mid].join(" "), words[mid..].join(" "))
    } else {
        let chars: Vec<char> = chunk.chars().collect();
        if chars.len() < 2 {
            // Nothing left to split; `latents` reports it.
            out.push(chunk.to_owned());
            return;
        }
        let mid = chars.len() / 2;
        (chars[..mid].iter().collect(), chars[mid..].iter().collect())
    };
    split_to_fit(&a, fits, out);
    split_to_fit(&b, fits, out);
}

fn init_state(manifest: &[StateEntry]) -> Result<Vec<DynValue>, Error> {
    manifest
        .iter()
        .map(|e| {
            let n = e.shape.iter().product::<usize>();
            let shape = e.shape.clone();
            Ok(match e.dtype {
                Dtype::F32 => {
                    let v = match e.fill {
                        Fill::Nan => f32::NAN,
                        Fill::Ones => 1.0,
                        Fill::Zeros | Fill::Empty => 0.0,
                    };
                    Tensor::from_array((shape, vec![v; n]))?.into_dyn()
                }
                Dtype::I64 => {
                    let v = i64::from(e.fill == Fill::Ones);
                    Tensor::from_array((shape, vec![v; n]))?.into_dyn()
                }
                Dtype::Bool => {
                    Tensor::from_array((shape, vec![e.fill == Fill::Ones; n]))?.into_dyn()
                }
            })
        })
        .collect()
}

fn missing(name: &str) -> Error {
    Error::Graph(format!("no output `{name}`"))
}

/// Run a streaming graph: `fixed` inputs plus the state tensors under their
/// manifest names. Returns the `wanted` outputs and the next state.
fn run_stateful(
    session: &mut Session,
    fixed: Vec<(&str, SessionInputValue<'_>)>,
    manifest: &[StateEntry],
    state: &[DynValue],
    wanted: &[&str],
) -> Result<(Vec<DynValue>, Vec<DynValue>), Error> {
    let mut inputs: Vec<(Cow<'_, str>, SessionInputValue<'_>)> =
        Vec::with_capacity(fixed.len() + manifest.len());
    inputs.extend(fixed.into_iter().map(|(k, v)| (Cow::Borrowed(k), v)));
    inputs.extend(
        manifest
            .iter()
            .zip(state)
            .map(|(e, v)| (Cow::Borrowed(e.input_name.as_str()), v.into())),
    );
    let mut out = session.run(inputs)?;
    let wanted = wanted
        .iter()
        .map(|n| out.remove(n).ok_or_else(|| missing(n)))
        .collect::<Result<_, _>>()?;
    let next = manifest
        .iter()
        .map(|e| {
            out.remove(&e.output_name)
                .ok_or_else(|| missing(&e.output_name))
        })
        .collect::<Result<_, _>>()?;
    Ok((wanted, next))
}

fn floats(v: &DynValue) -> Result<&[f32], Error> {
    Ok(v.try_extract_tensor::<f32>()?.1)
}

impl PocketTts {
    pub fn load(bundle_dir: &Path, p: Precision, threads: usize) -> Result<PocketTts, Error> {
        let bundle = Bundle::load(bundle_dir)?;
        let tokenizer = Tokenizer::load(&bundle_dir.join("tokenizer.model"))?;
        let bos_before_voice = if bundle.insert_bos_before_voice {
            let (_, bos) = npy::read_f32(&bundle_dir.join("bos_before_voice.npy"))?;
            if bos.len() != bundle.conditioning_dim {
                return Err(Error::Bundle(format!(
                    "bos_before_voice.npy holds {} floats, not one row of {}",
                    bos.len(),
                    bundle.conditioning_dim
                )));
            }
            Some(bos)
        } else {
            None
        };
        // The KV caches are [2, batch, positions, heads, head_dim].
        let kv_capacity = bundle
            .flow_lm_state
            .iter()
            .filter(|e| e.shape.len() == 5)
            .map(|e| e.shape[2])
            .min();
        let sessions = Sessions::load(bundle_dir, p, threads)?;
        Ok(PocketTts {
            bundle,
            sessions,
            tokenizer,
            bos_before_voice,
            kv_capacity,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.bundle.sample_rate
    }

    /// Run `mimi_encoder` on a 24 kHz mono clip, after the package's
    /// `end_on_pause`.
    pub fn encode_voice(&mut self, clip_24k_mono: &[f32]) -> Result<VoiceEmbedding, Error> {
        let clip = end_on_pause(clip_24k_mono, self.bundle.sample_rate);
        if clip.is_empty() {
            return Err(Error::Voice("the clip is empty".into()));
        }
        let audio = Tensor::from_array(([1usize, 1, clip.len()], clip))?;
        let out = self
            .sessions
            .mimi_encoder
            .run(ort::inputs!["audio" => audio])?;
        let latents = out.get("latents").ok_or_else(|| missing("latents"))?;
        let (shape, data) = latents.try_extract_tensor::<f32>()?;
        let cd = self.bundle.conditioning_dim;
        if shape.len() != 3 || shape[2] as usize != cd {
            return Err(Error::Graph(format!(
                "mimi_encoder returned shape {:?}, not [1, frames, {cd}]",
                &shape[..]
            )));
        }
        Ok(VoiceEmbedding {
            frames: shape[1] as usize,
            data: data.to_vec(),
        })
    }

    /// Prefill `flow_lm_main` with BOS and the voice, from the manifest's
    /// initial state.
    pub fn voice_state(&mut self, e: &VoiceEmbedding) -> Result<VoiceState, Error> {
        let cd = self.bundle.conditioning_dim;
        if e.frames == 0 || e.data.len() != e.frames * cd {
            return Err(Error::Voice(format!(
                "embedding holds {} floats for {} frames; this bundle wants rows of {cd}",
                e.data.len(),
                e.frames
            )));
        }
        let mut prompt = Vec::with_capacity((e.frames + 1) * cd);
        if let Some(bos) = &self.bos_before_voice {
            prompt.extend_from_slice(bos);
        }
        prompt.extend_from_slice(&e.data);
        let prompt_len = prompt.len() / cd;
        // Leave room for one full chunk, so `chunks` can always split text
        // down to pieces that fit.
        let full = self.bundle.max_token_per_chunk;
        if frame_budget(prompt_len, full, self.bundle.frame_rate, self.kv_capacity).is_none() {
            return Err(Error::TooLong(format!(
                "the voice clip fills {prompt_len} of {} attention positions, leaving no room \
                 for {full} tokens of text; use a shorter clip",
                self.kv_capacity.unwrap_or_default()
            )));
        }
        let manifest = &self.bundle.flow_lm_state;
        let empty_seq =
            Tensor::from_array(([1usize, 0, self.bundle.latent_dim], Vec::<f32>::new()))?;
        let prompt = Tensor::from_array(([1usize, prompt_len, cd], prompt))?;
        let (_, state) = run_stateful(
            &mut self.sessions.flow_lm_main,
            vec![
                ("sequence", empty_seq.into()),
                ("text_embeddings", prompt.into()),
            ],
            manifest,
            &init_state(manifest)?,
            &[],
        )?;
        Ok(VoiceState { state, prompt_len })
    }

    /// Tokens of `chunk` once prepared as the model is fed it.
    fn token_count(&self, chunk: &str) -> usize {
        self.tokenizer.encode(&prepare_text_prompt(chunk).0).len()
    }

    /// `text` in the chunks `generate` speaks: the package's
    /// `split_into_best_sentences`, then any chunk that would not fit in the
    /// attention cache after `v` split further at word boundaries.
    pub fn chunks(&self, v: &VoiceState, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut fits = |c: &str| {
            let tokens = self.token_count(c);
            frame_budget(
                v.prompt_len,
                tokens,
                self.bundle.frame_rate,
                self.kv_capacity,
            )
            .is_some()
        };
        for chunk in
            split_into_best_sentences(&self.tokenizer, text, self.bundle.max_token_per_chunk)
        {
            split_to_fit(&chunk, &mut fits, &mut out);
        }
        out
    }

    /// The latent frames for one chunk of text (`frames × latent_dim`),
    /// sampled with noise of variance `temperature` drawn from `rng`. The
    /// chunk is prepared as the package does but not split further; one
    /// from `chunks` always fits.
    pub fn latents(
        &mut self,
        v: &VoiceState,
        chunk: &str,
        temperature: f32,
        rng: &mut Gauss,
    ) -> Result<Vec<f32>, Error> {
        let (prepared, guess) = prepare_text_prompt(chunk);
        let ids = self.tokenizer.encode(&prepared);
        if prepared.is_empty() || ids.is_empty() {
            return Ok(Vec::new());
        }
        // The package adds 2 to its guess (`generate_audio_stream`).
        let frames_after_eos = self
            .bundle
            .model_recommended_frames_after_eos
            .unwrap_or(guess + 2);
        let fr = self.bundle.frame_rate;
        let Some(max_frames) = frame_budget(v.prompt_len, ids.len(), fr, self.kv_capacity) else {
            return Err(Error::TooLong(format!(
                "voice ({}) plus text ({} tokens) plus {} frames of speech overflow {} \
                 attention positions",
                v.prompt_len,
                ids.len(),
                speech_frames(ids.len(), fr),
                self.kv_capacity.unwrap_or_default()
            )));
        };
        let (ld, cd) = (self.bundle.latent_dim, self.bundle.conditioning_dim);
        let manifest = &self.bundle.flow_lm_state;
        let s = &mut self.sessions;

        let n_ids = ids.len();
        let ids = Tensor::from_array(([1usize, n_ids], ids))?;
        let out = s.text_conditioner.run(ort::inputs!["token_ids" => ids])?;
        let text = out.get("embeddings").ok_or_else(|| missing("embeddings"))?;
        let empty_seq = Tensor::from_array(([1usize, 0, ld], Vec::<f32>::new()))?;
        let (_, mut state) = run_stateful(
            &mut s.flow_lm_main,
            vec![
                ("sequence", empty_seq.into()),
                ("text_embeddings", text.into()),
            ],
            manifest,
            &v.state,
            &[],
        )?;
        drop(out);

        let empty_text = Tensor::from_array(([1usize, 0, cd], Vec::<f32>::new()))?;
        let std = temperature.max(0.0).sqrt();
        let dt = 1.0 / LSD_STEPS as f32;
        // NaN marks the BOS position; the graph swaps in its BOS latent.
        let mut curr = vec![f32::NAN; ld];
        let mut latents = Vec::new();
        let mut eos_step = None;
        for step in 0..max_frames {
            let seq = Tensor::from_array(([1usize, 1, ld], curr))?;
            let (out, next) = run_stateful(
                &mut s.flow_lm_main,
                vec![
                    ("sequence", seq.into()),
                    ("text_embeddings", (&empty_text).into()),
                ],
                manifest,
                &state,
                &["conditioning", "eos_logit"],
            )?;
            state = next;
            let eos = floats(&out[1])?
                .first()
                .copied()
                .ok_or_else(|| missing("eos_logit"))?;
            if eos > EOS_THRESHOLD && eos_step.is_none() && step >= MIN_FRAMES_BEFORE_EOS {
                eos_step = Some(step);
            }
            if eos_step.is_some_and(|e| step >= e + frames_after_eos) {
                break;
            }

            let mut x = vec![0.0f32; ld];
            rng.fill(&mut x, std);
            for j in 0..LSD_STEPS {
                let st = |v: f32| Tensor::from_array(([1usize, 1], vec![v]));
                let flow = s.flow_lm_flow.run(ort::inputs![
                    "c" => &out[0],
                    "s" => st(j as f32 * dt)?,
                    "t" => st((j + 1) as f32 * dt)?,
                    "x" => Tensor::from_array(([1usize, ld], x.clone()))?,
                ])?;
                let dir = floats(flow.get("flow_dir").ok_or_else(|| missing("flow_dir"))?)?;
                if dir.len() != ld {
                    return Err(Error::Graph(format!(
                        "flow_dir has {} values, not {ld}",
                        dir.len()
                    )));
                }
                for (x, d) in x.iter_mut().zip(dir) {
                    *x += d * dt;
                }
            }
            latents.extend_from_slice(&x);
            curr = x;
        }
        Ok(latents)
    }

    /// Decode one chunk's latents with a fresh `mimi_decoder` state, and fade
    /// the first 5 ms in.
    pub fn decode(&mut self, latents: &[f32]) -> Result<Vec<f32>, Error> {
        let ld = self.bundle.latent_dim;
        let manifest = &self.bundle.mimi_state;
        let mut state = init_state(manifest)?;
        let mut audio = Vec::with_capacity(latents.len() / ld * self.bundle.samples_per_frame);
        for chunk in latents.chunks(DECODE_CHUNK_FRAMES * ld) {
            let frames = Tensor::from_array(([1usize, chunk.len() / ld, ld], chunk.to_vec()))?;
            let (out, next) = run_stateful(
                &mut self.sessions.mimi_decoder,
                vec![("latent", frames.into())],
                manifest,
                &state,
                &["audio_frame"],
            )?;
            state = next;
            audio.extend_from_slice(floats(&out[0])?);
        }
        fade_in(&mut audio, self.bundle.sample_rate);
        Ok(audio)
    }

    /// Speak `text`: split by `chunks`, each chunk generated from `v` and
    /// decoded on its own, then concatenated.
    pub fn generate(
        &mut self,
        v: &VoiceState,
        text: &str,
        temperature: f32,
        seed: u64,
    ) -> Result<Vec<f32>, Error> {
        let mut rng = Gauss::new(seed);
        let mut audio = Vec::new();
        for chunk in self.chunks(v, text) {
            let latents = self.latents(v, &chunk, temperature, &mut rng)?;
            audio.extend(self.decode(&latents)?);
        }
        Ok(audio)
    }
}

/// The package's `end_on_pause`: cut the trailing silence (20 ms frames more
/// than 35 dB below the loudest), fade the last 20 ms out and append 80 ms of
/// zeros, so the prompt ends the way training prompts do.
pub fn end_on_pause(clip: &[f32], sample_rate: u32) -> Vec<f32> {
    let sr = sample_rate as f64;
    let frame = ((PAUSE_FRAME_SEC * sr) as usize).max(1);
    let n = clip.len() / frame;
    if n == 0 {
        return clip.to_vec();
    }
    let db: Vec<f32> = clip[..n * frame]
        .chunks_exact(frame)
        .map(|f| {
            let rms = (f.iter().map(|s| s * s).sum::<f32>() / frame as f32).sqrt();
            20.0 * (rms + 1e-12).log10()
        })
        .collect();
    let loudest = db.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let last = db
        .iter()
        .rposition(|&d| d > loudest - PAUSE_FLOOR_DB)
        .unwrap_or(n - 1);
    let end = (last + 1) * frame;
    let mut out = clip[..end].to_vec();
    let fade = ((PAUSE_FADE_SEC * sr) as usize).min(end);
    // torch.linspace(1, 0, fade)
    let span = fade.saturating_sub(1).max(1) as f32;
    for (i, s) in out[end - fade..].iter_mut().enumerate() {
        *s *= 1.0 - i as f32 / span;
    }
    out.resize(end + (PAUSE_SEC * sr) as usize, 0.0);
    out
}

/// Ramp the first 5 ms linearly from 0 to 1 (`torch.linspace(0, 1, sr // 200)`),
/// hiding the click of a fresh Mimi decoder state.
pub fn fade_in(audio: &mut [f32], sample_rate: u32) {
    let n = (sample_rate / 200) as usize;
    let span = n.saturating_sub(1).max(1) as f32;
    for (i, s) in audio.iter_mut().take(n).enumerate() {
        *s *= i as f32 / span;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_budget_clamps_to_the_cache_and_refuses_only_without_room_for_speech() {
        // english_2026-04: 12.5 frames/s, a 1000-position cache.
        let fr = 12.5;
        // 35 tokens: ceil((35/3 + 2) * 12.5) = 171, speech ceil(35/3 * 12.5) = 146.
        assert_eq!(max_gen_len(35, fr), 171);
        assert_eq!(speech_frames(35, fr), 146);
        assert_eq!(frame_budget(111, 35, fr, Some(1000)), Some(171));
        assert_eq!(frame_budget(111, 35, fr, None), Some(171));
        // After the 111-position fenrir prompt:
        // 160 tokens leave 729 positions, over the budget of 692: unclamped.
        assert_eq!(frame_budget(111, 160, fr, Some(1000)), Some(692));
        // 170 tokens leave 719, under the budget of 734 but over the 709
        // frames of speech: clamped to the room.
        assert_eq!(max_gen_len(170, fr), 734);
        assert_eq!(speech_frames(170, fr), 709);
        assert_eq!(frame_budget(111, 170, fr, Some(1000)), Some(719));
        // 200 tokens leave 689, under the 834 frames of speech: split it.
        assert_eq!(speech_frames(200, fr), 834);
        assert_eq!(frame_budget(111, 200, fr, Some(1000)), None);
        // Prompt and text alone overflow.
        assert_eq!(frame_budget(990, 20, fr, Some(1000)), None);
    }

    #[test]
    fn split_to_fit_halves_at_words_then_characters() {
        let mut out = Vec::new();
        let mut fits = |c: &str| c.len() <= 11;
        split_to_fit("one two three four five six", &mut fits, &mut out);
        assert_eq!(out, ["one", "two three", "four", "five six"]);
        // Nothing is lost or reordered.
        assert_eq!(out.join(" "), "one two three four five six");

        let mut out = Vec::new();
        split_to_fit(
            "abcdefghijklmnopqrstuvwxyz",
            &mut |c| c.len() <= 8,
            &mut out,
        );
        assert_eq!(out.concat(), "abcdefghijklmnopqrstuvwxyz");
        assert!(out.iter().all(|c| c.len() <= 8), "{out:?}");

        let mut out = Vec::new();
        split_to_fit("  fits  ", &mut |_| true, &mut out);
        assert_eq!(out, ["fits"]);
    }
}
