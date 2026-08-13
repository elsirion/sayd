//! G2P by binding libespeak-ng directly.
//!
//! The `espeak-rs` crate compiles espeak-ng from source, which is a poor fit on
//! NixOS where the library already exists in the store. This talks to the same
//! `libespeak-ng.so` the Python side uses through phonemizer, so the phoneme
//! stream is identical, and then applies misaki's English remapping on top.
//!
//! Note this is the *espeak-only* G2P tier, used as misaki-en's fallback for
//! out-of-lexicon words and as the whole-text path for British voices.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::Mutex;

#[allow(non_camel_case_types)]
type espeak_AUDIO_OUTPUT = c_int;
const AUDIO_OUTPUT_SYNCHRONOUS: espeak_AUDIO_OUTPUT = 2;
const ESPEAK_INITIALIZE_PHONEME_IPA: c_int = 0x0002;
const ESPEAK_CHARS_UTF8: c_int = 1;
const ESPEAK_PHONEMES_IPA: c_int = 0x02;
const ESPEAK_PHONEMES_TIE: c_int = 0x80;

extern "C" {
    fn espeak_Initialize(
        output: espeak_AUDIO_OUTPUT,
        buflength: c_int,
        path: *const c_char,
        options: c_int,
    ) -> c_int;
    fn espeak_SetVoiceByName(name: *const c_char) -> c_int;
    fn espeak_TextToPhonemes(
        textptr: *mut *const c_void,
        textmode: c_int,
        phonememode: c_int,
    ) -> *const c_char;
}

// espeak-ng keeps global translator state; every call must be serialized.
static LOCK: Mutex<()> = Mutex::new(());
static INIT: std::sync::Once = std::sync::Once::new();

fn ensure_init() {
    INIT.call_once(|| {
        let path = std::env::var("ESPEAK_DATA_PATH").ok();
        let c = path.as_deref().map(|p| CString::new(p).unwrap());
        let ptr = c.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        let rate = unsafe {
            espeak_Initialize(
                AUDIO_OUTPUT_SYNCHRONOUS,
                0,
                ptr,
                ESPEAK_INITIALIZE_PHONEME_IPA,
            )
        };
        if rate < 0 {
            eprintln!("warning: espeak_Initialize failed ({rate})");
        }
        // If the very first `espeak_SetVoiceByName` call after `Initialize`
        // selects a non-default voice (e.g. "en-gb"), espeak-ng segfaults;
        // priming the default "en-us" voice here first works around it.
        // Reproduced upstream too: this is why the predecessor GTK app's
        // warm-up call (`phonemize_en("Warm up.", false)`, always American)
        // ran before any voice could be selected -- it never surfaced
        // because British voices there could never be the first call.
        let cvoice = CString::new("en-us").unwrap();
        unsafe {
            espeak_SetVoiceByName(cvoice.as_ptr());
        }
    });
}

/// Raw IPA for `text`, with '^' ties inside multi-letter phoneme names —
/// matching phonemizer's `tie='^'`, which misaki's mapping table expects.
fn phonemize_raw(text: &str, voice: &str) -> String {
    ensure_init();
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(cvoice) = CString::new(voice) else {
        return String::new();
    };
    unsafe {
        espeak_SetVoiceByName(cvoice.as_ptr());
    }

    // espeak takes a C string, so an interior NUL would truncate the text --
    // and `CString::new` panics on one. Text normally arrives via the
    // engine's cleanup stage, which strips control characters, but this is a
    // public entry point and must not depend on a caller's ordering.
    let sanitized: String = text.chars().filter(|c| *c != '\u{0}').collect();
    let Ok(ctext) = CString::new(sanitized) else {
        return String::new();
    };
    let mut cursor = ctext.as_ptr() as *const c_void;
    let bytes = ctext.as_bytes();
    let start = cursor as usize;
    let mode = ESPEAK_PHONEMES_IPA | ESPEAK_PHONEMES_TIE | ((b'^' as c_int) << 8);

    let mut out = String::new();
    let mut last_consumed = 0usize;
    while !cursor.is_null() {
        let p = unsafe { espeak_TextToPhonemes(&mut cursor, ESPEAK_CHARS_UTF8, mode) };
        if p.is_null() {
            break;
        }
        let seg = unsafe { CStr::from_ptr(p) }.to_string_lossy();
        out.push_str(seg.trim());

        // espeak stops at clause boundaries and swallows the punctuation. Recover
        // it from the span it just consumed so commas/periods still reach the
        // model -- they are real tokens in Kokoro's vocab and drive prosody.
        let consumed = if cursor.is_null() {
            bytes.len()
        } else {
            (cursor as usize).saturating_sub(start).min(bytes.len())
        };
        if let Some(punct) = bytes[last_consumed..consumed]
            .iter()
            .rev()
            .find(|c| matches!(c, b'.' | b',' | b';' | b':' | b'!' | b'?'))
        {
            out.push(*punct as char);
        }
        last_consumed = consumed;
        out.push(' ');
        if consumed >= bytes.len() {
            break;
        }
    }
    out.trim().to_string()
}

/// misaki's espeak->Kokoro remapping (misaki/espeak.py `EspeakFallback.E2M`),
/// longest key first so digraphs win over their prefixes.
const E2M: &[(&str, &str)] = &[
    ("ʔˌn\u{0329}", "ʔn"),
    ("ʔn\u{0329}", "ʔn"),
    ("a^ɪ", "I"),
    ("a^ʊ", "W"),
    ("d^ʒ", "ʤ"),
    ("e^ɪ", "A"),
    ("t^ʃ", "ʧ"),
    ("ɔ^ɪ", "Y"),
    ("ə^l", "ᵊl"),
    ("ʲo", "jo"),
    ("ʲə", "jə"),
    ("ɚ", "əɹ"),
    ("e", "A"),
    ("ʲ", ""),
    ("r", "ɹ"),
    ("x", "k"),
    ("ç", "k"),
    ("ɐ", "ə"),
    ("ɬ", "l"),
    ("\u{0303}", ""),
];

pub fn phonemize_en(text: &str, british: bool) -> String {
    let mut ps = phonemize_raw(text, if british { "en-gb" } else { "en-us" });
    for (from, to) in E2M {
        ps = ps.replace(from, to);
    }
    // syllabic consonant -> schwa + consonant
    ps = ps.replace('\u{0329}', "");
    if british {
        ps = ps.replace("e^ə", "ɛː").replace("iə", "ɪə").replace("ə^ʊ", "Q");
    } else {
        ps = ps
            .replace("o^ʊ", "O")
            .replace("ɜːɹ", "ɜɹ")
            .replace("ɜː", "ɜɹ")
            .replace("ɪə", "iə")
            .replace('ː', "");
    }
    ps = ps.replace('o', "ɔ");
    ps = ps.replace('ɾ', "T").replace('ʔ', "t");
    ps.replace('^', "")
}

/// Phonemize one word for use as a misaki-en fallback. Same mapping as
/// `phonemize_en`, but callers treat an empty result as "no pronunciation".
pub fn phonemize_word(word: &str, british: bool) -> Option<String> {
    let ps = phonemize_en(word, british);
    if ps.trim().is_empty() { None } else { Some(ps) }
}
