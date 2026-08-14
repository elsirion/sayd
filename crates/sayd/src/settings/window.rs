//! The GTK4/libadwaita settings window.
//!
//! Deliberately thin: every value it can produce is validated and written by
//! `super::model`, and every change is applied to the running engine by that
//! write's `ApplyConfig`. Nothing here decides anything, because nothing here
//! can be tested -- there is no display in CI or in an agent environment. If
//! you find yourself writing a bound, a clamp, a default or a file write in
//! this file, it belongs in `model.rs`; the bounds the spin rows below offer
//! are named constants imported from there for exactly that reason.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk4 as gtk;
use sayd_core::config::{CleanupConfig, Config, UrlPolicy};
use sayd_core::engine::SayOpts;
use sayd_core::handle::EngineHandle;
use sayd_core::queue::{Policy, Source as QueueSource};

use super::model::{
    SettingsModel, IDLE_UNLOAD_MAX, IDLE_UNLOAD_MIN, IDLE_UNLOAD_STEP, MAX_CHARS_MAX,
    MAX_CHARS_MIN, MAX_CHARS_STEP, MODELS, SPEED_MAX, SPEED_MIN, SPEED_STEP, THREADS_MAX,
    THREADS_MIN, THREADS_STEP,
};

thread_local! {
    /// The live window, if one is open. Re-presenting an open window is what
    /// a second click should do; building a second one would leave two views
    /// of the same config disagreeing.
    ///
    /// Thread-local rather than a static because `adw::PreferencesWindow` is
    /// not `Send`, and because there is exactly one thread -- the glib main
    /// thread -- that is ever allowed to touch it.
    static WINDOW: RefCell<Option<adw::PreferencesWindow>> = const { RefCell::new(None) };
}

/// The URL row's options, in the order the dropdown offers them.
///
/// A table rather than three `if`s so the index the widget reports maps back
/// to a policy by position, with no second list to keep in step.
const URL_POLICIES: [(UrlPolicy, &str); 3] = [
    (UrlPolicy::Link, "Say “link”"),
    (UrlPolicy::Domain, "Say the domain"),
    (UrlPolicy::Keep, "Read the whole URL"),
];

/// Read and write one cleanup flag. Function pointers rather than a macro or
/// six hand-written rows: `CleanupConfig`'s fields are plain `bool`s with no
/// common accessor, and a pair of one-line closures per field is the least
/// machinery that still lets the six rows share one handler.
type CleanupGet = fn(&CleanupConfig) -> bool;
type CleanupSet = fn(&mut CleanupConfig, bool);

/// The Text cleanup group, in the order spec §8 lists the transforms.
const CLEANUP_SWITCHES: [(&str, &str, CleanupGet, CleanupSet); 5] = [
    (
        "Collapse whitespace",
        "Runs of spaces and blank lines become a single space",
        |c| c.collapse_whitespace,
        |c, v| c.collapse_whitespace = v,
    ),
    (
        "Rejoin hyphenation",
        "Reunite words a line break split with a hyphen",
        |c| c.rejoin_hyphenation,
        |c, v| c.rejoin_hyphenation = v,
    ),
    (
        "Strip Markdown",
        "Drop emphasis, heading and link syntax instead of reading it out",
        |c| c.strip_markdown,
        |c, v| c.strip_markdown = v,
    ),
    (
        "Drop code blocks",
        "Skip fenced and indented code rather than speaking it",
        |c| c.drop_code_blocks,
        |c, v| c.drop_code_blocks = v,
    ),
    (
        "Spell out acronyms",
        "Read TTS as T-T-S rather than as a word",
        |c| c.spell_acronyms,
        |c, v| c.spell_acronyms = v,
    ),
];

/// Open the settings window, or present the one already open.
///
/// Must run on the main thread; the glib loop in `main` guarantees that.
pub fn open() {
    if let Some(w) = WINDOW.with(|w| w.borrow().clone()) {
        w.present();
        return;
    }
    // Not at startup, and not once at first use either: `adw::init` calls
    // `gtk::init`, which is idempotent, and a daemon on a machine with no
    // display should pay nothing for a window it never opens (see `main`'s
    // doc comment).
    if let Err(e) = adw::init() {
        eprintln!("warning: could not start the settings UI: {e}");
        return;
    }
    let Some((model, engine)) = super::host() else {
        eprintln!("warning: settings requested before the daemon finished starting");
        return;
    };

    let window = build(model, engine);
    window.connect_close_request(|_| {
        // Created on demand, destroyed on close, per the design: the daemon
        // spends most of its life with no window and should not hold one.
        // Clearing this is also what lets the *next* open re-read the config
        // rather than re-present a window whose values have gone stale.
        WINDOW.with(|w| *w.borrow_mut() = None);
        glib::Propagation::Proceed
    });
    window.present();
    WINDOW.with(|w| *w.borrow_mut() = Some(window));
}

/// Everything a row's change handler needs, cheap to clone into each of them.
#[derive(Clone)]
struct Ui {
    model: Arc<SettingsModel>,
    engine: EngineHandle,
    window: adw::PreferencesWindow,
    /// Raised while *this code* is setting a widget's value.
    ///
    /// GTK delivers `notify::`/`changed` synchronously and draws no
    /// distinction between a value the user chose and one the program wrote,
    /// so [`Ui::apply`]'s revert -- putting a rejected row back to what the
    /// file holds -- re-enters the very handler that just failed. Left
    /// unguarded that is not merely a wasted write: the reason an `edit`
    /// fails is usually the *file* (unwritable, a read-only home), so the
    /// revert's own `edit` fails identically and reverts again, forever, at
    /// a config write and a toast per iteration.
    ///
    /// A shared flag rather than `glib::signal_handler_block`: it covers all
    /// nine rows with one mechanism and does not require each handler to
    /// have been handed its own `SignalHandlerId` after the fact (which for
    /// a handler that must refer to itself means an `Rc<RefCell<Option<_>>>`
    /// per row). `Rc<Cell<_>>` and not an atomic because every one of these
    /// handlers runs on the glib main thread, one at a time.
    quiet: Rc<Cell<bool>>,
}

impl Ui {
    /// Whether this handler fired because of [`Ui::apply`]'s own revert
    /// rather than because the user did something. See `quiet`.
    fn echo(&self) -> bool {
        self.quiet.get()
    }

    fn toast(&self, message: &str) {
        self.window.add_toast(adw::Toast::new(message));
    }

    /// Push one change through the model.
    ///
    /// On failure, say so and put the widget back to what the file actually
    /// holds. The failure M3's review asked for somewhere to surface: a
    /// config the daemon could not write is not something to log and forget,
    /// because the row would then sit there showing a value the file does
    /// not have -- and `edit` guarantees a rejected write leaves the model
    /// untouched, so `current()` is the value to put back.
    fn apply(&self, edit: impl FnOnce(&mut Config), revert: impl FnOnce(&Config)) {
        if let Err(e) = self.model.edit(edit) {
            self.toast(&e);
            let current = self.model.current();
            self.quiet.set(true);
            revert(&current);
            self.quiet.set(false);
        }
    }
}

fn build(model: Arc<SettingsModel>, engine: EngineHandle) -> adw::PreferencesWindow {
    // Draw from the file, not from whatever this model last wrote itself: a
    // hand edit the watcher picked up while no window was open would
    // otherwise be invisible here, and the first row the user touched would
    // appear to revert it. See `SettingsModel::refresh`.
    let cfg = model.refresh();

    let window = adw::PreferencesWindow::new();
    window.set_title(Some("sayd Settings"));
    window.set_default_size(520, 700);

    let ui = Ui {
        model,
        engine,
        window: window.clone(),
        quiet: Rc::new(Cell::new(false)),
    };

    let page = adw::PreferencesPage::new();
    page.add(&voice_group(&ui, &cfg));
    page.add(&engine_group(&ui, &cfg));
    page.add(&cleanup_group(&ui, &cfg));
    window.add(&page);
    window
}

/// Turn a list of strings into a `ComboRow`'s model.
///
/// `gtk::StringList::new` takes `&[&str]`, and every list here is built from
/// owned `String`s, so this is the one place that bridges the two.
fn string_list<S: AsRef<str>>(items: &[S]) -> gtk::StringList {
    let list = gtk::StringList::new(&[]);
    for item in items {
        list.append(item.as_ref());
    }
    list
}

/// Select `position`, or select nothing if the current value is not in the
/// list at all.
///
/// "Nothing" is the honest answer: picking the first entry instead would
/// show a value the config file does not hold, and *writing* it would change
/// a setting the user never touched.
fn select(row: &adw::ComboRow, position: Option<usize>) {
    row.set_selected(position.map_or(gtk::INVALID_LIST_POSITION, |i| i as u32));
}

fn voice_group(ui: &Ui, cfg: &Config) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Voice and speed")
        .build();

    // --- Voice ----------------------------------------------------------
    let voices: Vec<String> = ui.model.voices().to_vec();
    let voice = adw::ComboRow::builder()
        .title("Voice")
        .model(&string_list(&voices))
        .build();
    // Populated *before* the handler is connected, which is what keeps
    // opening the window from writing the config nine times -- see `quiet`
    // for the other half of the same hazard.
    let selected = voices.iter().position(|v| *v == cfg.voice);
    select(&voice, selected);
    if selected.is_none() {
        // The configured voice has no pack installed. `sayd` already warns
        // about this at startup; saying it again here is what makes an
        // empty-looking dropdown legible rather than alarming.
        voice.set_subtitle(&format!("‘{}’ has no voice pack installed", cfg.voice));
    }
    let u = ui.clone();
    let known = voices.clone();
    voice.connect_selected_notify(move |row| {
        if u.echo() {
            return;
        }
        // Covers both `INVALID_LIST_POSITION` (nothing selected, because the
        // configured voice has no pack) and an empty models directory: there
        // is no voice to write, so write nothing.
        let Some(name) = known.get(row.selected() as usize).cloned() else {
            return;
        };
        u.apply(
            |c| c.voice = name,
            |c| select(row, known.iter().position(|v| *v == c.voice)),
        );
    });
    group.add(&voice);

    // --- Speed ----------------------------------------------------------
    let speed = adw::SpinRow::builder()
        .title("Speed")
        .subtitle("Playback rate for every utterance")
        .adjustment(&gtk::Adjustment::new(
            cfg.speed as f64,
            SPEED_MIN as f64,
            SPEED_MAX as f64,
            SPEED_STEP,
            SPEED_STEP,
            // No page size: a spin button is not a scrollbar, and a nonzero
            // one would shrink the reachable range by exactly that much.
            0.0,
        ))
        .digits(2)
        .build();
    let u = ui.clone();
    speed.connect_value_notify(move |row| {
        if u.echo() {
            return;
        }
        let value = row.value() as f32;
        u.apply(|c| c.speed = value, |c| row.set_value(c.speed as f64));
    });
    group.add(&speed);

    // --- Test -----------------------------------------------------------
    let test = adw::EntryRow::builder()
        .title("Test")
        .text("The quick brown fox jumps over the lazy dog.")
        .build();
    let speak = gtk::Button::builder()
        .label("Speak")
        .valign(gtk::Align::Center)
        .build();
    test.add_suffix(&speak);
    let u = ui.clone();
    let entry = test.clone();
    speak.connect_clicked(move |_| audition(&u, &entry));
    let u = ui.clone();
    // Pressing Enter in the field is the same action as pressing the button;
    // a test row you have to reach for the mouse to use is a test row nobody
    // uses twice.
    test.connect_entry_activated(move |row| audition(&u, row));
    group.add(&test);

    group
}

/// Speak the Test row's text through the engine.
///
/// Writes nothing: this is the one control in the window that is not a view
/// of the config, so it goes straight to `EngineHandle` rather than through
/// `Ui::apply`.
fn audition(ui: &Ui, entry: &adw::EntryRow) {
    let text = entry.text().to_string();
    if text.trim().is_empty() {
        return;
    }
    let opts = SayOpts {
        // `Replace` so repeated presses audition the current settings
        // instead of queueing up behind each other. Set explicitly, which is
        // why `Source::DBus` here is not a claim about where this came from:
        // `Source` only picks a *default* policy, and this overrides it.
        policy: Some(Policy::Replace),
        source: QueueSource::DBus,
        ..SayOpts::default()
    };
    // `submit` answers synchronously so a rejection (a voice pack that is
    // not installed, text over `max_chars`) can be shown next to the button
    // that caused it rather than only in the daemon's log.
    if let Err(e) = ui.engine.submit(text, opts) {
        ui.toast(&e);
    }
}

fn engine_group(ui: &Ui, cfg: &Config) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Engine")
        .description(
            "Changes take effect on the next utterance; switching model reloads the session",
        )
        .build();

    // --- Model ----------------------------------------------------------
    // The measured trade-off goes in the item text itself rather than in the
    // row's subtitle: it is what the user needs while the dropdown is *open*
    // and they are choosing between the three, not afterwards.
    let labels: Vec<String> = MODELS
        .iter()
        .map(|(name, note)| format!("{name} — {note}"))
        .collect();
    let model_row = adw::ComboRow::builder()
        .title("Model")
        .model(&string_list(&labels))
        .build();
    select(
        &model_row,
        MODELS.iter().position(|(name, _)| *name == cfg.model),
    );
    let u = ui.clone();
    model_row.connect_selected_notify(move |row| {
        if u.echo() {
            return;
        }
        // A hand-edited config can hold a model string this build does not
        // know, in which case nothing is selected and there is nothing to
        // write. `SettingsModel::edit` would reject it anyway.
        let Some((name, _)) = MODELS.get(row.selected() as usize) else {
            return;
        };
        u.apply(
            |c| c.model = (*name).to_string(),
            |c| select(row, MODELS.iter().position(|(n, _)| *n == c.model)),
        );
    });
    group.add(&model_row);

    // --- Threads --------------------------------------------------------
    let threads = spin(
        "Threads",
        "ONNX Runtime intra-op threads; measured peak at 8",
        cfg.threads as f64,
        THREADS_MIN,
        THREADS_MAX,
        THREADS_STEP,
    );
    let u = ui.clone();
    threads.connect_value_notify(move |row| {
        if u.echo() {
            return;
        }
        let value = row.value() as usize;
        u.apply(|c| c.threads = value, |c| row.set_value(c.threads as f64));
    });
    group.add(&threads);

    // --- Idle unload ----------------------------------------------------
    let idle = spin(
        "Idle unload",
        "Seconds of silence before the ~1.27 GB session is dropped; 0 never unloads",
        cfg.idle_unload_secs as f64,
        IDLE_UNLOAD_MIN,
        IDLE_UNLOAD_MAX,
        IDLE_UNLOAD_STEP,
    );
    let u = ui.clone();
    idle.connect_value_notify(move |row| {
        if u.echo() {
            return;
        }
        let value = row.value() as u64;
        u.apply(
            |c| c.idle_unload_secs = value,
            |c| row.set_value(c.idle_unload_secs as f64),
        );
    });
    group.add(&idle);

    // --- Long-text guard ------------------------------------------------
    let max_chars = spin(
        "Long-text guard",
        "Refuse submissions longer than this many characters",
        cfg.max_chars as f64,
        MAX_CHARS_MIN,
        MAX_CHARS_MAX,
        MAX_CHARS_STEP,
    );
    let u = ui.clone();
    max_chars.connect_value_notify(move |row| {
        if u.echo() {
            return;
        }
        let value = row.value() as usize;
        u.apply(
            |c| c.max_chars = value,
            |c| row.set_value(c.max_chars as f64),
        );
    });
    group.add(&max_chars);

    group
}

/// A whole-number spin row. `digits(0)` because every caller here edits a
/// `usize` or a `u64`, and a spinner offering `600.00` seconds invites the
/// user to type a fraction that the cast below would silently truncate.
fn spin(title: &str, subtitle: &str, value: f64, min: f64, max: f64, step: f64) -> adw::SpinRow {
    adw::SpinRow::builder()
        .title(title)
        .subtitle(subtitle)
        .adjustment(&gtk::Adjustment::new(value, min, max, step, step, 0.0))
        .digits(0)
        .build()
}

fn cleanup_group(ui: &Ui, cfg: &Config) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Text cleanup")
        .description("Applied to every submission before it is spoken")
        .build();

    for (title, subtitle, get, set) in CLEANUP_SWITCHES {
        let row = adw::SwitchRow::builder()
            .title(title)
            .subtitle(subtitle)
            .active(get(&cfg.cleanup))
            .build();
        let u = ui.clone();
        row.connect_active_notify(move |row| {
            if u.echo() {
                return;
            }
            let on = row.is_active();
            u.apply(
                |c| set(&mut c.cleanup, on),
                |c| row.set_active(get(&c.cleanup)),
            );
        });
        group.add(&row);
    }

    let labels: Vec<&str> = URL_POLICIES.iter().map(|(_, label)| *label).collect();
    let urls = adw::ComboRow::builder()
        .title("URLs")
        .model(&string_list(&labels))
        .build();
    select(
        &urls,
        URL_POLICIES
            .iter()
            .position(|(p, _)| *p == cfg.cleanup.urls),
    );
    let u = ui.clone();
    urls.connect_selected_notify(move |row| {
        if u.echo() {
            return;
        }
        let Some((policy, _)) = URL_POLICIES.get(row.selected() as usize) else {
            return;
        };
        let policy = *policy;
        u.apply(
            |c| c.cleanup.urls = policy,
            |c| {
                select(
                    row,
                    URL_POLICIES.iter().position(|(p, _)| *p == c.cleanup.urls),
                )
            },
        );
    });
    group.add(&urls);

    group
}
