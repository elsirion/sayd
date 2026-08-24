# One owner for the text pipeline, and a settings window that renders a schema

Date: 2026-08-24
Status: implemented (branch settings-schema-and-pipeline-owner)

Two changes, in this order. The first fixes a defect and gives the text
pipeline a single owner. The second turns the settings window from 3,444
hand-laid lines into a renderer over a declared schema, and lands the
sub-page hierarchy on top of it.

They are independent -- Milestone A touches no GTK, Milestone D touches no
pipeline code -- but A goes first, because the page order D renders is only
honest once A has made it true.

## Why: what is inconsistent today

**Two length limits, two homes, three checks.** `Config::max_chars` (global,
shown under `Engine`) and `RewordConfig::max_chars` (shown under `Reword` as
"Longest text to rewrite"). The global one is enforced in
`notify/monitor.rs`'s `speak` *and* again in `Engine::submit`.

**`reword.enabled` is a source flag inside a stage config.** It means
"rewrite my *notifications*". Its per-call equivalent is `--reword`, which
`dbus.rs`'s `wants_reword` deliberately keeps out of `SayOpts`. One
capability, two spellings, two layers.

**The text pipeline has no owner.** `clean` is called from
`reword/mod.rs`'s `admit_with` and again from `engine.rs`'s `submit`. The
ordering -- clean, reword, clean -- exists as prose in three doc comments
rather than as one function, and its correctness is justified by a claimed
idempotence:

> `Engine::submit` cleans it again to the same value, because `clean` is
> idempotent.  -- `RewordPlan::admit_with`

> The rewrite sits between the two, which is sound because `clean` is
> idempotent -- pinned by `cleanup::tests::clean_is_idempotent`.
> -- `notify/monitor.rs::speak`

**The claim is false.** `clean_is_idempotent` asserts it over eleven
hand-picked strings, none of which is table-shaped. Measured on master:

    in:    "| 1. | 2,5 EUR |\n| 3. | 1.234 |"
    once:  "1. 2,5 E U R 3. 1.234"
    twice: "2,5 E U R 3. 1.234"

The first pass turns table pipes into spaces, which exposes a leading `1.`
that the second pass reads as an ordered-list marker and eats. The path that
cleans twice is the refusal path -- `will_reword` says no (fewer than three
words, over `max_chars`, breaker open, no provider), `admit_with` hands back
the *cleaned* string, and `Engine::submit` cleans it again. So a notification
that is not reworded can be spoken with less content than one that never went
near the rewriter.

**Per-source settings exist for exactly one source.** Notifications have
`enabled`, `cooldown_secs`, `allow`, `speak_app_name`, `speak_body`. `say`
and the selection hotkey have none.

**The window is half declarative.** `CLEANUP_SWITCHES` and
`NOTIFICATION_SWITCHES` are tables of `(title, subtitle, get, set)`. Engine,
Voice and Reword are ~60 hand-written lines each repeating the same
show/register/connect triple. `chunking.target_chars`,
`chunking.lookahead_chunks` and `reword.api_key_env` have no rows at all --
the last of these is the one the field's own doc comment tells users to
prefer.

---

# Milestone A: one `prepare()`

## The rule

**Every string that reaches `Engine::submit` is cleaned exactly once, by
`Engine::submit`.** Nothing upstream hands it text that has already been
cleaned. The cleaned-for-the-provider copy exists inside the plan, is sent on
the wire, and never escapes.

That is the whole fix. It removes the double-clean, and with it the
dependency on an idempotence that does not hold.

## The owner

A new `crates/sayd/src/pipeline.rs`:

```rust
/// Whether this submission wants a rewrite.
pub enum Ask {
    /// The caller did not ask and nothing asks on its behalf: plain `say`.
    Never,
    /// `[reword] notifications` governs. The notification path.
    Automatic,
    /// The caller asked: `say --reword`, D-Bus `reword: true`.
    Requested,
}

/// Longer than `max_chars`. Carries both numbers so each caller can say so
/// in its own register -- a D-Bus error, a log line -- from one measurement.
pub struct TooLong { pub chars: usize, pub limit: usize }

/// The one path from a source's raw text to what the engine is handed.
///
/// Order is fixed here and nowhere else: length gate, then admission (which
/// cleans its own copy for the wire), then the rewrite or the original.
pub async fn prepare(
    text: impl Into<Origin>,
    cfg: &Config,
    ask: Ask,
) -> Result<Spoken, TooLong>;
```

Every source goes through it, `Ask::Never` included -- that is what makes it
the owner rather than a third path beside the two that exist. The length gate
is today's early return in `monitor.rs::speak`, moved here so both sources
get it, and returned rather than acted on: `dbus.rs` turns `TooLong` into the
`fdo::Error` its callers already expect, `monitor.rs` into the warning it
already logs. `Engine::submit` keeps its own `max_chars` check -- it is the
engine's guarantee about its own queue, not this gate's job to replace.

`dbus.rs::maybe_reword` and the reword half of `monitor.rs::speak` both
collapse into calls to `prepare`.

## What changes inside `reword`

`RewordPlan::admit_with` keeps cleaning -- `will_reword` must judge the
string that will actually be sent, and CRITICAL 1 requires that no code
fence or URL reaches the provider. Two things change:

- On `Err`, it returns the **original**, not the cleaned string. The refusal
  path is then byte-identical to the rewording-off path.
- `Spoken::fallback` carries the **original**, not the cleaned original, for
  the same reason: it is submitted through `Engine::submit`, which cleans.

`RewordPlan` gains a field for the original alongside the cleaned `text` it
already owns. The cleaned copy stays exactly as load-bearing as it is today:
it is what `will_reword` judged and what `resolve` sends.

## Tests

- Delete `cleanup::tests::clean_is_idempotent`. It asserts something false in
  general and nothing will depend on it. Replace it with
  `clean_is_not_assumed_idempotent`, which asserts the table case above
  *diverges* -- so anyone who reintroduces the assumption finds a test that
  says why it is wrong.
- New: for a corpus that includes the table case, every one of the three
  paths (rewording off, rewrite refused, rewrite accepted then refused by the
  engine) submits a string equal to `clean(original)`.
- New: `prepare` returns `None` above `max_chars` and does not touch the
  rewriter -- the existing "must not cost a network round trip" property,
  moved with the gate.
- The three doc comments asserting idempotence are rewritten to state the
  once-only rule instead.

---

# Milestone D: the window renders a schema

## The schema

A new `crates/sayd/src/settings/schema.rs` declaring the whole config surface
as data. This is `CLEANUP_SWITCHES` generalised from two groups to all of
them, not a new idea in this file.

```rust
pub enum Row {
    Bool  { title, subtitle, get: fn(&Config) -> bool,   set: fn(&mut Config, bool) },
    Int   { title, subtitle, min, max, step, page_increment,
            get: fn(&Config) -> f64, set: fn(&mut Config, f64) },
    Text  { title, subtitle, secret: bool,
            get: fn(&Config) -> String, set: fn(&mut Config, String) },
    Choice{ title, options: Options, unknown: fn(&str) -> String,
            get: fn(&Config) -> String, set: fn(&mut Config, String) },
    /// The rows no descriptor can describe. Five of them, listed below.
    Custom(fn(&Ui, &Config) -> gtk::Widget),
}

pub enum Options {
    Static(&'static [(&'static str, &'static str)]),  // value, label
    /// Voices are discovered by scanning the models directory.
    Discovered(fn() -> Vec<(String, String)>),
}

pub struct Group { title, description: Option<&'static str>, rows: &'static [Row] }

pub struct Page {
    title: &'static str,
    /// The top-level on/off. Rendered above the groups, in its own group.
    master: Option<Row>,
    groups: &'static [Group],
    /// What the root page's navigation row says underneath the title.
    summary: fn(&Config) -> String,
}
```

`window.rs` keeps: `Ui`/`UiState`/`WeakUi`, the `quiet` echo guard, `Combo`,
`Spin`, `bind_entry`/`commit_entry`, `group_description`, and the five
`Custom` bodies. It loses every hand-written show/register/connect triple.
The renderer walks the schema, builds the row, registers its redraw closure
with `Ui::row`, and connects its handler -- once per `Row` variant instead of
once per row.

**The five `Custom` rows**, which stay hand-written because no descriptor
describes them: the Voice group's Test button; the Reword Test entry and its
result row; the allowlist's entry-plus-list; and the two suggestion groups
with their icons. Five of roughly thirty. If a later field cannot be
described either, that is a signal to extend the enum, not to add a sixth
`Custom`.

## The hierarchy

```
sayd Settings                                  (root page)
├─ Voice and speed     Voice · Speed · Speed mode · [Test]
├─ Engine              Model · Threads · Idle unload
├─ Text                → Cleanup          "5 transforms on · URLs: say “link”"
│                      → Rewording        "Off" / "qwen3:32b via Ollama"
└─ Sources             → say command      "Up to 4000 characters"
                       → Notifications    "On · 6 applications · 5 s cooldown"
```

```
Cleanup                              (subpage)
  [Clean up text]                    ← master: cleanup.enabled, NEW
  Transforms   Collapse whitespace · Rejoin hyphenation · Strip Markdown
               Drop code blocks · Spell out acronyms
  URLs

Rewording                            (subpage)
  [Reword text]                      ← master: reword.enabled, REDEFINED
  Rewrite notifications automatically   ← reword.notifications, NEW NAME
  Endpoint     Endpoint · Provider · Model · API key · API key variable
  Limits       Deadline · Longest text to rewrite
  Test         (Custom)

say command                          (subpage)
  Long-text guard                    ← moved out of Engine

Notifications                        (subpage)
  [Speak notifications]              ← master: notifications.enabled, unchanged
  What to say  Say the application name · Say the body
  Rate         Cooldown
  Applications to announce  (Custom)
  Seen notifying            (Custom)
  Common applications       (Custom)
```

The nav rows sit directly on the root under a group heading rather than
behind an intermediate `Cleanup` page: a page whose entire content is two
more nav rows is a click that shows nothing.

**There is no `say` master switch.** It was in the original sketch and it
does not earn its keep: notifications arrive uninvited, so a switch for them
is worth having, but `say` only runs when the user runs it. Switching it off
would mean `Say` returning a D-Bus error naming a setting -- a new failure
mode in exchange for nothing. The `say` page holds the submission limit that
belongs to it and no on/off.

**The summaries are load-bearing**, not decoration. They are what makes the
root page a status view rather than a menu; without them the change trades
one long page for four short ones plus clicks.

## Config changes

Three fields, each with an exact default and migration.

**`cleanup.enabled: bool`, default `true`.** When false, `clean` returns the
text untouched. It short-circuits inside `clean` itself, so both call sites
(`Engine::submit` and `admit_with`) honour it without either one testing it.
Absent from an old file, `serde(default)` supplies `true`, and behaviour is
unchanged.

**`reword.notifications: bool`, default `false`.** This is today's
`reword.enabled`, renamed to say what it means. Every consumer of the old
meaning moves to it: `notify_cooldown_min_secs`, `reword_startup_refusal`,
and `RewordPlan::automatic`.

**`reword.enabled: bool`, default `true`.** Redefined as the master: when
false, neither `Automatic` nor `Requested` admits, and the endpoint config is
kept rather than cleared. Default `true` so that a config that has never
mentioned rewording behaves exactly as it does today -- nothing happens
without a provider in any case.

**Migration.** A file carrying `[reword] enabled` but no
`[reword] notifications` is an old file, and its `enabled` carries the old
meaning. `Config::load_str` detects the absent key, moves the value across,
and sets the master:

    notifications = old enabled
    enabled       = true

This is behaviour-identical to today in every case, including the
configured-provider-with-`enabled=false` user whose `say --reword` keeps
working. Detection reads the parsed TOML document for the key's presence
before deserialising into `Config`; once the window or any other writer has
rewritten the file, both keys are present and no inference runs again.

`reword_startup_refusal` moving to `notifications` matters: migrating
everyone to `enabled = true` while the refusal still keyed on `enabled` would
make an unconfigured daemon refuse to boot.

## Also in scope

- **`reword.api_key_env` gets a row** ("API key variable"). Its own doc
  comment says to prefer it over `api_key`; the window is the only place most
  users will touch this config, so today the recommended path is the
  unreachable one.
- **`max_chars` moves out of `Engine`** to the `say` page. It is a submission
  limit, not an engine property.
- **`chunking.*` stays unreachable.** YAGNI; nothing has asked for it.
- **Mute stays out of the window.** The tray owns it; a second control for
  the same bit is a race waiting to happen.

## Out of scope, deliberately

**One option vocabulary with three binding sites** -- built-in, config,
per-call -- so that `SayOpts` stops being a hand-picked subset and
`say --no-cleanup` becomes possible. It is the right conceptual answer and it
is much cheaper once the schema exists, because a binding site becomes a
column in a table that already lists every option. Sequenced after this.

**Per-source profiles with inheritance.** That is the above plus a dimension,
and the dimension is speculative until someone names a source they want a
different voice for.

## Testing

The nine GTK scenarios in `window.rs` locate rows by walking down from
`ui.window`; a subpage that has not been pushed is not in that tree. The test
helpers gain a `push(page_title)` step, and the scenarios that touch reword
rows push the Rewording page first. `Ui::rows` is unaffected -- it is a `Vec`
of redraw closures with no tie to visibility, and a row on an unpushed page
must still redraw, since the page it is on may be pushed later without
rebuilding.

All GTK tests run under `scripts/headless.sh`.

New coverage:

- Every `Page` in the schema is reachable from the root, and every root
  navigation row names a page that exists. Pins the tree against a page
  declared and never linked.
- Each `summary` renders from a known config to a known string.
- The migration: an old file with `enabled = true` and no `notifications`
  loads as `notifications = true, enabled = true`; with `enabled = false` and
  a provider set, as `notifications = false, enabled = true`, and
  `say --reword` still admits. A new file with both keys is left alone.
- `cleanup.enabled = false` makes `clean` the identity, checked at both call
  sites.
- The master switches gate what they claim to: with `reword.enabled = false`,
  neither `Automatic` nor `Requested` admits, and the endpoint fields survive
  a round trip through the window.
