# sayd-core

The engine behind [sayd](https://crates.io/crates/sayd): utterance queue,
sentence chunking, "write for the ear" text cleanup, configuration, and the
state machine that drives them.

Deliberately depends on neither an inference stack nor an audio backend nor
D-Bus. Synthesis reaches it through the `Synthesizer` trait and audio through
`AudioSink`, which is what lets the whole engine be driven in a unit test with
no model, no audio device and no display.

- `engine` — commands in, immutable snapshots out; `tick()` does one unit of
  work and returns
- `handle` — runs the engine on its own thread
- `queue` — four submission policies with per-source defaults
- `chunk` — two-phase: split to a character target, then re-split whatever
  overruns the model's phoneme-token budget once phonemised
- `cleanup` — URLs, markdown, code fences, acronyms
- `config` — TOML under XDG paths, atomic writes

## Licence

MIT.
