//! The GTK4/libadwaita settings window.
//!
//! Deliberately thin: every value it can produce is validated and written by
//! `super::model`, and every change is applied to the running engine by that
//! write's `ApplyConfig`. Nothing here decides anything. If you find
//! yourself writing a bound, a clamp, a default or a file write in this
//! file, it belongs in `model.rs`; the bounds the spin rows below offer are
//! named constants imported from there for exactly that reason.
//!
//! The one promise this layer makes on its own is that **no row ever shows a
//! value the config does not hold**. Two mechanisms carry it, because GTK
//! makes it harder than it sounds:
//!
//! - Every row registers a "draw yourself from this `Config`" closure with
//!   [`Ui::row`], and [`Ui::redraw`] runs the lot. It runs after every
//!   accepted edit (so a clamp or a repaired out-of-range value shows up),
//!   after every rejected one, after an asynchronous write failure, and when
//!   an already-open window is re-presented.
//! - A row that cannot express what the config holds says so, rather than
//!   quietly showing something else -- see [`Combo`] and [`Spin`].

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
    static WINDOW: RefCell<Option<Ui>> = const { RefCell::new(None) };
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

/// One row's "draw yourself from this config" closure; see [`Ui::rows`].
/// Boxed because the nine of them have nine different types, `Rc<RefCell<_>>`
/// because [`Ui`] is cloned into every handler and each of those may call
/// [`Ui::redraw`].
type Redraws = Rc<RefCell<Vec<Box<dyn Fn(&Config)>>>>;

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
    if let Some(ui) = WINDOW.with(|w| w.borrow().clone()) {
        // Re-reading here and not only in `build`: asking for the settings
        // again is exactly the moment a user expects to be looking at what
        // the config currently says, and this model deliberately does not
        // subscribe to the watcher (see `SettingsModel::refresh`), so a hand
        // edit that landed while the window sat open is invisible until
        // something asks.
        let cfg = ui.model.refresh();
        ui.redraw(&cfg);
        ui.window.present();
        return;
    }
    // Checked *before* `adw::init`, so a settings request that beats startup
    // costs nothing. `main`'s doc comment promises that a daemon which never
    // opens the window never touches GTK, and initialising it only to
    // discover there is no model to show would break that for the one case
    // where it is easiest to keep.
    let Some((model, engine)) = super::host() else {
        eprintln!("warning: settings requested before the daemon finished starting");
        return;
    };
    // Not at startup, and not once at first use either: `adw::init` calls
    // `gtk::init`, which is idempotent, and a daemon on a machine with no
    // display should pay nothing for a window it never opens.
    if let Err(e) = adw::init() {
        eprintln!("warning: could not start the settings UI: {e}");
        return;
    }

    let ui = build(model, engine);
    let model = ui.model.clone();
    ui.window.connect_close_request(move |_| {
        // Created on demand, destroyed on close, per the design: the daemon
        // spends most of its life with no window and should not hold one.
        WINDOW.with(|w| *w.borrow_mut() = None);
        // Drops the model's failure sender, which is what ends the drain
        // task in `build` -- and with it the last reference that task holds
        // to this window.
        model.stop_watching_write_failures();
        glib::Propagation::Proceed
    });
    ui.window.present();
    WINDOW.with(|w| *w.borrow_mut() = Some(ui));
}

/// Everything a row's change handler needs, cheap to clone into each of them.
#[derive(Clone)]
struct Ui {
    model: Arc<SettingsModel>,
    window: adw::PreferencesWindow,
    /// Raised while *this code* is setting a widget's value.
    ///
    /// GTK delivers `notify::`/`changed` synchronously and draws no
    /// distinction between a value the user chose and one the program wrote,
    /// so [`Ui::redraw`] -- putting the rows back to what the config holds
    /// -- re-enters the very handlers that triggered it. Left unguarded that
    /// is not merely a wasted write: a redraw after a *failed* edit would
    /// have each row re-edit and fail identically, forever, at a config
    /// write and a toast per iteration.
    ///
    /// A shared flag rather than `glib::signal_handler_block`: it covers all
    /// nine rows with one mechanism and does not require each handler to
    /// have been handed its own `SignalHandlerId` after the fact (which for
    /// a handler that must refer to itself means an `Rc<RefCell<Option<_>>>`
    /// per row). `Rc<Cell<_>>` and not an atomic because every one of these
    /// handlers runs on the glib main thread, one at a time.
    quiet: Rc<Cell<bool>>,
    /// One "draw yourself from this config" closure per row, in the order
    /// the rows were built.
    ///
    /// These deliberately capture only their own widget, never a [`Ui`]:
    /// the window already owns its handlers and the handlers own a `Ui`, so
    /// a closure here that captured one too would add a second cycle to keep
    /// track of for nothing.
    rows: Redraws,
}

impl Ui {
    /// Whether this handler fired because of [`Ui::redraw`] rather than
    /// because the user did something. See `quiet`.
    fn echo(&self) -> bool {
        self.quiet.get()
    }

    /// Run `f` with widget writes marked as ours rather than the user's.
    ///
    /// Saves and restores rather than clearing to `false`. Nesting is one
    /// deep today -- a redraw's own signals return at `echo()` before they
    /// can redraw again -- but nothing in the type system says so, and a
    /// second level that reset the flag on the way out would arm every
    /// remaining row of the outer redraw as if the user had touched it.
    fn quietly(&self, f: impl FnOnce()) {
        let was = self.quiet.replace(true);
        f();
        self.quiet.set(was);
    }

    fn toast(&self, message: &str) {
        self.window.add_toast(adw::Toast::new(message));
    }

    /// Register a row's redraw closure. Called once per row, at build time.
    fn row(&self, draw: impl Fn(&Config) + 'static) {
        self.rows.borrow_mut().push(Box::new(draw));
    }

    /// Put every row back to what `cfg` says.
    ///
    /// All rows and not just the one that changed, because a config is a
    /// single value: `validate` can clamp a field the user did not touch,
    /// an asynchronous write failure has no row to blame, and a re-present
    /// after a hand edit can move any of them.
    fn redraw(&self, cfg: &Config) {
        self.quietly(|| {
            for draw in self.rows.borrow().iter() {
                draw(cfg);
            }
        });
    }

    /// Push one change through the model, and redraw from whatever the model
    /// then holds.
    ///
    /// Redraws on success too, not only on failure. The accepted config is
    /// not always the requested one -- `validate` clamps speed -- and a row
    /// that had been reporting an unrepresentable value from the file has to
    /// stop saying so once the file no longer holds one.
    ///
    /// Only *validation* failures arrive here. The write itself happens on
    /// the model's own thread (see `SettingsModel::edit`); its failures come
    /// back through the drain task in [`build`].
    fn apply(&self, edit: impl FnOnce(&mut Config)) {
        match self.model.edit(edit) {
            Ok(cfg) => self.redraw(&cfg),
            Err(e) => {
                self.toast(&e);
                self.redraw(&self.model.current());
            }
        }
    }
}

fn build(model: Arc<SettingsModel>, engine: EngineHandle) -> Ui {
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
        window: window.clone(),
        quiet: Rc::new(Cell::new(false)),
        rows: Rc::new(RefCell::new(Vec::new())),
    };

    let page = adw::PreferencesPage::new();
    page.add(&voice_group(&ui, &cfg, engine));
    page.add(&engine_group(&ui, &cfg));
    page.add(&cleanup_group(&ui, &cfg));
    window.add(&page);

    // A write that fails does so long after the click that caused it, on the
    // model's writer thread, so it cannot be returned from `edit`. Draining
    // it here is what keeps "a config the daemon could not write" from being
    // logged and forgotten while the rows sit there showing it.
    //
    // The task ends when the sender is dropped, which `open`'s close handler
    // arranges -- that is also what releases the `Ui` clone below, and with
    // it this window.
    let failures = ui.model.watch_write_failures();
    let u = ui.clone();
    glib::spawn_future_local(async move {
        while let Ok(message) = failures.recv().await {
            u.toast(&message);
            // The model has already put itself back to what the file holds.
            u.redraw(&u.model.current());
        }
    });

    ui
}

/// A `ComboRow` that can show a configured value it does not offer as a
/// choice.
///
/// `AdwComboRow` has no "nothing is selected" state to put it in.
/// `set_selected(INVALID_LIST_POSITION)` looks like one and is not: the row
/// wraps its model in a `GtkSingleSelection` with `autoselect` on, which
/// refuses to unselect, so the call is a no-op and `selected()` stays `0`.
/// Measured against libadwaita 1.9.3 under a headless compositor, along with
/// the obvious repair -- handing `set_model` a `GtkSingleSelection` built
/// with `.autoselect(false)` -- which is *worse*: the row builds its own
/// selection over whatever list model it is given, so ours is ignored
/// entirely, `selected()` reads `0` and the row is deaf to that model's own
/// selection changes.
///
/// What the un-repaired version cost was not cosmetic. A configured voice
/// with no pack installed rendered as the first *installed* voice, selected,
/// beside a subtitle saying that voice was missing -- and clicking the entry
/// the row already showed emitted no signal and wrote nothing, so with a
/// single installed pack there was no way to fix it from the window at all.
/// The Model row was worse: a hand-edited `model = "int4"` displayed as
/// `fp32` while every *other* row toasted "'int4' is not a model this build
/// knows" (`edit` seeds from the file and `validate` rejects the whole
/// config), so nine rows failed over a field the user never touched and the
/// one control that could have repaired it was inert.
///
/// So the value gets a real entry of its own, at index 0, saying what it is.
/// Everything else shifts up by one, which is what `offset` is for.
#[derive(Clone)]
struct Combo {
    row: adw::ComboRow,
    list: gtk::StringList,
    /// Whether index 0 is the synthetic entry rather than a real choice.
    ///
    /// Created on demand and, once created, never removed: measured,
    /// `StringList::remove(0)` renumbers the selection and emits a
    /// `selected` notify of its own, which is a signal saying "the user
    /// picked something" that the user did not. Leaving a self-describing
    /// dead entry at the top of a dropdown until the window is reopened is
    /// the cheaper of the two.
    synthetic: Rc<Cell<bool>>,
    /// How to word that entry for this row.
    describe: fn(&str) -> String,
}

impl Combo {
    fn new<S: AsRef<str>>(title: &str, choices: &[S], describe: fn(&str) -> String) -> Combo {
        // `gtk::StringList::new` takes `&[&str]` and every list here is
        // built from owned `String`s, so it is filled by `append` instead.
        let list = gtk::StringList::new(&[]);
        for choice in choices {
            list.append(choice.as_ref());
        }
        let row = adw::ComboRow::builder().title(title).model(&list).build();
        Combo {
            row,
            list,
            synthetic: Rc::new(Cell::new(false)),
            describe,
        }
    }

    fn offset(&self) -> u32 {
        u32::from(self.synthetic.get())
    }

    /// Show `value`, which is at `position` among the choices if it is one
    /// of them at all.
    fn show(&self, value: &str, position: Option<usize>) {
        match position {
            Some(i) => {
                self.row.set_selected(self.offset() + i as u32);
                // The config no longer holds anything this row cannot
                // express, so it must stop claiming otherwise -- even if the
                // synthetic entry it is talking about is still in the list.
                self.row.set_subtitle("");
            }
            None => {
                let note = (self.describe)(value);
                if self.synthetic.get() {
                    self.list.splice(0, 1, &[note.as_str()]);
                } else {
                    self.list.splice(0, 0, &[note.as_str()]);
                    self.synthetic.set(true);
                }
                self.row.set_selected(0);
                self.row.set_subtitle(&note);
            }
        }
    }

    /// Which choice the row is showing, or `None` when it is showing the
    /// synthetic entry (or nothing at all, for an empty list, where
    /// `selected()` is `INVALID_LIST_POSITION` and the subtraction below
    /// lands far past the end).
    fn choice(&self) -> Option<usize> {
        self.row
            .selected()
            .checked_sub(self.offset())
            .map(|i| i as usize)
    }
}

/// A spin row that admits when the config holds a value it cannot show.
///
/// A `gtk::Adjustment` built with a value outside its own bounds clamps it
/// and emits nothing -- measured: `Adjustment::new(1.0, 100.0, …)` reads
/// back `100`. Nothing is written (rows are populated before their handlers
/// are connected), but the display lies, and a hand-edited `max_chars = 1`
/// that is rejecting every submission and parking the engine in
/// `State::Error` would show as a perfectly plausible `100`.
///
/// The bounds are not enforced here and no clamp is added -- `edit` seeds
/// from the file, so clamping would let an unrelated row silently rewrite a
/// hand-edited value (see the constants' doc comment in `model.rs`). What is
/// added is the row saying so, the way [`Combo`] does.
#[derive(Clone)]
struct Spin {
    row: adw::SpinRow,
    subtitle: &'static str,
    min: f64,
    max: f64,
    digits: u32,
}

impl Spin {
    fn new(
        title: &str,
        subtitle: &'static str,
        min: f64,
        max: f64,
        step: f64,
        digits: u32,
    ) -> Spin {
        let row = adw::SpinRow::builder()
            .title(title)
            .subtitle(subtitle)
            .adjustment(&gtk::Adjustment::new(
                min, min, max, step, step,
                // No page size: a spin button is not a scrollbar, and a
                // nonzero one would shrink the reachable range by exactly
                // that much.
                0.0,
            ))
            .digits(digits)
            .build();
        Spin {
            row,
            subtitle,
            min,
            max,
            digits,
        }
    }

    fn show(&self, value: f64) {
        self.row.set_value(value);
        if value < self.min || value > self.max {
            let digits = self.digits as usize;
            self.row.set_subtitle(&format!(
                "{} — the file holds {value:.digits$}, outside the range this offers, \
                 so the number shown is not it",
                self.subtitle
            ));
        } else {
            self.row.set_subtitle(self.subtitle);
        }
    }
}

fn voice_group(ui: &Ui, cfg: &Config, engine: EngineHandle) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Voice and speed")
        .build();

    // --- Voice ----------------------------------------------------------
    let voices: Vec<String> = ui.model.voices().to_vec();
    // `sayd` already warns about a configured voice with no pack at startup;
    // saying it again here is what keeps the row from silently showing some
    // other voice as if it were the configured one.
    let voice = Combo::new("Voice", &voices, |v| {
        format!("‘{v}’ — no voice pack installed")
    });
    // Populated *before* the handler is connected, which is what keeps
    // opening the window from writing the config nine times -- see `quiet`
    // for the other half of the same hazard.
    voice.show(&cfg.voice, voices.iter().position(|v| *v == cfg.voice));
    let c = voice.clone();
    let known = voices.clone();
    ui.row(move |cfg| c.show(&cfg.voice, known.iter().position(|v| *v == cfg.voice)));
    let u = ui.clone();
    let c = voice.clone();
    let known = voices.clone();
    voice.row.connect_selected_notify(move |_| {
        if u.echo() {
            return;
        }
        match c.choice().and_then(|i| known.get(i).cloned()) {
            Some(name) => u.apply(|cfg| cfg.voice = name),
            // The synthetic entry, or an empty models directory: there is no
            // voice to write. Redrawing rather than merely returning is what
            // stops the row sitting on a selection nothing agrees with.
            None => u.redraw(&u.model.current()),
        }
    });
    group.add(&voice.row);

    // --- Speed ----------------------------------------------------------
    let speed = Spin::new(
        "Speed",
        "Playback rate for every utterance",
        SPEED_MIN as f64,
        SPEED_MAX as f64,
        SPEED_STEP,
        2,
    );
    speed.show(cfg.speed as f64);
    let s = speed.clone();
    ui.row(move |cfg| s.show(cfg.speed as f64));
    let u = ui.clone();
    speed.row.connect_value_notify(move |row| {
        if u.echo() {
            return;
        }
        let value = row.value() as f32;
        u.apply(|c| c.speed = value);
    });
    group.add(&speed.row);

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
    // Not registered with `Ui::row`: this is the one control in the window
    // that is not a view of the config, so there is nothing to redraw it
    // from -- and clobbering what the user typed on every edit elsewhere
    // would be its own bug.
    let u = ui.clone();
    let e = engine.clone();
    let entry = test.clone();
    speak.connect_clicked(move |_| audition(&u, &e, &entry));
    let u = ui.clone();
    // Pressing Enter in the field is the same action as pressing the button;
    // a test row you have to reach for the mouse to use is a test row nobody
    // uses twice.
    test.connect_entry_activated(move |row| audition(&u, &engine, row));
    group.add(&test);

    group
}

/// Speak the Test row's text through the engine.
///
/// Writes nothing: this is the one control in the window that is not a view
/// of the config, so it goes straight to `EngineHandle` rather than through
/// `Ui::apply` -- and it is the only place an `EngineHandle` is needed,
/// which is why one is passed here rather than carried in [`Ui`].
fn audition(ui: &Ui, engine: &EngineHandle, entry: &adw::EntryRow) {
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
    // that caused it rather than only in the daemon's log. Bounded at
    // `SUBMIT_REPLY_TIMEOUT`, and the only blocking call left on this
    // thread.
    if let Err(e) = engine.submit(text, opts) {
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
    let model_row = Combo::new("Model", &labels, |m| {
        format!("‘{m}’ — not a model this build knows")
    });
    let position = |name: &str| MODELS.iter().position(|(n, _)| *n == name);
    model_row.show(&cfg.model, position(&cfg.model));
    let c = model_row.clone();
    ui.row(move |cfg| c.show(&cfg.model, position(&cfg.model)));
    let u = ui.clone();
    let c = model_row.clone();
    model_row.row.connect_selected_notify(move |_| {
        if u.echo() {
            return;
        }
        match c.choice().and_then(|i| MODELS.get(i)) {
            Some((name, _)) => {
                let name = (*name).to_string();
                u.apply(|c| c.model = name);
            }
            None => u.redraw(&u.model.current()),
        }
    });
    group.add(&model_row.row);

    // --- Threads --------------------------------------------------------
    let threads = Spin::new(
        "Threads",
        "ONNX Runtime intra-op threads; measured peak at 8",
        THREADS_MIN,
        THREADS_MAX,
        THREADS_STEP,
        0,
    );
    threads.show(cfg.threads as f64);
    let s = threads.clone();
    ui.row(move |cfg| s.show(cfg.threads as f64));
    let u = ui.clone();
    threads.row.connect_value_notify(move |row| {
        if u.echo() {
            return;
        }
        let value = row.value() as usize;
        u.apply(|c| c.threads = value);
    });
    group.add(&threads.row);

    // --- Idle unload ----------------------------------------------------
    let idle = Spin::new(
        "Idle unload",
        "Seconds of silence before the ~1.27 GB session is dropped; 0 never unloads",
        IDLE_UNLOAD_MIN,
        IDLE_UNLOAD_MAX,
        IDLE_UNLOAD_STEP,
        0,
    );
    idle.show(cfg.idle_unload_secs as f64);
    let s = idle.clone();
    ui.row(move |cfg| s.show(cfg.idle_unload_secs as f64));
    let u = ui.clone();
    idle.row.connect_value_notify(move |row| {
        if u.echo() {
            return;
        }
        let value = row.value() as u64;
        u.apply(|c| c.idle_unload_secs = value);
    });
    group.add(&idle.row);

    // --- Long-text guard ------------------------------------------------
    let max_chars = Spin::new(
        "Long-text guard",
        "Refuse submissions longer than this many characters",
        MAX_CHARS_MIN,
        MAX_CHARS_MAX,
        MAX_CHARS_STEP,
        0,
    );
    max_chars.show(cfg.max_chars as f64);
    let s = max_chars.clone();
    ui.row(move |cfg| s.show(cfg.max_chars as f64));
    let u = ui.clone();
    max_chars.row.connect_value_notify(move |row| {
        if u.echo() {
            return;
        }
        let value = row.value() as usize;
        u.apply(|c| c.max_chars = value);
    });
    group.add(&max_chars.row);

    group
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
        let r = row.clone();
        ui.row(move |cfg| r.set_active(get(&cfg.cleanup)));
        let u = ui.clone();
        row.connect_active_notify(move |row| {
            if u.echo() {
                return;
            }
            let on = row.is_active();
            u.apply(|c| set(&mut c.cleanup, on));
        });
        group.add(&row);
    }

    let labels: Vec<&str> = URL_POLICIES.iter().map(|(_, label)| *label).collect();
    // Unreachable in practice -- `UrlPolicy` is an enum, so the table below
    // covers every value it can take -- but `Combo` has no way to know that,
    // and a wording that reads as nonsense would be worse than one that
    // never appears.
    let urls = Combo::new("URLs", &labels, |p| {
        format!("‘{p}’ — not a URL setting this build knows")
    });
    let position = |p: UrlPolicy| URL_POLICIES.iter().position(|(q, _)| *q == p);
    let describe = |p: UrlPolicy| format!("{p:?}").to_lowercase();
    urls.show(&describe(cfg.cleanup.urls), position(cfg.cleanup.urls));
    let c = urls.clone();
    ui.row(move |cfg| c.show(&describe(cfg.cleanup.urls), position(cfg.cleanup.urls)));
    let u = ui.clone();
    let c = urls.clone();
    urls.row.connect_selected_notify(move |_| {
        if u.echo() {
            return;
        }
        match c.choice().and_then(|i| URL_POLICIES.get(i)) {
            Some((policy, _)) => {
                let policy = *policy;
                u.apply(|c| c.cleanup.urls = policy);
            }
            None => u.redraw(&u.model.current()),
        }
    });
    group.add(&urls.row);

    group
}
