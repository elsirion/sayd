//! The SentencePiece unigram model in the bundle's `tokenizer.model`, read
//! straight from its protobuf, so no sentencepiece or protobuf crate is needed.
//!
//! The package's released configs tokenize with a HuggingFace `tokenizer.json`
//! converted from this model: a `Prepend("▁")` normalizer, a `Metaspace`
//! pre-tokenizer that splits before every `▁`, Viterbi over the unigram
//! scores, byte fallback for characters with no piece. This follows that
//! pipeline rather than the sentencepiece library, which also normalizes with
//! NFKC and collapses whitespace; the package's ids are the reference.

use std::collections::HashMap;
use std::path::Path;

use crate::Error;

const SPACE: char = '\u{2581}';
/// HuggingFace scores an unknown character `min_score - 10`.
const UNK_PENALTY: f64 = 10.0;

const TYPE_UNKNOWN: u64 = 2;
const TYPE_CONTROL: u64 = 3;
const TYPE_BYTE: u64 = 6;

struct Piece {
    text: String,
    score: f64,
    kind: u64,
}

pub struct Tokenizer {
    pieces: Vec<Piece>,
    ids: HashMap<String, usize>,
    max_piece_chars: usize,
    unk_id: usize,
    unk_score: f64,
    /// Id of `<0xNN>` for each byte, when the model has all 256.
    byte_ids: Option<Vec<usize>>,
}

fn bad(m: &str) -> Error {
    Error::Tokenizer(m.to_string())
}

fn varint(b: &[u8], pos: &mut usize) -> Result<u64, Error> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*pos).ok_or_else(|| bad("truncated varint"))?;
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(bad("varint too long"))
}

/// Walk one protobuf message, handing each field to `f` as
/// `(number, varint value, length-delimited or fixed32 bytes)`.
fn fields(b: &[u8], mut f: impl FnMut(u64, u64, &[u8]) -> Result<(), Error>) -> Result<(), Error> {
    let mut pos = 0;
    while pos < b.len() {
        let key = varint(b, &mut pos)?;
        let (num, wire) = (key >> 3, key & 7);
        match wire {
            0 => {
                let v = varint(b, &mut pos)?;
                f(num, v, &[])?;
            }
            1 | 5 => {
                let n = if wire == 1 { 8 } else { 4 };
                let s = b.get(pos..pos + n).ok_or_else(|| bad("truncated field"))?;
                pos += n;
                f(num, 0, s)?;
            }
            2 => {
                let n = varint(b, &mut pos)? as usize;
                let s = pos
                    .checked_add(n)
                    .and_then(|end| b.get(pos..end))
                    .ok_or_else(|| bad("truncated field"))?;
                pos += n;
                f(num, 0, s)?;
            }
            _ => return Err(bad("unsupported protobuf wire type")),
        }
    }
    Ok(())
}

impl Tokenizer {
    pub fn load(path: &Path) -> Result<Tokenizer, Error> {
        let bytes = std::fs::read(path)?;
        let mut pieces = Vec::new();
        // ModelProto.pieces (1) is a repeated SentencePiece message.
        fields(&bytes, |num, _, body| {
            if num != 1 {
                return Ok(());
            }
            let (mut text, mut score, mut kind) = (None, 0.0f32, 1u64);
            fields(body, |num, v, s| {
                match num {
                    1 => {
                        text = Some(
                            String::from_utf8(s.to_vec()).map_err(|_| bad("piece is not utf-8"))?,
                        )
                    }
                    2 => score = f32::from_le_bytes(s.try_into().map_err(|_| bad("bad score"))?),
                    3 => kind = v,
                    _ => {}
                }
                Ok(())
            })?;
            pieces.push(Piece {
                text: text.ok_or_else(|| bad("piece without text"))?,
                score: f64::from(score),
                kind,
            });
            Ok(())
        })?;
        if pieces.is_empty() {
            return Err(bad("no pieces"));
        }
        let unk_id = pieces
            .iter()
            .position(|p| p.kind == TYPE_UNKNOWN)
            .ok_or_else(|| bad("no <unk> piece"))?;
        let min_score = pieces.iter().map(|p| p.score).fold(f64::INFINITY, f64::min);
        let mut ids = HashMap::new();
        for (i, p) in pieces.iter().enumerate() {
            ids.entry(p.text.clone()).or_insert(i);
        }
        let byte_ids: Option<Vec<usize>> = (0..=255u8)
            .map(|b| ids.get(&format!("<0x{b:02X}>")).copied())
            .collect();
        Ok(Tokenizer {
            max_piece_chars: pieces
                .iter()
                .map(|p| p.text.chars().count())
                .max()
                .unwrap_or(1),
            unk_score: min_score - UNK_PENALTY,
            pieces,
            ids,
            unk_id,
            byte_ids,
        })
    }

    pub fn vocab_size(&self) -> usize {
        self.pieces.len()
    }

    pub fn encode(&self, text: &str) -> Vec<i64> {
        if text.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut segment = String::new();
        for c in
            std::iter::once(SPACE).chain(text.chars().map(|c| if c == ' ' { SPACE } else { c }))
        {
            // Split before every `▁`, which starts the next segment.
            if c == SPACE && !segment.is_empty() {
                self.encode_segment(&segment, &mut out);
                segment.clear();
            }
            segment.push(c);
        }
        self.encode_segment(&segment, &mut out);
        out
    }

    /// Viterbi over one pre-token. Ties keep the earlier candidate, as in
    /// HuggingFace's lattice.
    fn encode_segment(&self, segment: &str, out: &mut Vec<i64>) {
        let chars: Vec<char> = segment.chars().collect();
        let n = chars.len();
        let mut best = vec![f64::NEG_INFINITY; n + 1];
        // (start, id, is_unk) of the best piece ending at each position.
        let mut back: Vec<(usize, usize, bool)> = vec![(0, 0, false); n + 1];
        best[0] = 0.0;
        let mut buf = String::new();
        for start in 0..n {
            let mut single = false;
            buf.clear();
            for len in 1..=self.max_piece_chars.min(n - start) {
                buf.push(chars[start + len - 1]);
                if let Some(&id) = self.ids.get(buf.as_str()) {
                    single |= len == 1;
                    let score = best[start] + self.pieces[id].score;
                    if score > best[start + len] {
                        best[start + len] = score;
                        back[start + len] = (start, id, false);
                    }
                }
            }
            if !single {
                let score = best[start] + self.unk_score;
                if score > best[start + 1] {
                    best[start + 1] = score;
                    back[start + 1] = (start, self.unk_id, true);
                }
            }
        }
        let mut tail = Vec::new();
        let mut end = n;
        while end > 0 {
            let (start, id, unk) = back[end];
            tail.push((start, end, id, unk));
            end = start;
        }
        for &(start, end, id, unk) in tail.iter().rev() {
            match (&self.byte_ids, unk) {
                (Some(bytes), true) => {
                    let s: String = chars[start..end].iter().collect();
                    out.extend(s.bytes().map(|b| bytes[usize::from(b)] as i64));
                }
                _ => out.push(id as i64),
            }
        }
    }

    /// Text for `ids`: special tokens dropped, `▁` read as a space (the
    /// leading one removed), byte tokens joined back into UTF-8.
    pub fn decode(&self, ids: &[i64]) -> String {
        let mut out = String::new();
        let mut bytes: Vec<u8> = Vec::new();
        let mut first = true;
        let flush = |bytes: &mut Vec<u8>, out: &mut String| {
            if bytes.is_empty() {
                return;
            }
            match std::str::from_utf8(bytes) {
                Ok(s) => out.push_str(s),
                Err(_) => out.extend(bytes.iter().map(|_| '\u{fffd}')),
            }
            bytes.clear();
        };
        for &id in ids {
            let Some(p) = usize::try_from(id).ok().and_then(|i| self.pieces.get(i)) else {
                continue;
            };
            if p.kind == TYPE_UNKNOWN || p.kind == TYPE_CONTROL {
                continue;
            }
            if p.kind == TYPE_BYTE {
                if let Some(b) = p
                    .text
                    .strip_prefix("<0x")
                    .and_then(|h| h.strip_suffix('>'))
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    bytes.push(b);
                    first = false;
                    continue;
                }
            }
            flush(&mut bytes, &mut out);
            let text = p.text.replace(SPACE, " ");
            out.push_str(if first {
                text.strip_prefix(' ').unwrap_or(&text)
            } else {
                &text
            });
            first = false;
        }
        flush(&mut bytes, &mut out);
        out
    }
}
