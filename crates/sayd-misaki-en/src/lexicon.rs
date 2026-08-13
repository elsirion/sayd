//! FST-backed lexicon. Data is embedded at compile time, so construction is
//! just building two FST views -- no parsing, no allocation per entry.

use fst::Map as FstMap;

const TAGGED: u8 = 0x01;
const SEP: u8 = 0x02;

static GOLD_FST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/us_gold.fst"));
static GOLD_BLOB: &str = include_str!(concat!(env!("OUT_DIR"), "/us_gold.blob"));
static GOLD_IDX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/us_gold.idx"));
static SILVER_FST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/us_silver.fst"));
static SILVER_BLOB: &str = include_str!(concat!(env!("OUT_DIR"), "/us_silver.blob"));
static SILVER_IDX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/us_silver.idx"));

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Simple(&'static str),
    ByTag(Vec<(&'static str, Option<&'static str>)>),
}

impl Entry {
    /// Resolve to a phoneme string. With no POS tagger we can never match a
    /// specific tag, so we always take DEFAULT -- except for `en.py`'s
    /// `lookup` `'None'`-tag branch (en.py:239-240): when `future_vowel` is
    /// `None` (no next token established a value -- see
    /// `special_case::next_future_vowel`) and this entry has a `'None'` tag
    /// with a value, that reading wins over DEFAULT. This is what makes
    /// isolated/end-of-utterance readings of words like "this" ("ðˈɪs") and
    /// "by" ("bˈI") come out stressed instead of their mid-sentence DEFAULT
    /// form -- unlike the other special cases in `special_case.rs`, this
    /// mechanism is generic over every gold entry that happens to carry a
    /// `'None'` tag (32 of them, mostly short/irregular function words), not
    /// hand-listed per word. If DEFAULT itself has no value, the first tag
    /// with a value wins (unchanged from before).
    ///
    /// Divergence from en.py, deliberately not fixed: en.py:239-240 tests
    /// the *key* (`'None' in ps`), so a `'None'`-tagged entry whose value is
    /// null still wins this branch -- `ps.get('None', ps['DEFAULT'])` then
    /// returns that null, which the caller (en.py:244) treats as a miss and
    /// falls through to `get_NNP(word)`, NOT to DEFAULT. This function
    /// instead requires the `'None'` entry to have a value (`p.is_some()`)
    /// to take this branch at all, so on a null value it falls through to
    /// the DEFAULT search below instead of `get_NNP`. This is unreachable
    /// with the vendored data -- every gold entry with a `'None'` tag (32 of
    /// them) has a non-null value, and no silver entry has a `'None'` tag at
    /// all -- so it changes no observable output today. Recorded here so it
    /// isn't a surprise if `data/` is ever regenerated from a newer misaki.
    pub fn resolve(&self, future_vowel: Option<bool>) -> Option<&'static str> {
        match self {
            Entry::Simple(s) => Some(s),
            Entry::ByTag(v) => {
                if future_vowel.is_none() {
                    if let Some((_, Some(p))) = v.iter().find(|(t, p)| *t == "None" && p.is_some()) {
                        return Some(p);
                    }
                }
                v.iter()
                    .find(|(t, p)| *t == "DEFAULT" && p.is_some())
                    .and_then(|(_, p)| *p)
                    .or_else(|| v.iter().find_map(|(_, p)| *p))
            }
        }
    }
}

struct Tier {
    fst: FstMap<&'static [u8]>,
    blob: &'static str,
    idx: &'static [u8],
}

impl Tier {
    fn new(fst: &'static [u8], blob: &'static str, idx: &'static [u8]) -> Self {
        Tier { fst: FstMap::new(fst).expect("corrupt fst"), blob, idx }
    }

    fn offset(&self, i: usize) -> usize {
        let b = &self.idx[i * 4..i * 4 + 4];
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize
    }

    fn get(&self, word: &str) -> Option<Entry> {
        let i = self.fst.get(word.as_bytes())? as usize;
        let payload = &self.blob[self.offset(i)..self.offset(i + 1)];
        Some(decode(payload))
    }

    /// Membership only, no blob decode -- just the FST lookup `get` also
    /// does, without paying for the payload slice or `decode`'s allocation
    /// for tagged entries.
    fn contains(&self, word: &str) -> bool {
        self.fst.get(word.as_bytes()).is_some()
    }
}

fn decode(payload: &'static str) -> Entry {
    if !payload.starts_with(TAGGED as char) {
        return Entry::Simple(payload);
    }
    let mut out = Vec::new();
    for seg in payload.split(TAGGED as char).skip(1) {
        let (tag, phon) = seg.split_once(SEP as char).unwrap_or((seg, ""));
        out.push((tag, if phon.is_empty() { None } else { Some(phon) }));
    }
    Entry::ByTag(out)
}

pub struct Lexicon {
    gold: Tier,
    silver: Tier,
    pub british: bool,
}

impl Lexicon {
    /// Only the US (`us_gold`/`us_silver`) lexicon tiers are vendored (see
    /// `build.rs` and `data/PROVENANCE.md`); no `gb_gold`/`gb_silver` data
    /// exists in this crate. `raw` therefore always resolves against American
    /// pronunciations regardless of `british`. The flag is stored and still
    /// read by `stem.rs`, where it selects a handful of British-vs-American
    /// suffix phonemes (e.g. `_s`/`_ed`'s epenthetic vowel) for words reached
    /// through the stemmer -- but since the lexicon those suffixes attach to
    /// is itself always American, `british: true` does NOT give British
    /// pronunciations. Callers who need actual British output must route
    /// around this crate (e.g. a whole-text espeak `en-gb` fallback).
    pub fn new(british: bool) -> Self {
        Lexicon {
            gold: Tier::new(GOLD_FST, GOLD_BLOB, GOLD_IDX),
            silver: Tier::new(SILVER_FST, SILVER_BLOB, SILVER_IDX),
            british,
        }
    }

    /// Exact lookup, gold first then silver.
    pub fn raw(&self, word: &str) -> Option<Entry> {
        self.gold.get(word).or_else(|| self.silver.get(word))
    }

    /// 4 = gold, 3 = silver, None = absent. Mirrors misaki's rating scale.
    /// Presence-only: unlike `raw`, this never decodes the blob payload, so
    /// it does no allocation even for tagged (POS-dependent) entries.
    pub fn rating(&self, word: &str) -> Option<u8> {
        if self.gold.contains(word) {
            Some(4)
        } else if self.silver.contains(word) {
            Some(3)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_entries_round_trip() {
        let lex = Lexicon::new(false);
        assert_eq!(lex.raw("hello").unwrap().resolve(None), Some("həlˈO"));
        assert_eq!(lex.raw("the").unwrap().resolve(None), Some("ði"));
        assert_eq!(lex.rating("hello"), Some(4));
    }

    #[test]
    fn tagged_entries_resolve_to_default() {
        let lex = Lexicon::new(false);
        // "read" is DEFAULT=ɹˈid with VBD/VBN/VBP=ɹˈɛd. No tagger -> DEFAULT.
        assert_eq!(lex.raw("read").unwrap().resolve(Some(true)), Some("ɹˈid"));
        assert_eq!(lex.raw("record").unwrap().resolve(Some(true)), Some("ɹˈɛkəɹd"));
        match lex.raw("wind").unwrap() {
            Entry::ByTag(v) => assert!(v.iter().any(|(t, _)| *t == "VERB")),
            other => panic!("expected ByTag, got {other:?}"),
        }
    }

    #[test]
    fn none_tag_wins_only_when_future_vowel_is_unknown() {
        // en.py:239-240: the 'None'-tag branch only fires when
        // ctx.future_vowel is None (this crate's "no next token established
        // a value"). Otherwise DEFAULT wins, same as any other tagged entry.
        let lex = Lexicon::new(false);
        let this = lex.raw("this").unwrap();
        assert_eq!(this.resolve(None), Some("ðˈɪs"));
        assert_eq!(this.resolve(Some(true)), Some("ðɪs"));
        assert_eq!(this.resolve(Some(false)), Some("ðɪs"));
        let by = lex.raw("by").unwrap();
        assert_eq!(by.resolve(None), Some("bˈI"));
        assert_eq!(by.resolve(Some(false)), Some("bI"));
    }

    #[test]
    fn missing_words_are_none() {
        let lex = Lexicon::new(false);
        assert!(lex.raw("zzzzqqqnotaword").is_none());
    }
}
