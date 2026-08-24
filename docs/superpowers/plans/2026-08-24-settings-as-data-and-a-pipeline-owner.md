# One pipeline owner, and a settings window that renders a schema — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Give the text pipeline a single owner (fixing a live content-loss defect), then replace the hand-laid settings window with a renderer over a declared schema, and land the sub-page hierarchy on it.

**Spec:** `docs/superpowers/specs/2026-08-24-settings-as-data-and-a-pipeline-owner.md`

**Architecture:** Milestone A introduces `sayd::pipeline::prepare` as the one path from a source's raw text to `Engine::submit`, and makes every string that reaches the engine get cleaned exactly once, by the engine. Milestone D declares the config surface as data in `settings/schema.rs` and reduces `window.rs` to a renderer plus five hand-written `Custom` rows.

**Tech Stack:** Rust, GTK4 + libadwaita 0.9.2 (`v1_4`), zbus 5, tokio, ksni.

## Global Constraints

- **`clean` is not idempotent and nothing may assume it is.** Measured: `"| 1. | 2,5 EUR |\n| 3. | 1.234 |"` cleaned twice loses its leading `1.`.
- **Every string reaching `Engine::submit` is cleaned exactly once, by `Engine::submit`.** No caller hands it pre-cleaned text.
- **CRITICAL 1 holds:** the copy sent to a rewriting provider is cleaned first, so no code fence or URL leaves the machine.
- **The `reword` cargo feature is off by default.** Everything must compile and pass with it on and off. `reword::build_rewriter` is the only body that differs.
- **All GTK tests run under `scripts/headless.sh`.** They open real windows otherwise.
- Run clippy before every commit: `nix develop --command cargo clippy --workspace --all-targets -- -D warnings`.
- Test commands (all three must pass at every task boundary):
  - `scripts/headless.sh nix develop --command cargo test --workspace`
  - `scripts/headless.sh nix develop --command cargo test -p sayd --features reword`
  - `scripts/headless.sh nix develop --command cargo test -p sayd --features models`
- Baseline counts before this plan: 611 / 643 / 618.

---

### Task 1: The refusal path stops cleaning twice

**Files:**
- Modify: `crates/sayd/src/reword/mod.rs` (`RewordPlan`, `admit_with`, `reword_or_original`)
- Modify: `crates/sayd-core/src/cleanup.rs` (replace `clean_is_idempotent`)
- Modify: `crates/sayd/src/notify/monitor.rs` (`speak`'s doc comment)
- Modify: `crates/sayd-core/src/engine.rs` (`submit`'s cleaning comment, if it asserts idempotence)

**Interfaces:**
- Produces: `RewordPlan` gains a private `original: String`. `admit_with` returns `Err(original)`. `Spoken::fallback` carries the original.

The defect: `admit_with` cleans, and on refusal hands the *cleaned* string back through `Err`; both consumers (`dbus.rs:183`, `monitor.rs`) pass it to `Engine::submit`, which cleans again. Same for `Spoken::fallback`, which is submitted when the engine refuses a rewrite.

- [ ] **Step 1: Write the failing test** in `crates/sayd/src/reword/mod.rs`'s test module. Use the table string, a config whose provider is unset (so `context` returns `None` and `admit` refuses), and assert the `Err` is the **original**, not the cleaned form:

```rust
#[test]
fn a_refused_plan_hands_back_the_original_not_a_cleaned_copy() {
    const TABLE: &str = "| 1. | 2,5 EUR |\n| 3. | 1.234 |";
    let cfg = RewordConfig::default(); // no provider: `context` refuses
    let cleanup = CleanupConfig::default();
    let back = RewordPlan::automatic(Written(TABLE.to_string()), &cfg, &cleanup)
        .expect_err("no provider, so no plan");
    assert_eq!(
        back, TABLE,
        "the refusal path must hand back what it was given; `Engine::submit` \
         cleans, and cleaning a cleaned string is not a no-op"
    );
}
```

- [ ] **Step 2: Run it and watch it fail.** `nix develop --command cargo test -p sayd reword::tests::a_refused_plan` — expected FAIL, left is the cleaned string.

- [ ] **Step 3: Make `admit_with` keep both strings.** Clean into a local for judging and for the wire; return the original on refusal; store both on the plan.

```rust
fn admit_with(
    text: String,
    cfg: &RewordConfig,
    cleanup: &CleanupConfig,
    rewriter: Arc<dyn Rewriter>,
    state: Arc<RewordState>,
) -> Result<RewordPlan, String> {
    // Order is load-bearing. Cleanup comes first so `will_reword` judges the
    // string that will actually be sent, and so CRITICAL 1 holds: the copy
    // on the wire has had its code fences and URLs removed.
    //
    // What goes *back* on refusal is the original, untouched. `clean` is
    // NOT idempotent -- a markdown table cleaned twice loses its leading
    // list marker -- so handing back the cleaned form would have
    // `Engine::submit` clean it a second time and speak less than the
    // rewording-off path speaks for the same notification.
    let cleaned = clean(&text, cleanup);
    if !will_reword(&cleaned, cfg, &state) {
        return Err(text);
    }
    Ok(RewordPlan { rewriter, state, original: text, text: cleaned, cfg: cfg.clone() })
}
```

- [ ] **Step 4: Make the fallback the original too.** In `reword_or_original`, the `Spoken::fallback` built when a rewrite is produced must be `self.original`, not `self.text`. Thread the original through from the plan.

- [ ] **Step 5: Replace the idempotence test** in `crates/sayd-core/src/cleanup.rs`. Delete `clean_is_idempotent` — it asserts something false over eleven hand-picked strings — and pin the counterexample so nobody reintroduces the assumption:

```rust
#[test]
fn clean_is_not_idempotent_and_callers_must_not_assume_it_is() {
    // The first pass turns the table's pipes into spaces, which exposes a
    // leading `1.` that the second pass reads as an ordered-list marker and
    // drops. Anything that cleans an already-cleaned string can lose
    // content this way; see `RewordPlan::admit_with`.
    let cfg = CleanupConfig::default();
    let once = clean("| 1. | 2,5 EUR |\n| 3. | 1.234 |", &cfg);
    let twice = clean(&once, &cfg);
    assert_ne!(
        once, twice,
        "if this now passes, `clean` may have become idempotent -- check \
         before relaxing any caller that relies on it not being assumed"
    );
    assert_eq!(once, "1. 2,5 E U R 3. 1.234");
    assert_eq!(twice, "2,5 E U R 3. 1.234");
}
```

- [ ] **Step 6: Pin the property that actually matters.** New test asserting all three paths submit `clean(original)`: rewording off, rewrite refused by `will_reword`, and rewrite accepted then refused by the engine (the `fallback` path).

- [ ] **Step 7: Rewrite the doc comments that assert idempotence.** `RewordPlan::admit_with`, `notify/monitor.rs::speak` ("which is sound because `clean` is idempotent -- pinned by `cleanup::tests::clean_is_idempotent`"), and any matching sentence in `engine.rs`. Each states the once-only rule instead.

- [ ] **Step 8: Run all three suites and clippy, then commit.**

```bash
git commit -am "fix(reword): the refusal path handed back a cleaned string that got cleaned again"
```

---

### Task 2: `pipeline::prepare` becomes the owner

**Files:**
- Create: `crates/sayd/src/pipeline.rs`
- Modify: `crates/sayd/src/main.rs` (`mod pipeline;`)
- Modify: `crates/sayd/src/dbus.rs` (delete `maybe_reword`, `wants_reword` folds in)
- Modify: `crates/sayd/src/notify/monitor.rs` (`speak` calls `prepare`)

**Interfaces:**
- Produces: `pipeline::{prepare, Ask, TooLong}` exactly as the spec declares them.
- Consumes: `reword::{Origin, Written, Composed, Spoken, RewordPlan}`.

The ordering currently lives as prose in three doc comments. This makes it one function that every source calls, `Ask::Never` included — that is what makes it an owner rather than a third path.

- [ ] **Step 1: Write the failing tests** for the three `Ask` variants and the length gate. `Ask::Never` returns the text as written with no rewriter contact; `Ask::Automatic` honours `[reword] notifications`; `Ask::Requested` admits regardless of it; over `max_chars` returns `Err(TooLong { chars, limit })` **without** contacting the rewriter (the existing "must not cost a network round trip" property).

- [ ] **Step 2: Run them, watch them fail** (`pipeline` does not exist).

- [ ] **Step 3: Write `crates/sayd/src/pipeline.rs`** with `Ask`, `TooLong`, and `prepare` per the spec. Order inside: length gate, then `RewordPlan::automatic`/`requested`/none, then `plan.resolve().await` or `Spoken::as_written`.

- [ ] **Step 4: Rewire `dbus.rs`.** `maybe_reword` disappears; `say`, `say_selection` and `say_clipboard` call `prepare` with `Ask::Requested` when `wants_reword(opts)` and `Ask::Never` otherwise. `TooLong` becomes the `fdo::Error::Failed` message. Keep `submit_spoken` — CRITICAL 2's retry is unchanged.

- [ ] **Step 5: Rewire `monitor.rs::speak`.** The `max_chars` early return and the `RewordPlan::automatic` match both collapse into one `prepare(text, cfg, Ask::Automatic)` call. `TooLong` becomes the warning it already logs. Keep the spawn/await split: a refused plan submits inline, an admitted one detaches.

- [ ] **Step 6: Confirm `Engine::submit` still checks `max_chars`.** It is the engine's guarantee about its own queue and this gate does not replace it. Do not delete it.

- [ ] **Step 7: Build both feature configurations**, run all three suites and clippy, commit.

```bash
git commit -am "refactor(pipeline): one owner for the path from source text to the engine"
```

---

### Task 3: Config fields, redefinition and migration

**Files:**
- Modify: `crates/sayd-core/src/config.rs` (`CleanupConfig`, `RewordConfig`, `load_str`, `notify_cooldown_min_secs`, `reword_startup_refusal`)
- Modify: `crates/sayd-core/src/cleanup.rs` (`clean` short-circuit)
- Modify: `crates/sayd/src/reword/mod.rs` (`automatic`, and the master gate)
- Modify: `crates/sayd/src/settings/model.rs` (clamp/normalize paths that mention `reword.enabled`)

**Interfaces:**
- Produces: `CleanupConfig::enabled` (default `true`), `RewordConfig::notifications` (default `false`, today's `enabled`), `RewordConfig::enabled` redefined as the master (default `true`).

- [ ] **Step 1: Write the failing migration tests.**

```rust
#[test]
fn an_old_file_moves_reword_enabled_to_notifications() {
    let (c, err) = Config::load_str("[reword]\nenabled = true\nprovider = \"llama-cpp\"\n");
    assert!(err.is_none());
    assert!(c.reword.notifications, "the old key carried the auto-rewrite meaning");
    assert!(c.reword.enabled, "and the new master is on, so nothing changes");
}

#[test]
fn an_old_file_with_auto_off_keeps_reword_available() {
    // This user configured a provider and turned automatic rewriting off.
    // `say --reword` worked for them and must keep working.
    let (c, _) = Config::load_str("[reword]\nenabled = false\nprovider = \"llama-cpp\"\n");
    assert!(!c.reword.notifications);
    assert!(c.reword.enabled);
}

#[test]
fn a_new_file_carrying_both_keys_is_left_alone() {
    let (c, _) = Config::load_str("[reword]\nenabled = false\nnotifications = true\n");
    assert!(!c.reword.enabled, "the master the file asked for");
    assert!(c.reword.notifications);
}
```

- [ ] **Step 2: Run them; watch them fail.**

- [ ] **Step 3: Add the fields**, with doc comments saying which is which and that `enabled` defaults `true` so a config that never mentioned rewording behaves as it does today.

- [ ] **Step 4: Implement the migration in `load_str`.** Detect the absent `[reword] notifications` key by parsing the document to `toml::Value` first, then deserialising as now. If absent and `[reword] enabled` is present: `notifications = enabled; enabled = true`. Run it **before** the existing `timeout_ms` and `cooldown_secs` clamps, since `notify_cooldown_min_secs` reads the flag this moves.

- [ ] **Step 5: Move the consumers of the old meaning** to `notifications`: `notify_cooldown_min_secs`, `reword_startup_refusal`, `RewordPlan::automatic`. Migrating everyone to `enabled = true` while the refusal still keyed on `enabled` would make an unconfigured daemon refuse to boot — that is the failure this step exists to prevent, so give it a test.

- [ ] **Step 6: Gate on the master.** `RewordPlan::admit` (or `context`) refuses when `!cfg.enabled`, so neither `Automatic` nor `Requested` admits and the endpoint fields survive untouched.

- [ ] **Step 7: `cleanup.enabled` short-circuits inside `clean`**, so both call sites honour it without either testing it. Test at both sites.

- [ ] **Step 8: All three suites, clippy, commit.**

```bash
git commit -am "feat(config): a rewording master switch, a cleanup master, and the migration for both"
```

---

### Task 4: The schema, rendering today's layout

**Files:**
- Create: `crates/sayd/src/settings/schema.rs`
- Modify: `crates/sayd/src/settings/window.rs` (renderer replaces hand-written triples)
- Modify: `crates/sayd/src/settings/mod.rs`

**Interfaces:**
- Produces: `schema::{Row, Options, Group, Page, ROOT}` exactly as the spec declares them.

**This task changes no visible layout.** The schema declares the existing flat page with today's groups in today's order, and the window renders it. That isolates the one question worth isolating: does the renderer reproduce what was there? Every existing GTK test must pass **unmodified**.

- [ ] **Step 1: Write `schema.rs`** with the type declarations from the spec, and `ROOT` declaring today's surface: `Voice and speed`, `Engine`, `Text cleanup`, `Notifications`, `Reword`, then the three `Custom` list groups.

- [ ] **Step 2: Write the tree-integrity tests** — these need no GTK and must be plain `#[test]`s: every declared page is reachable from the root; no two rows in a group share a title; every `Choice` row's `get` returns a value its `Options` offers, or the `unknown` formatter covers it.

- [ ] **Step 3: Write the renderer in `window.rs`.** One `fn render_row(ui: &Ui, cfg: &Config, row: &Row) -> gtk::Widget` per variant, each doing the show/register/connect triple once. `Bool` uses `adw::SwitchRow`, `Int` the existing `Spin`, `Choice` the existing `Combo` (keep it — `AdwComboRow` has no unselected state and `Combo` is what covers that), `Text` the existing `bind_entry`/`commit_entry`.

- [ ] **Step 4: Delete the hand-written groups** — `voice_group`, `engine_group`, `cleanup_group`, `notification_group`, and the descriptor-able rows of `reword_group`. Keep the five `Custom` bodies: the Voice Test button, the Reword Test entry and result row, the allowlist, and the two suggestion groups.

- [ ] **Step 5: Run the GTK suite unmodified.** `scripts/headless.sh nix develop --command cargo test -p sayd --features reword settings::` — all nine scenarios pass with no edits. If a scenario needs editing, the renderer is not reproducing the layout; fix the renderer, not the test.

- [ ] **Step 6: All three suites, clippy, commit.**

```bash
git commit -am "refactor(settings): declare the config surface as data and render it"
```

---

### Task 5: The hierarchy

**Files:**
- Modify: `crates/sayd/src/settings/schema.rs` (the layout, as data)
- Modify: `crates/sayd/src/settings/window.rs` (subpage push, nav rows, summaries, test helpers)

**Interfaces:**
- Consumes: `schema::Page::{master, summary}` from Task 4.

- [ ] **Step 1: Add the subpage plumbing to the renderer.** A root nav row is a non-activatable-looking `adw::ActionRow` with `go-next-symbolic` that calls `adw::PreferencesWindow::push_subpage` with an `adw::NavigationPage` built from the `Page`. Build the pages eagerly and retain them, so `Ui::rows` redraw closures registered for their rows keep working whether or not the page is currently pushed.

- [ ] **Step 2: Declare the hierarchy** from the spec's tree: root keeps `Voice and speed` and `Engine`, gains `Text` (→ Cleanup, → Rewording) and `Sources` (→ say command, → Notifications). Masters: `Clean up text` → `cleanup.enabled`, `Reword text` → `reword.enabled`, `Speak notifications` → `notifications.enabled`. **No `say` master** — see the spec for why.

- [ ] **Step 3: Add the moved and new rows.** `Long-text guard` moves from `Engine` to the `say command` page. `API key variable` (`reword.api_key_env`) joins the Rewording page's Endpoint group. `Rewrite notifications automatically` binds `reword.notifications`.

- [ ] **Step 4: Write the summaries** and a test per page rendering a known config to a known string. These are load-bearing, not decoration: without them the root page is a menu rather than a status view.

- [ ] **Step 5: Teach the test helpers to push.** Add `fn push(ui: &Ui, page_title: &str)` and have the scenarios that touch reword rows push `Rewording` first. A subpage that has not been pushed is not under `ui.window`, so `find_row` cannot see it.

- [ ] **Step 6: Pin that an unpushed page's rows still redraw** — a config change while the Rewording page is closed must be visible when it is opened, without rebuilding the window.

- [ ] **Step 7: All three suites under `scripts/headless.sh`, clippy, commit.**

```bash
git commit -am "feat(settings): sub-pages for cleanup, rewording and each source"
```
