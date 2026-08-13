# sayd-misaki-en

English grapheme-to-phoneme for [Kokoro](https://huggingface.co/hexgrad/Kokoro-82M)
text-to-speech, ported to Rust from [misaki](https://github.com/hexgrad/misaki)
v0.9.4.

Turns English text into Kokoro-compatible phoneme strings with no Python, no
spaCy and no torch. misaki's two lexicons (183,562 entries) are compiled into
FSTs at build time and embedded, so lookup is zero-parse at startup.

```rust
let g2p = sayd_misaki_en::G2p::new(false);
assert!(!g2p.phonemize("Hello there.").is_empty());
```

Correctness is defined as byte-identical output against Python misaki on a
committed golden corpus, currently 99%+ on word-level parity. There is no
part-of-speech tagger, so POS-dependent lexicon entries resolve to their
`DEFAULT` variant — a documented ~1% error rate on homographs.

Only US lexicons are vendored. The `british` flag selects British suffix
phonemes in the stemmer but does **not** give British pronunciations for words
the lexicon knows; route British voices around this crate entirely.

Part of [sayd](https://crates.io/crates/sayd), a local speech daemon for
Wayland.

## Licence

Apache-2.0 — the vendored lexicons come from misaki under that licence. See
`LICENSE` and `data/PROVENANCE.md`. This differs from the rest of the sayd
workspace, which is MIT.
