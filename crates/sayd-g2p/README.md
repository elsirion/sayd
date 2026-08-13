# sayd-g2p

One `Phonemizer` over two grapheme-to-phoneme tiers for Kokoro text-to-speech.

> **This crate does not build without espeak-ng.** It links the library at
> build time, so `cargo build` fails at the link step on a machine that lacks
> it — this is not a runtime-only requirement, and no cargo feature turns it
> off. See [Requirements](#requirements).

American text goes through [`sayd-misaki-en`](https://crates.io/crates/sayd-misaki-en)
— vendored misaki lexicons, number and currency normalisation, `-s`/`-ed`/`-ing`
stemming — with espeak-ng as the per-word fallback for anything the lexicon and
stemmer both miss.

British text bypasses the lexicon entirely and takes a whole-text espeak `en-gb`
call, because only US lexicons are vendored. This must stay a whole-text call:
espeak phonemises at clause level, and per-word calls produce audibly different
output.

```rust
let p = sayd_g2p::Phonemizer::new();
let us = p.phonemize("tomato", sayd_g2p::Dialect::American);
let gb = p.phonemize("tomato", sayd_g2p::Dialect::British);
assert_ne!(us, gb);
```

## Requirements

Links against `libespeak-ng` at build time via `cargo:rustc-link-lib=espeak-ng`.
Set `ESPEAK_LIB_DIR` if it is not on the default search path, and
`ESPEAK_DATA_PATH` to the espeak-ng data directory at runtime:

```sh
# Debian/Ubuntu
apt install libespeak-ng-dev
export ESPEAK_DATA_PATH=/usr/share/espeak-ng-data
# Nix
export ESPEAK_LIB_DIR=$(nix eval --raw nixpkgs#espeak-ng)/lib
export ESPEAK_DATA_PATH=$(nix eval --raw nixpkgs#espeak-ng)/share/espeak-ng-data
```

The library is linked rather than built from source on purpose: a `-sys` crate
that compiles espeak-ng would be slower and would not use the system's data
files. espeak-ng keeps global translator state, so every call is serialised
internally — `Phonemizer` is `Send + Sync` and safe to share, but calls do not
run concurrently.

Part of [sayd](https://crates.io/crates/sayd), a local speech daemon for Wayland.
