# misaki-en

English G2P for Kokoro TTS, ported from [misaki](https://github.com/hexgrad/misaki)
(Apache-2.0). No spaCy, no torch, no POS tagger.

```rust
let g2p = misaki_en::G2p::new(false);
assert_eq!(g2p.phonemize("hello world"), "həlˈO wˈɜɹld");
```

The `british` flag (`G2p::new`'s / `G2p::with_fallback`'s first argument) does
**not** select US vs UK pronunciations: only the US lexicons are vendored (see
"Known limitation" below), so `british: true` currently affects only a
handful of suffix phonemes the stemmer chooses (`stem.rs`) -- every
lexicon/stemmer hit is American either way. `rust/src/main.rs` routes British
voices around this crate entirely (a whole-text espeak `en-gb` call) rather
than relying on this flag.

Pass an espeak closure to handle out-of-lexicon words:

```rust
let g2p = misaki_en::G2p::with_fallback(false, Box::new(|w| Some(espeak::phonemize_en(w, false))));
```

## Known limitation: no POS tagging

790 of 90,201 gold entries vary by part of speech. This crate always takes the
`DEFAULT` variant, so past-tense *read* comes out as "reed", and *record* is
always the noun. Measured on a 500-word article, ~1% of tokens are affected.
Fixing it needs a tagger — see the plan's Future Work.

### Function-word special cases (no tagger needed)

A handful of the highest-frequency words in English -- `a`, `an`, `the`, `to`,
`in`, `am` -- get a dedicated reduced pronunciation depending on context, and
misaki resolves this *before* the lexicon rather than through the tagged
`DEFAULT`/POS-variant mechanism above. This crate ports the parts of that
which don't need a tag (`src/special_case.rs`), plus a related generic
mechanism: any lexicon entry with a `'None'`-tagged variant (`this`, `by`,
`have`, `has`, `will`, `would`, ... 32 words total) takes that stressed
reading instead of `DEFAULT` when nothing follows it. Both are driven by
`future_vowel` -- whether the *next* token's phonemes start with a vowel, a
consonant, or there's no next token at all -- computed by walking the token
list right-to-left before assembling output.

Two related cases are *not* ported because they need a tag this crate
doesn't have, even though they don't fit the "no tagger" pattern above:

- Standalone `I` -- reference misaki always downgrades it to secondary
  stress (given a `PRP` tag); this crate keeps the lexicon's own `ˈI`
  (primary stress).
- Particle "in" ("log in", "check in") followed by another word -- reference
  keeps it stressed (`RP`/`ADV` tag); this crate assumes standalone "in" is
  always the `IN` (preposition) tag, so it comes out unstressed whenever
  something follows, same as prepositional "in" ("in time") correctly does.

See `src/special_case.rs`'s doc comments for the full list of what's ported
vs. skipped and why.

## Known limitation: no British lexicon

Only `us_gold`/`us_silver` are vendored (`gb_gold.json`/`gb_silver.json` are
not — see the plan's Future Work). The `british` flag is not a US/UK switch;
see the note above the first code sample.

## Known limitation: possessives with an orphaned apostrophe

For words like `dogs'` and `James'`, the reference emits a trailing `”`
(U+201D) — `dˈɔɡz”`, `ʤˈAmz”` — because it routes the orphaned apostrophe
through its espeak fallback. This port's tokenizer splits the trailing
apostrophe into its own `'` (U+0027) token, which is not in `PUNCTS` and is
therefore dropped, so it emits `dˈɔɡz` instead. Neither word is in the
vendored lexicon. This is an accepted, documented limitation, not a bug to
fix; the behaviour is pinned by a test in `src/tokenize.rs`.

## Testing

`cargo test` runs unit tests plus a parity check against a corpus generated
from the reference Python implementation. Regenerate the corpus with:

    nix-shell shell.nix --run 'python3 tools/gen_golden.py'

Run that from the **repo root**, not this crate's directory: `shell.nix` is a
relative path, and `tools/gen_golden.py` is too (the script itself resolves
its output directory from `__file__`, but nix-shell has to find `shell.nix`
first). Running it from `rust/misaki-en/` will fail to find either file.

Never hand-edit files under `tests/golden/`.
