# Rewording: an explicit provider, and a model that will not stop thinking

Date: 2026-08-16
Status: approved, not yet implemented

> **2026-08-16 update, later commit:** the ceiling numbers below --
> `reword.timeout_ms`'s 2000 ms ceiling ("The problem, measured", "Out of
> scope") and the flat 10 s `REWORD_HTTP_CEILING` ("Scoped to hardware fast
> enough...") -- were both removed by a later commit so a local model can be
> given as long as it legitimately needs. They are left as written here
> because they were accurate measurements and constraints at the time this
> document was approved, and rewriting them in place would misrepresent what
> this design was reasoned against. Do not read either number as current
> behaviour; see `RewordConfig::timeout_ms` and
> `sayd::reword::http::http_ceiling` for what replaced them.

## The problem, measured

`gemma-4-E4B-it-Q4_K_M` served by a local llama.cpp router is thinking-capable.
For notification-rewrite prompts it usually emits a reasoning block, which
llama.cpp routes into `reasoning_content` while `content` stays empty until the
thinking ends. The thinking does not end inside the client's fixed
`MAX_TOKENS = 256`, so the request runs to the cap:

    finish_reason: "length"   completion_tokens: 256   content: ""
    reasoning_content: "Thinking Process:\n1. **Analyze the input:** ..."
    timings: predicted 256 tok in 13332 ms -> 19.2 tok/s

256 tokens at the 8--19 tok/s this machine sustains is 13--33 s, past the 10 s
`REWORD_HTTP_CEILING` that `REWORD_TEST_CEILING` reuses. The settings window's
Test row therefore reports "No answer after 10.0 s".

It is intermittent because `temperature = 0.2` still samples. The same prompt,
ten times, against a warm resident model:

| Request | Over 10 s | Emitted reasoning |
|---|---|---|
| as shipped today | 9/10 | 9/10 |
| `chat_template_kwargs: {enable_thinking: false}` | 0/6 | 0/6 |
| `reasoning_budget: 0` | 6/6 | 6/6 |
| `max_tokens: 48` | 0/6 | 5/6 |

`chat_template_kwargs` is the mechanism that works; the router ignores
`reasoning_budget`. Lowering `max_tokens` bounds the latency without producing
a usable answer -- `content` is still empty 5 times in 6.

Two things this is **not**. It is not a cold model load: the journal shows the
model resident throughout, and a cold load is ~6 s in any case. It is not
general endpoint slowness: the non-thinking path answers in 0.5--1.3 s.

### What it costs beyond the Test button

- A thinking response that completes still has empty `content`, so
  `http.rs`'s parse returns `Malformed("the response carried no
  choices[0].message.content")`. The response was well-formed; the message is
  wrong about what happened.
- A thinking response that does *not* complete is cut by the client ceiling
  into `RewordError::Ceiling`, which is transport-class (`reword/mod.rs`'s
  "`Unreachable` and `Ceiling` are the same row of §8's table"). Three in a
  row open the transport breaker for 60 s, though the transport is healthy.
- Each runaway holds one of the two `REWORD_MAX_INFLIGHT` permits for the
  full ceiling.

In the daemon path none of this reaches the speaker: `timeout_ms` (1500 ms
default, 2000 ms ceiling) expires long first and the raw text is spoken, which
is the designed degradation. The feature is simply never delivering a rewrite.

## Decisions

### `reword.provider` exists, and the README paragraph denying it does not

README's *Endpoints* section currently opens: "There is no `provider` setting,
because there is nothing to choose between: PPQ, Ollama, llama.cpp's `server`,
LM Studio, vLLM and OpenAI all speak the same request." That was true of the
request and is now false of one field in it. Providers agree on
`/chat/completions` and disagree on how to turn reasoning off. `base_url` still
says *where*; `provider` says *in which dialect thinking is disabled*.

Scope is deliberately that one job. `provider` does **not** take over the API
key row (`settings::model::reword_key_row_applies` keeps deriving from loopback
plus preset match) and does not drive the endpoint preset menu. Those work and
are tested; widening the field's remit would grow the diff past the bug.

### Two values, both measured

    provider = "llama-cpp"   # chat_template_kwargs: {enable_thinking: false}
    provider = "generic"     # sends nothing; byte-identical to today

Only behaviour verified against a real server ships. vLLM documents the same
`chat_template_kwargs` upstream but is unverified here, and Ollama and
LM Studio have not been checked at all, so none of the three gets a name yet.
An `openai` variant was considered and dropped: it would send nothing, making
it indistinguishable from `generic` on the wire. Adding a provider later is one
match arm and one test.

### Parsed leniently, enforced at use

The field deserializes as `Option<String>` and resolves to the enum at its two
points of use -- `reword_startup_refusal` and `HttpRewriter::new` -- not at
parse. `RewordConfig` is `#[serde(default)]`, so a strict enum would make a
typo -- `provider = "llama.cpp"` -- fail `toml::from_str` for the whole
document, and `load_str` would return `Config::default()`, silently discarding
every other setting in the file. Parse-lenient/enforce-at-use is what
`timeout_ms`'s clamp and `settings::model::normalize` already do.

### Missing provider is fatal only where the user asked for something undeliverable

`enabled = true` with no usable `provider` is a contradiction: the user asked
for automatic rewording that cannot be configured. The daemon prints the fix
and exits 1.

Everywhere else it degrades, because a hard exit in those places costs more
than it buys. The settings window is reached through the running daemon's tray,
so a daemon that refuses to boot removes the GUI the field would be set with;
and `config_watch` reloads this file live, where there is no "start" to fail.

| Situation | Behaviour |
|---|---|
| `enabled = true`, provider absent or unrecognised | print the fix, exit 1 |
| `enabled = false` | starts normally; the table stays inert |
| `say --reword` with no provider | `RewordError::NotConfigured`, raw text spoken, logged once |
| live reload drops or breaks provider | warn, rewording off, daemon lives |
| built without `--features reword` | unchanged; `enabled = true` is already a no-op with its own diagnostic |

The startup check is therefore behind `#[cfg(feature = "reword")]`.

### `max_tokens` becomes `3 * max_chars`

The fixed 256 is replaced by three times the longest text the feature will
accept, so 1200 by default and 96..=6000 across `max_chars`'s existing
32..=2000 clamp. This preserves the reason the constant was generous in the
first place -- a tight limit truncates mid-sentence and a truncated sentence
passes the length check and gets spoken, while a generous one means an
over-long answer arrives complete and is rejected whole.

Consequence, stated rather than buried: at 1200 tokens and 19 tok/s a runaway
would need 63 s, so `max_tokens` no longer bounds latency at all.
`REWORD_HTTP_CEILING` becomes the sole bound. It already was the real one at
256.

### Hitting the cap is an error that speaks the raw text

The client parses `finish_reason` and `reasoning_content`. On
`finish_reason == "length"` it returns a new `RewordError::Truncated(String)`
naming the real cause, and the existing degradation speaks the original:

    error: reword: the model hit the 1200-token cap without finishing
           (1200 tokens of reasoning, no answer) -- speaking the text as written

The trigger is `finish_reason` alone, **whether or not `content` is empty**. A
truncated answer with content in it is the more dangerous case, not the safer
one: it is the mid-sentence truncation the generous token limit exists to
avoid, and it would otherwise pass the guard's length check and be spoken. An
answer that merely carries a populated `reasoning_content` alongside a complete
`content` and `finish_reason: "stop"` is fine and is accepted.

`Truncated` is not transport-class, so it cannot open the 60 s breaker that
three `Ceiling`s do today. This is the half of the fix that still works when
`provider` is set wrong, or when a model that did not used to reason starts.

Scoped to hardware fast enough to reach the cap at all. `finish_reason:
"length"` only arrives if the generation *finishes* `max_tokens` inside the
10 s `REWORD_HTTP_CEILING` -- otherwise the client's own timeout ends the
request first, as `RewordError::Ceiling`, which *is* transport-class and
does open the breaker. At the 1200-token default that needs 120 tok/s;
this machine sustains 8--19 tok/s under load (measured above), so a runaway
here still times out as `Ceiling` before it can be classified as
`Truncated`. It was already close to true at the old 256-token cap -- 25.6
tok/s needed, against a measured ceiling of 19.2 -- and raising the cap to
1200 moved the requirement further out of reach rather than closer. None of
this makes the branch wrong: `chat_template_kwargs` is what stops the
reasoning happening at all, measured at 6/6, and is the actual fix.
`Truncated` is a real classification for a provider that answers with
`finish_reason: "length"` well inside the ceiling -- a faster box, a
smaller model, a remote GPU endpoint -- just not a safety net this reporting
hardware will see fire.

## Components

| Unit | Change |
|---|---|
| `sayd-core::config` | `RewordConfig::provider: Option<String>`; `Provider` enum with `from_config`; `reword_startup_refusal(&RewordConfig) -> Option<String>` |
| `sayd::reword::http` | `ChatRequest.chat_template_kwargs` (skipped when `None`); `max_tokens` derived from `max_chars`; parse `finish_reason` and `reasoning_content`; new truncation error |
| `sayd::main` | call `reword_startup_refusal` after `Config::load`, exit 1 with the message, behind the feature gate |
| `sayd::settings::model` | render the new error in the Test row |
| `README.md` | rewrite *Endpoints*' opening paragraph; add `provider` to the config example; note thinking in *The deadline* |

`reword_startup_refusal` is a pure function over `RewordConfig` precisely so
the rule is testable without `main()`.

`config_watch` needs no change. `build_rewriter` -> `HttpRewriter::new` is the
single production construction point and it is called per attempt, so
rejecting an unusable `provider` there is what produces `NotConfigured` on the
`--reword` path, on the Test row, and after a live reload alike. One check,
three behaviours, no reload-specific code.

## Testing

Written first, per test-driven-development.

**Config**
- `provider` absent parses to `None`, rest of config intact.
- An unrecognised `provider` leaves every other field parsed and set -- the
  regression that a strict enum would introduce.
- Round-trips through the settings window's whole-config serialisation.

**Startup rule**
- The four combinations of `enabled` x provider-usable; only
  `enabled && !usable` refuses.
- The refusal message names the field and at least one valid value.

**Request**
- `llama-cpp` sends `chat_template_kwargs: {enable_thinking: false}`.
- `generic` sends no such key -- asserted on the serialised body, so the
  "byte-identical to today" claim is pinned.
- `max_tokens == 3 * max_chars`, at the default and at both clamp ends.

**Response**
- `finish_reason: "length"` with empty content yields `Truncated`, not
  `Malformed`.
- `finish_reason: "length"` with *non-empty* content also yields `Truncated` --
  the mid-sentence truncation is never spoken.
- `Truncated` is not transport-class: three of them do not open the breaker.
- `finish_reason: "stop"` with a populated `reasoning_content` beside a
  complete `content` is still accepted.

## Out of scope

- vLLM, Ollama, LM Studio dialects (unverified; add when measured).
- `provider` absorbing the API key row or the preset menu.
- Any change to `timeout_ms`, its 2000 ms ceiling, or the breaker's shape.
- Model choice. This machine has no dedicated GPU and sustains 8--19 tok/s
  under load, so even a clean 27-token answer measured 1.5 s -- already past
  the 1500 ms default budget. Worth its own investigation, not this one.
