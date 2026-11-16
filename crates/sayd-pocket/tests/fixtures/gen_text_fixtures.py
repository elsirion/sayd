"""Regenerate text_cases.json from the reference PyTorch pocket-tts package.

Run from the repo root, with the bench venv (isolated mode):

    source .tts-bench/env.sh
    HF_HUB_OFFLINE=1 .tts-bench/venv-pocket/bin/python -I \
        crates/sayd-pocket/tests/fixtures/gen_text_fixtures.py \
        .tts-bench/cache/hf/hub/models--kyutai--pocket-tts-without-voice-cloning/snapshots/*/languages/english_2026-04/tokenizer.json \
        crates/sayd-pocket/tests/fixtures/text_cases.json

Args: <tokenizer.json> <output.json>. Needs pocket-tts 4.0.1. The settings
mirror the english_2026-04 bundle: pad_with_spaces_for_short_inputs=false,
remove_semicolons=false, append_terminal_punctuation=true,
capitalize_first_letter=true, replace_characters = curly single quotes.
Per input it records the tokenizer ids, prepare_text_prompt's output and
split_into_best_sentences(..., 50). Empty input makes the package raise
ValueError; that is recorded as "error": true.
"""

import json
import sys

from pocket_tts.models.text_chunking import prepare_text_prompt, split_into_best_sentences
from pocket_tts.modules.text_conditioner import JsonTokenizer

REPLACE = {"’": "'", "‘": "'"}
MAX_TOKENS = 50

LINES = [
    "I've updated the config parser and added two tests. Everything passes, and the build is green.",
    "Oh, wonderful. The tests passed on the first try. I'm sure that won't come back to haunt us at all.",
    "Good news and bad news. Good news: I found the bug! Bad news: it was in your commit called quick fix. From three weeks ago.",
    "Okay, okay, you got me. I deleted the wrong file. Restoring it now.",
    "Alright, the migration finished. It only took four tries and one small existential crisis.",
    "Wait, you want me to rewrite the whole thing in Rust? Sure. Why not. What could possibly go wrong.",
]

PARAGRAPH = (
    "The daemon reads a line from the socket, strips the framing, and hands the text to the engine. "
    "If the engine is busy, the request waits its turn; otherwise it starts at once! "
    "Does anyone really read these logs at 3.5 seconds per line? Probably not, but we keep them anyway, "
    "because one day somebody will need to know why foo_bar() returned early; that day is always a bad one. "
    "Version 2.0 of the protocol adds a header, a checksum, and a small amount of optimism... "
    "It also adds a very long clause that keeps going without any punctuation at all, so that the splitter has to fall back "
    "to something smaller than a sentence, and decide where to cut it, for the model to cope with it gracefully, "
    "and then there is the end of it. "
)

CASES = LINES + [
    "It’s Bob’s ‘quoted’ thing.",
    "Hello there,",
    "Wait for it -",
    "Pi is about 3.5 and e is 2.718, roughly.",
    "Use a tool, e.g. a hammer, or similar.",
    "Call foo_bar() twice.",
    "Well... I suppose so... maybe.",
    "She said \"Hi?\", then left.",
    "(quoted words)",
    "He said: \"go home\"",
    "lowercase start without a stop",
    "  \n spaced   out \r\n text  ",
    "Semi; colon: test; here",
    "It costs $4.50 today. Tomorrow 5.25 maybe.",
    "Café naïve über straße",
    "emoji \U0001f600 and 中文 text",
    "Hi",
    "ok",
    "",
    PARAGRAPH * 2,
]


def main():
    tok_path, out_path = sys.argv[1], sys.argv[2]
    tok = JsonTokenizer(4000, tok_path)
    cases = []
    for text in CASES:
        case = {"text": text, "ids": tok.encode(text)}
        try:
            prepared, frames = prepare_text_prompt(
                text, False, False, True, True, REPLACE
            )
            chunks = split_into_best_sentences(
                tok, text, MAX_TOKENS, False, False, True, True, REPLACE
            )
            case.update(error=False, prepared=prepared, frames_after_eos_guess=frames, chunks=chunks)
        except ValueError:
            case.update(error=True, prepared="", frames_after_eos_guess=0, chunks=[])
        cases.append(case)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(cases, f, ensure_ascii=False, indent=1)
        f.write("\n")
    print(len(cases), "cases,", sum(len(c["chunks"]) for c in cases), "chunks")


main()
