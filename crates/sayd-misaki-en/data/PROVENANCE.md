# Vendored lexicons

`us_gold.json` and `us_silver.json` are copied verbatim from **misaki v0.9.4**
(https://github.com/hexgrad/misaki), licensed Apache-2.0. See `LICENSE`.

- `us_gold.json`  — 90,201 entries. 89,411 are `word -> phonemes`; 790 are
  `word -> {TAG: phonemes}` where TAG is one of DEFAULT/NOUN/VERB/ADJ/VBD/
  VBN/VBP/ADV/DT/None.
- `us_silver.json` — 93,361 entries, all `word -> phonemes`. Lower confidence;
  consulted only after gold misses.

Regenerating: re-copy from an installed misaki of the same version. Do not
hand-edit.
