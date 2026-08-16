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
//!
//! The notification allowlist and the suggestion groups beneath it are the
//! parts whose *number* of rows is not fixed, so their redraw closures
//! rebuild them rather than setting a value; see [`allowlist_group`] and
//! [`suggestions_group`].
//!
//! The second promise is a lifetime one, and it is not free: the window is
//! built on demand and **freed** on close, so a daemon that opened the
//! settings once is back to carrying no GTK resources at all. What that
//! costs is that no handler may hold the state it is handed -- see [`Ui`]
//! and [`WeakUi`], which is where the whole arrangement is explained.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk4 as gtk;
use sayd_core::config::{CleanupConfig, Config, NotificationConfig, UrlPolicy};
use sayd_core::engine::SayOpts;
use sayd_core::handle::EngineHandle;
use sayd_core::queue::{Policy, Source as QueueSource};

use super::model::{
    allow_add, allow_contains, allow_remove, icon_file_size_within_limit, icon_pixels_within_limit,
    reword_key_row_applies, IconSource, SettingsModel, Suggestion, SuggestionKind, COOLDOWN_MAX,
    COOLDOWN_MIN, COOLDOWN_STEP, ENDPOINT_PRESETS, IDLE_UNLOAD_MAX, IDLE_UNLOAD_MIN,
    IDLE_UNLOAD_STEP, MAX_CHARS_MAX, MAX_CHARS_MIN, MAX_CHARS_STEP, MODELS, REWORD_MAX_CHARS_MAX,
    REWORD_MAX_CHARS_MIN, REWORD_MAX_CHARS_STEP, REWORD_TEST_DEFAULT, REWORD_TIMEOUT_MAX,
    REWORD_TIMEOUT_MIN, REWORD_TIMEOUT_STEP, SPEED_MAX, SPEED_MIN, SPEED_MODES, SPEED_STEP,
    THREADS_MAX, THREADS_MIN, THREADS_STEP,
};
use crate::notify::seen;

thread_local! {
    /// The live window, if one is open. Re-presenting an open window is what
    /// a second click should do; building a second one would leave two views
    /// of the same config disagreeing.
    ///
    /// Thread-local rather than a static because `adw::PreferencesWindow` is
    /// not `Send`, and because there is exactly one thread -- the glib main
    /// thread -- that is ever allowed to touch it.
    ///
    /// This is the *only* strong [`Ui`] in the process: handlers hold a
    /// [`WeakUi`]. Clearing it is therefore what frees the window, not
    /// merely what forgets it -- see [`Ui`].
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

/// One row's "draw yourself from this config" closure; see [`UiState::rows`].
/// Boxed because they have as many different types as there are rows,
/// `RefCell` because a handler reached through a shared [`Ui`] may call
/// [`Ui::redraw`].
///
/// The [`Ui`] is handed *in* rather than captured, which matters for exactly
/// one of these closures: the allowlist's, which builds fresh rows with
/// fresh handlers every time it runs and so needs a `Ui` to give them. A
/// closure that captured one would close a reference cycle through this very
/// list -- list holds closure holds `Ui` holds list -- which nothing would
/// ever break. Passing it as an argument makes that unrepresentable rather
/// than merely discouraged.
type Redraws = RefCell<Vec<Box<dyn Fn(&Ui, &Config)>>>;

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

/// The same arrangement as [`CleanupGet`]/[`CleanupSet`], for the same
/// reason: `NotificationConfig`'s three flags are plain `bool`s with no
/// common accessor, so one handler serves all three only if each row can say
/// which field it is.
type NotifyGet = fn(&NotificationConfig) -> bool;
type NotifySet = fn(&mut NotificationConfig, bool);

/// The Notifications group's switches, in the order spec §6 lists them.
///
/// `enabled` leads because the other two are refinements of it -- and
/// because it is the one that starts and stops the bus monitor, rather than
/// changing the wording of an announcement that was going to happen anyway.
const NOTIFICATION_SWITCHES: [(&str, &str, NotifyGet, NotifySet); 3] = [
    (
        "Speak notifications",
        "Announce desktop notifications from the applications listed below; \
         they are still shown as usual",
        |c| c.enabled,
        |c, v| c.enabled = v,
    ),
    (
        "Say the application name",
        "Announce “Signal: Ada: dinner?” rather than the summary on its own",
        |c| c.speak_app_name,
        |c, v| c.speak_app_name = v,
    ),
    (
        "Say the body",
        "Read the body after the summary; many applications only restate the summary there",
        |c| c.speak_body,
        |c, v| c.speak_body = v,
    ),
];

/// What a suggestion whose icon cannot be drawn shows instead.
///
/// What lands here is every row for which no candidate drew: an application
/// that sent no icon in any of the three fields it could have (`notify-send`
/// sends none), a curated or seen icon *name* the user's theme does not
/// have, a path that is not there any more, and an image too large to decode
/// on this thread (see [`image_from_file`]). `gtk::Image` would draw some of
/// those as its own broken-image glyph, which is the wrong thing to say: the
/// row is not broken, the icon is simply unknown, and a generic application
/// icon says exactly that. Checked against the theme rather than assumed --
/// see [`suggestion_icon`], which is also the only place in this file that
/// can ask, since the answer depends on the display.
///
/// The *symbolic* variant, and that was worth looking at: the full-colour
/// `application-x-executable` is a blue gem, and a list where nine of
/// thirteen rows carry the same saturated blue gem beside Element's green
/// and Firefox's orange reads as thirteen applications whose logos happen to
/// look alike, rather than as four that have an icon and nine that do not.
/// The symbolic version is drawn in the label's own colour, so it recedes
/// exactly as far as "we do not know this one" should. Compared side by side
/// under a headless compositor against Adwaita 49; both names resolve there,
/// and have for a decade.
const FALLBACK_ICON: &str = "application-x-executable-symbolic";

/// How large a suggestion's icon is drawn, in pixels.
///
/// 32 rather than the 16 a `gtk::Image` prefix defaults to: these are
/// application icons, and at 16px the ones that carry a real logo (Signal's,
/// Firefox's) are indistinguishable from each other and from the fallback,
/// which is the entire reason for showing an icon rather than a name alone.
/// It is also what libadwaita's own `AdwActionRow` list of applications uses.
const SUGGESTION_ICON_PX: i32 = 32;

/// How often an open window asks whether a new application has notified.
///
/// The same cadence `notify::monitor`'s own `DUE_INTERVAL` runs at, and for
/// a comparable reason: it is the shortest interval at which "nothing is
/// happening" costs nothing worth measuring -- one relaxed atomic load --
/// and it is fast enough that a user who triggers a notification to see it
/// appear does not conclude the feature is broken and close the window.
/// What it does *not* do is redraw once a second; see
/// [`Ui::redraw_suggestions_if_changed`].
const SEEN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The Reword group's result row, by `GtkWidget:name`.
///
/// It is the one row in this window with no stable title: its title *is* what
/// it reports, which is a provider's answer or a failure sentence. Every
/// structural property it has -- a non-activatable `AdwActionRow` with
/// unlimited title lines -- is shared with rows built elsewhere on the page,
/// so identity is the only thing left to find it by.
const RESULT_ROW_NAME: &str = "reword-test-result";

/// The two suggestion groups, in the order the page shows them: what has
/// actually notified, then the built-in guesses.
///
/// Two groups rather than one "Suggestions" heading, because the difference
/// between the two kinds is a *sentence* rather than a word. A seen entry is
/// a fact -- that application sent that name, on this machine, this run, and
/// adding it is guaranteed to match. A curated entry is a guess at what an
/// application passes as its `app_name`, and a wrong guess silently never
/// matches anything (see `CURATED`'s doc comment in `model.rs`), which is
/// precisely the failure a user cannot diagnose from the row itself. Saying
/// so once, in a group description, costs one line; saying it on every
/// curated row costs thirteen subtitles that all read the same, and saying it
/// nowhere would present a guess as a fact.
///
/// It also makes the "hide when there is nothing to suggest" rule fall out
/// per kind rather than for the pair: a user who has allowed every curated
/// application still gets the seen group, and a fresh daemon that has watched
/// nothing notify still gets the curated one.
const SUGGESTION_GROUPS: [(SuggestionKind, &str, &str); 2] = [
    (
        SuggestionKind::Seen,
        "Seen notifying",
        "Applications that have notified since sayd started, most recent first, \
         with the icon each one sent. These names are exactly what the application \
         passes, so adding one is certain to match it.",
    ),
    (
        SuggestionKind::Curated,
        "Common applications",
        "A short built-in list, offered before anything has notified. Each name is \
         sayd's best guess at what the application passes, not a name it has seen: \
         if adding one turns out to announce nothing, the application uses some \
         other name, and the daemon's log has the real one.",
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
        //
        // This line is the whole of that promise. `WINDOW` holds the only
        // strong `Ui` in the process (every handler holds a `WeakUi`), so
        // clearing it here drops the state, the window reference it carries
        // and every redraw closure's hold on a widget -- which is what lets
        // the `gtk_window_destroy` that follows this handler actually
        // finalize the tree rather than merely hide it. See `Ui`.
        WINDOW.with(|w| *w.borrow_mut() = None);
        // Drops the model's failure sender, which is what ends the drain
        // task in `build`.
        model.stop_watching_write_failures();
        glib::Propagation::Proceed
    });
    ui.window.present();
    WINDOW.with(|w| *w.borrow_mut() = Some(ui));
}

/// Everything a row's change handler needs, cheap to clone into each of them.
///
/// Shared behind an `Rc` so that a handler can hold a [`WeakUi`] rather than
/// one of these. That indirection is what makes closing the settings window
/// actually free it. Measured under a headless compositor against the
/// version where handlers held a `Ui` directly: `close()` plus half a second
/// of main loop freed *nothing at all* -- 21 live `Ui`s, the
/// `AdwPreferencesWindow` itself and 533 of 533 widgets in its tree survived,
/// once per opening, for the life of a daemon that is supposed to carry no
/// GTK resources between openings. The same measurement after this change
/// reads 0, 0 and 0 of 533.
///
/// Two reference cycles were doing it, and neither is one `gtk_window_destroy`
/// can break:
///
/// - `Ui` holds the window, the window owns its widget tree, and every row
///   in that tree holds a handler that held a `Ui`. Dispose does not help:
///   the handlers keep the window's own refcount above zero, so it is never
///   finalized and its widgets' closures are never dropped.
/// - `rows` holds each row's widget -- that is what a redraw closure draws
///   *to* -- and each of those widgets holds a handler that held a `Ui` that
///   holds `rows`. GTK is not even involved in that one; it is a plain `Rc`
///   cycle, and clearing `rows` on close does not fix the first one (also
///   measured).
///
/// A weak handler reference cuts both, and puts the whole window's lifetime
/// in one place: `WINDOW` holds the only strong `Ui` in the process, so
/// clearing it on close is what drops the window reference, the redraw
/// closures, and the widget references those carry.
#[derive(Clone)]
struct Ui(Rc<UiState>);

/// A handler's reference to the [`Ui`] it belongs to.
///
/// Weak, because a strong one is a cycle GTK stores and cannot break -- see
/// [`Ui`]. Upgrading is therefore allowed to fail, and doing nothing is the
/// right answer when it does: the window has already been closed and its
/// state released, so there is no row left to redraw and no toast left to
/// show. It is not a *missed* edit either -- the config the user would have
/// been editing is only reachable through a window that no longer exists.
///
/// That is not a theoretical branch. Measured, the one caller that reaches
/// it in ordinary use is the write-failure drain task in [`build`]: the
/// writer thread posts a failed write to the channel, and the task is not
/// polled until the next turn of the main loop, which can be after the user
/// has closed the window. Before this, that path toasted a destroyed window.
/// Row handlers themselves were *not* observed to fire during teardown, but
/// they are on the same footing and cost nothing to make safe.
#[derive(Clone)]
struct WeakUi(Weak<UiState>);

/// The state a [`Ui`] shares. Never held by a handler; see [`WeakUi`].
struct UiState {
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
    /// per row). A `Cell` and not an atomic because every one of these
    /// handlers runs on the glib main thread, one at a time.
    quiet: Cell<bool>,
    /// One "draw yourself from this config" closure per row, in the order
    /// the rows were built.
    ///
    /// These capture their own widget strongly, which is safe only because
    /// nothing a widget stores holds this state back: handlers hold a
    /// [`WeakUi`]. They deliberately do not capture a [`Ui`] -- one is
    /// passed to them instead, for the reason spelled out on [`Redraws`].
    rows: Redraws,
    /// The two suggestion groups' redraw closures, kept apart from `rows`.
    ///
    /// Separate because they are the only rows that can need redrawing
    /// while the *config* has not changed at all: an application notifying
    /// while the window sits open changes what they should show and nothing
    /// else (IMPORTANT 7). [`Ui::redraw`] runs both lists; the seen-registry
    /// poll in [`build`] runs only this one, so noticing a new application
    /// cannot clobber the allowlist entry field the user is halfway through
    /// typing into, or any other row.
    suggestion_rows: Redraws,
    /// What those closures draw: computed once per redraw rather than once
    /// per group.
    ///
    /// IMPORTANT 6: each group used to call `SettingsModel::suggestions`
    /// itself, so a `seen::record` landing between the two calls could show
    /// an application in neither -- absent from the seen group (not yet
    /// recorded when it ran) and deduplicated out of the curated one (seen
    /// by the time *it* ran). One call, partitioned by the groups, cannot
    /// disagree with itself.
    suggestions: RefCell<Vec<Suggestion>>,
    /// The `notify::seen` generation `suggestions` was computed from. See
    /// [`Ui::refresh_suggestions`].
    seen_generation: Cell<u64>,
}

/// So a `Ui` reads as the struct it stands in for. Nothing outside this
/// module sees either type.
impl std::ops::Deref for Ui {
    type Target = UiState;

    fn deref(&self) -> &UiState {
        &self.0
    }
}

impl WeakUi {
    /// Run `f` against the live window, or do nothing at all if there is no
    /// longer one. See [`WeakUi`] for why doing nothing is right.
    fn with(&self, f: impl FnOnce(&Ui)) {
        if let Some(state) = self.0.upgrade() {
            f(&Ui(state));
        }
    }

    /// Run `f` for a change the *user* made.
    ///
    /// [`WeakUi::with`] plus the `quiet` guard, in one place rather than
    /// once per handler: every `notify::` handler in this file owes both
    /// checks, and the cost of forgetting the second one is an infinite loop
    /// of failed edits (see [`UiState::quiet`]).
    fn on_user_change(&self, f: impl FnOnce(&Ui)) {
        self.with(|ui| {
            if ui.echo() {
                return;
            }
            f(ui);
        });
    }
}

impl Ui {
    fn downgrade(&self) -> WeakUi {
        WeakUi(Rc::downgrade(&self.0))
    }

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
    fn row(&self, draw: impl Fn(&Ui, &Config) + 'static) {
        self.rows.borrow_mut().push(Box::new(draw));
    }

    /// Register a suggestion group's redraw closure. See
    /// [`UiState::suggestion_rows`] for why these are a separate list.
    fn suggestion_row(&self, draw: impl Fn(&Ui, &Config) + 'static) {
        self.suggestion_rows.borrow_mut().push(Box::new(draw));
    }

    /// Put every row back to what `cfg` says.
    ///
    /// All rows and not just the one that changed, because a config is a
    /// single value: `validate` can clamp a field the user did not touch,
    /// an asynchronous write failure has no row to blame, and a re-present
    /// after a hand edit can move any of them.
    fn redraw(&self, cfg: &Config) {
        // Before the rows are drawn, not inside one of them: both
        // suggestion groups draw from this one list. See
        // [`UiState::suggestions`].
        self.refresh_suggestions();
        self.quietly(|| {
            // Borrowed, not `borrow_mut`: the allowlist's closure rebuilds
            // widgets while this iteration is live, and a row registering
            // itself mid-redraw (which nothing does -- `row` is only called
            // at build time) would otherwise be a panic rather than a
            // question.
            for draw in self.rows.borrow().iter() {
                draw(self, cfg);
            }
            for draw in self.suggestion_rows.borrow().iter() {
                draw(self, cfg);
            }
        });
    }

    /// Recompute the suggestion cache, and say whether it changed.
    ///
    /// The `notify::seen` generation is recorded whether or not anything
    /// changed: it is a "have I looked at this state yet" marker for the
    /// poll in [`build`], not a description of what the cache holds.
    fn refresh_suggestions(&self) -> bool {
        self.seen_generation.set(seen::generation());
        let fresh = self.model.suggestions();
        if *self.suggestions.borrow() == fresh {
            return false;
        }
        *self.suggestions.borrow_mut() = fresh;
        true
    }

    /// Redraw the two suggestion groups, and nothing else, if what they
    /// should show has changed.
    ///
    /// IMPORTANT 7: "Seen notifying" never updated while the window was
    /// open, because every other `redraw` call site is driven by a config
    /// change and an application notifying is not one. That made the
    /// README's own walkthrough work only in the order it happened to
    /// prescribe -- leave Settings open, trigger a notification, watch
    /// nothing appear -- which is precisely the discovery loop these
    /// suggestions exist to close.
    ///
    /// Two guards against that becoming a redraw storm, since this runs on
    /// a timer: the generation is an atomic load that answers "has anything
    /// been recorded at all" without taking the registry's lock, and the
    /// cache comparison answers "and would it look any different" without
    /// touching a widget. A chatty allowlisted application passes neither
    /// (see `notify::seen::GENERATION`), so the common case costs one
    /// atomic load a second and nothing else.
    fn redraw_suggestions_if_changed(&self, cfg: &Config) {
        if self.seen_generation.get() == seen::generation() {
            return;
        }
        if !self.refresh_suggestions() {
            return;
        }
        self.quietly(|| {
            for draw in self.suggestion_rows.borrow().iter() {
                draw(self, cfg);
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

    let ui = Ui(Rc::new(UiState {
        model,
        window: window.clone(),
        quiet: Cell::new(false),
        rows: RefCell::new(Vec::new()),
        suggestion_rows: RefCell::new(Vec::new()),
        suggestions: RefCell::new(Vec::new()),
        seen_generation: Cell::new(0),
    }));
    // Seeded before any group is built, because `suggestions_group` draws
    // itself once on the way past. Every later refresh goes through
    // `Ui::redraw` or the poll below.
    ui.refresh_suggestions();

    let page = adw::PreferencesPage::new();
    page.add(&voice_group(&ui, &cfg, engine.clone()));
    page.add(&engine_group(&ui, &cfg));
    page.add(&cleanup_group(&ui, &cfg));
    page.add(&notification_group(&ui, &cfg));
    // The Reword group needs one too: its result row speaks what came back.
    page.add(&reword_group(&ui, &cfg, engine));
    // The allowlist and its two suggestion groups belong together at the
    // bottom of the page, so the Reword group goes above them.
    page.add(&allowlist_group(&ui, &cfg));
    for (kind, title, description) in SUGGESTION_GROUPS {
        page.add(&suggestions_group(&ui, &cfg, kind, title, description));
    }
    window.add(&page);

    // A write that fails does so long after the click that caused it, on the
    // model's writer thread, so it cannot be returned from `edit`. Draining
    // it here is what keeps "a config the daemon could not write" from being
    // logged and forgotten while the rows sit there showing it.
    //
    // The task ends when the sender is dropped, which `open`'s close handler
    // arranges. It holds a `WeakUi` rather than a `Ui` for the same reason
    // every handler does -- a strong one here would keep the window alive
    // for as long as the task took to notice the channel had closed, and
    // that noticing needs a turn of the very main loop the close is
    // happening on.
    // IMPORTANT 7: the one thing this window shows that no config change
    // announces itself through. `notify::seen` is written from a tokio task
    // and read here; there is no signal to connect to and nothing to make
    // one out of that would not be a channel across the two runtimes, for a
    // list of at most `MAX_SEEN` names.
    //
    // The poll itself is an atomic load, and it does nothing further unless
    // the answer changed -- see `Ui::redraw_suggestions_if_changed`, which
    // is also where the guard against a chatty application rebuilding
    // identical rows lives. `timeout_add_local` and not a tokio timer
    // because everything it touches is a widget, and widgets belong to this
    // thread.
    //
    // Ends with the window: a `WeakUi` that no longer upgrades is the
    // window having been closed, and `ControlFlow::Break` is what takes the
    // source off the main loop rather than leaving it ticking for the life
    // of a daemon that no longer has a window at all.
    let u = ui.downgrade();
    glib::timeout_add_local(SEEN_POLL_INTERVAL, move || {
        let Some(state) = u.0.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let ui = Ui(state);
        ui.redraw_suggestions_if_changed(&ui.model.current());
        glib::ControlFlow::Continue
    });

    let failures = ui.model.watch_write_failures();
    let u = ui.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(message) = failures.recv().await {
            u.with(|ui| {
                ui.toast(&message);
                // The model has already put itself back to what the file
                // holds.
                ui.redraw(&ui.model.current());
            });
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
        // `use_markup(false)` for the reason the allowlist rows set it, one
        // layer further in: the *subtitle* this row shows is built by
        // `describe` out of a value from the config file or the models
        // directory, and `AdwPreferencesRow:use-markup` governs the subtitle
        // label as well as the title -- measured, a row left on the default
        // renders both of them **blank** for a voice named `Ada & Co`, which
        // is the one string the user needs to read in order to fix it.
        //
        // Belt and braces today rather than a fix: measured against
        // libadwaita 1.9.3, `AdwComboRow` is the one row type that already
        // overrides the property to `false` (`ActionRow`, `SpinRow`,
        // `SwitchRow`, `EntryRow` and `PreferencesRow` itself all default to
        // `true`). Nothing documents that asymmetry as API, and a row whose
        // safety depends on which subclass it happens to be is a row that
        // breaks silently when it is changed.
        let row = adw::ComboRow::builder()
            .title(title)
            .use_markup(false)
            .model(&list)
            .build();
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

    /// Which choice `row` is showing, or `None` when it is showing the
    /// synthetic entry (or nothing at all, for an empty list, where
    /// `selected()` is `INVALID_LIST_POSITION` and the subtraction below
    /// lands far past the end).
    ///
    /// An associated function over the row the signal handed back, rather
    /// than a method reading `self.row`. A handler then needs only the
    /// `synthetic` flag, and never a clone of the very row it is attached
    /// to -- which would be a widget holding a handler holding that same
    /// widget, a cycle nothing in GTK breaks and the one shape [`Ui`] cannot
    /// protect against on its own.
    fn choice(row: &adw::ComboRow, synthetic: &Cell<bool>) -> Option<usize> {
        row.selected()
            .checked_sub(u32::from(synthetic.get()))
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
    ui.row(move |_, cfg| c.show(&cfg.voice, known.iter().position(|v| *v == cfg.voice)));
    let u = ui.downgrade();
    let synthetic = voice.synthetic.clone();
    let known = voices.clone();
    voice.row.connect_selected_notify(move |row| {
        u.on_user_change(|u| {
            match Combo::choice(row, &synthetic).and_then(|i| known.get(i).cloned()) {
                Some(name) => u.apply(|cfg| cfg.voice = name),
                // The synthetic entry, or an empty models directory: there
                // is no voice to write. Redrawing rather than merely
                // returning is what stops the row sitting on a selection
                // nothing agrees with.
                None => u.redraw(&u.model.current()),
            }
        });
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
    ui.row(move |_, cfg| s.show(cfg.speed as f64));
    let u = ui.downgrade();
    speed.row.connect_value_notify(move |row| {
        u.on_user_change(|u| {
            let value = row.value() as f32;
            u.apply(|c| c.speed = value);
        });
    });
    group.add(&speed.row);

    // --- Speed mode -------------------------------------------------------
    // Same reasoning as the Model row below: the measured trade-off goes in
    // the item text itself, visible while the dropdown is open and the user
    // is choosing, not tucked into a subtitle they have already looked past.
    let speed_mode_labels: Vec<String> = SPEED_MODES
        .iter()
        .map(|(name, note)| format!("{name} — {note}"))
        .collect();
    let speed_mode_row = Combo::new("Speed mode", &speed_mode_labels, |m| {
        format!("‘{m}’ — not a speed mode this build knows")
    });
    let speed_mode_position = |name: &str| SPEED_MODES.iter().position(|(n, _)| *n == name);
    speed_mode_row.show(&cfg.speed_mode, speed_mode_position(&cfg.speed_mode));
    let c = speed_mode_row.clone();
    ui.row(move |_, cfg| c.show(&cfg.speed_mode, speed_mode_position(&cfg.speed_mode)));
    let u = ui.downgrade();
    let synthetic = speed_mode_row.synthetic.clone();
    speed_mode_row.row.connect_selected_notify(move |row| {
        u.on_user_change(|u| {
            match Combo::choice(row, &synthetic).and_then(|i| SPEED_MODES.get(i)) {
                Some((name, _)) => {
                    let name = (*name).to_string();
                    u.apply(|c| c.speed_mode = name);
                }
                None => u.redraw(&u.model.current()),
            }
        });
    });
    group.add(&speed_mode_row.row);

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
    let u = ui.downgrade();
    let e = engine.clone();
    // Weak, because `speak` is a suffix *of* `test`: a strong clone here
    // would be the row holding a button holding the row, which outlives the
    // window that used to contain it. The same shape `Combo::choice` avoids,
    // one widget further apart.
    let field = test.downgrade();
    speak.connect_clicked(move |_| {
        let Some(field) = field.upgrade() else { return };
        u.with(|ui| audition(ui, &e, &field.text()));
    });
    let u = ui.downgrade();
    // Pressing Enter in the field is the same action as pressing the button;
    // a test row you have to reach for the mouse to use is a test row nobody
    // uses twice.
    test.connect_entry_activated(move |row| u.with(|ui| audition(ui, &engine, &row.text())));
    group.add(&test);

    group
}

/// Speak `text` through the engine.
///
/// Writes nothing: the callers are the voice group's Test row and the Reword
/// group's result row, neither of which is a view of the config, so this goes
/// straight to `EngineHandle` rather than through `Ui::apply` -- and they are
/// the only place an `EngineHandle` is needed, which is why one is passed
/// here rather than carried in [`Ui`].
///
/// Takes the string rather than the widget it came from: the Reword group's
/// result row is an `AdwActionRow` with a title, not an entry, and a helper
/// that insisted on an `AdwEntryRow` would have to be written twice.
fn audition(ui: &Ui, engine: &EngineHandle, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let text = text.to_string();
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
    ui.row(move |_, cfg| c.show(&cfg.model, position(&cfg.model)));
    let u = ui.downgrade();
    let synthetic = model_row.synthetic.clone();
    model_row.row.connect_selected_notify(move |row| {
        u.on_user_change(
            |u| match Combo::choice(row, &synthetic).and_then(|i| MODELS.get(i)) {
                Some((name, _)) => {
                    let name = (*name).to_string();
                    u.apply(|c| c.model = name);
                }
                None => u.redraw(&u.model.current()),
            },
        );
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
    ui.row(move |_, cfg| s.show(cfg.threads as f64));
    let u = ui.downgrade();
    threads.row.connect_value_notify(move |row| {
        u.on_user_change(|u| {
            let value = row.value() as usize;
            u.apply(|c| c.threads = value);
        });
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
    ui.row(move |_, cfg| s.show(cfg.idle_unload_secs as f64));
    let u = ui.downgrade();
    idle.row.connect_value_notify(move |row| {
        u.on_user_change(|u| {
            let value = row.value() as u64;
            u.apply(|c| c.idle_unload_secs = value);
        });
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
    ui.row(move |_, cfg| s.show(cfg.max_chars as f64));
    let u = ui.downgrade();
    max_chars.row.connect_value_notify(move |row| {
        u.on_user_change(|u| {
            let value = row.value() as usize;
            u.apply(|c| c.max_chars = value);
        });
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
        ui.row(move |_, cfg| r.set_active(get(&cfg.cleanup)));
        let u = ui.downgrade();
        row.connect_active_notify(move |row| {
            u.on_user_change(|u| {
                let on = row.is_active();
                u.apply(|c| set(&mut c.cleanup, on));
            });
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
    ui.row(move |_, cfg| c.show(&describe(cfg.cleanup.urls), position(cfg.cleanup.urls)));
    let u = ui.downgrade();
    let synthetic = urls.synthetic.clone();
    urls.row.connect_selected_notify(move |row| {
        u.on_user_change(|u| {
            match Combo::choice(row, &synthetic).and_then(|i| URL_POLICIES.get(i)) {
                Some((policy, _)) => {
                    let policy = *policy;
                    u.apply(|c| c.cleanup.urls = policy);
                }
                None => u.redraw(&u.model.current()),
            }
        });
    });
    group.add(&urls.row);

    group
}

fn notification_group(ui: &Ui, cfg: &Config) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Notifications")
        .description("Takes effect at once: turning this on starts watching the session bus")
        .build();

    // The other rows here, and the allowlist below, stay *sensitive* when
    // "Speak notifications" is off. Deliberately, and not an oversight:
    //
    // - Those values are not meaningless while announcements are off, they
    //   are merely not in effect. The file keeps every one of them, and they
    //   apply the instant the switch goes back on. A dimmed row says
    //   "unset", which would be a lie about what the config holds -- the one
    //   thing this file promises never to do.
    // - Curating the list with announcements off is a real way to use this:
    //   turn it off for an hour, tidy up, turn it back on.
    // - It would be a rule, and rules do not live in this layer. There is no
    //   config field that says these four depend on that one, so `model.rs`
    //   has nothing to hang it on and the window would be deciding something
    //   on its own.
    //
    // Cheap to revisit if it reads wrong on hardware; nothing else depends
    // on it.

    for (title, subtitle, get, set) in NOTIFICATION_SWITCHES {
        let row = adw::SwitchRow::builder()
            .title(title)
            .subtitle(subtitle)
            .active(get(&cfg.notifications))
            .build();
        let r = row.clone();
        ui.row(move |_, cfg| r.set_active(get(&cfg.notifications)));
        let u = ui.downgrade();
        row.connect_active_notify(move |row| {
            u.on_user_change(|u| {
                let on = row.is_active();
                u.apply(|c| set(&mut c.notifications, on));
            });
        });
        group.add(&row);
    }

    // The subtitle has to spend its words on `0`, which is the one value here
    // that does not mean what a "seconds between X" row usually means: it is
    // not "no wait", it turns rate limiting off entirely, so every single
    // notification from an allowed application is spoken (`Limiter::decide`'s
    // `cooldown_secs == 0` arm, and the test that pins it). Left unsaid, `0`
    // reads like the *least* chatty setting rather than the most.
    let cooldown = Spin::new(
        "Cooldown",
        "Seconds before the same application is announced again; 0 speaks every notification",
        COOLDOWN_MIN,
        COOLDOWN_MAX,
        COOLDOWN_STEP,
        0,
    );
    cooldown.show(cfg.notifications.cooldown_secs as f64);
    let s = cooldown.clone();
    ui.row(move |_, cfg| s.show(cfg.notifications.cooldown_secs as f64));
    let u = ui.downgrade();
    cooldown.row.connect_value_notify(move |row| {
        u.on_user_change(|u| {
            let value = row.value() as u64;
            u.apply(|c| c.notifications.cooldown_secs = value);
        });
    });
    group.add(&cooldown.row);

    group
}

/// A two-way text row bound to one `String` field of the config.
///
/// The commit rule is the reason this is a helper rather than three copies:
/// **on the apply button and on focus-out, never per keystroke.** A base URL
/// is invalid for almost the whole time you are typing it, and the model's
/// `WRITE_DEBOUNCE` was sized for spin-button auto-repeat, not for typing --
/// committing per keystroke would hand the validator a dozen half-URLs and
/// write most of them to the file.
///
/// `AdwEntryRow`'s `apply` signal covers the button and Enter; it is not
/// emitted on focus-out, so the focus half is a `GtkEventControllerFocus` on
/// the row (its `leave` fires when focus leaves the row's whole subtree,
/// which is what "tabbed out of this field" means -- the widget that
/// actually holds the focus is the `GtkText` inside). The controller holds a
/// *weak* reference to the row it is attached to: a strong one would be the
/// row holding a controller holding the row, the cycle `Combo::choice`
/// exists to avoid one widget further apart.
///
/// The row is populated by the caller *before* this is called, in the order
/// every other row here uses -- see `quiet`.
///
/// The redraw closure below only rewrites the row when its text disagrees
/// with the config (see the comment on that check), which reads as if that
/// inequality were the one thing standing between a user mid-keystroke and
/// a redraw overwriting them. It is not doing that much work in practice.
/// Every route to a redraw that a mouse or keyboard press could take goes
/// through *this row losing focus first* -- clicking another row, Tab, the
/// preset menu, all move focus off this entry before anything downstream
/// can call `apply` -- and losing focus is exactly what commits this row's
/// own text (see the `EventControllerFocus` below). So by the time such a
/// redraw arrives, there is nothing left half-typed here to lose: either
/// the commit matched what the row already showed, or it changed the config
/// and this redraw is the one putting the row's text back in sync with it.
/// The redraws that *do* arrive with focus still in this row -- and so are
/// the ones the inequality check is actually protecting -- are the
/// write-failure drain (an async task, not routed through any widget) and
/// `open()`'s re-present (before the user has typed anything at all, so
/// there is nothing at stake either way).
fn bind_entry(
    ui: &Ui,
    row: &impl IsA<adw::EntryRow>,
    get: fn(&Config) -> String,
    set: fn(&mut Config, String),
) {
    // Upcast once: a `PasswordEntryRow` is an `EntryRow`, and everything
    // below is the parent class's.
    let row: adw::EntryRow = row.as_ref().clone();

    let r = row.clone();
    ui.row(move |_, cfg| {
        let want = get(cfg);
        // Only when it differs. A redraw fires for every accepted edit
        // anywhere in the window, and `set_text` on an entry the user is
        // half-way through moves the cursor to the end and loses what came
        // after it.
        if r.text() != want {
            r.set_text(&want);
        }
    });

    let u = ui.downgrade();
    row.connect_apply(move |row| commit_entry(&u, row, get, set));

    let focus = gtk::EventControllerFocus::new();
    let u = ui.downgrade();
    let weak = row.downgrade();
    focus.connect_leave(move |_| {
        let Some(row) = weak.upgrade() else { return };
        commit_entry(&u, &row, get, set);
    });
    row.add_controller(focus);
}

/// Write what a bound entry row holds, unless the config already holds it.
///
/// The "unless" is what keeps tabbing through the window from writing the
/// file once per row: `leave` fires for every field the focus passes
/// through, whether or not anything was typed into it. It also makes the
/// apply button idempotent, and -- because a rejected edit redraws the row
/// back to what the model holds -- keeps the focus-out that follows a
/// refused apply from asking for the same rejection twice.
fn commit_entry(
    u: &WeakUi,
    row: &adw::EntryRow,
    get: fn(&Config) -> String,
    set: fn(&mut Config, String),
) {
    let value = row.text().to_string();
    u.on_user_change(|u| {
        if get(&u.model.current()) == value {
            return;
        }
        u.apply(move |cfg| set(cfg, value));
    });
}

/// A plain string, made safe to hand to `AdwPreferencesGroup:description`.
///
/// That property is the one string in this window that `use_markup(false)`
/// cannot protect: it is parsed as Pango markup and `AdwPreferencesGroup`
/// exposes no `use-markup` of its own to turn that off (checked against
/// libadwaita 1.9). Every other description here is a fixed sentence, but
/// the Reword group's is built from the configured `base_url` -- so an
/// endpoint of `http://ada&co.lan:11434/v1` made GTK refuse the whole
/// string: *"Failed to set text ... Entity name 'co.lan. No API key is set'
/// is not known"*, measured under a headless compositor.
///
/// What that costs is worse than the blank row `use_markup` guards against,
/// because the label keeps whatever it had: the group goes on describing the
/// endpoint the user came *from*, and the one line telling them where their
/// text is being sent is quietly a lie. Escaping is the fix rather than
/// stripping, because the text is meant to be read verbatim -- the markup
/// parser turns `&amp;` back into `&` for display, so what is rendered is
/// exactly the string `model.rs` produced.
fn group_description(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}

/// Where a rewrite is sent, what it is sent as, and what it may cost.
///
/// The group with the only two-way text entries in this window; see
/// [`bind_entry`] for when one of them commits. `use_markup(false)` on every
/// row, not only the ones showing a config string: the endpoint and the
/// model name are user-supplied, `AdwPreferencesRow:use-markup` defaults to
/// `true` and governs the subtitle as well as the title, and a row left on
/// the default renders **both blank** for a value containing `&` -- which is
/// exactly the character a URL with a query string carries.
fn reword_group(ui: &Ui, cfg: &Config, engine: EngineHandle) -> adw::PreferencesGroup {
    // The description names the destination host and says where the key is
    // coming from -- a user who exports SAYD_REWORD_API_KEY and then sees no
    // key in the window would otherwise conclude the feature is
    // unconfigured. Built in `model.rs`, like every other string that
    // depends on what the config says.
    let group = adw::PreferencesGroup::builder()
        .title("Reword")
        .description(group_description(&ui.model.reword_description_now()))
        .build();
    // Redrawn like any row: the destination and the key sentence are what
    // the Endpoint and API key rows below change. Escaped on both paths --
    // see `group_description`, and note that the failure it prevents is a
    // *stale* description rather than a blank one.
    let g = group.clone();
    ui.row(move |ui, cfg| {
        g.set_description(Some(&group_description(
            &ui.model.reword_description_for(cfg),
        )))
    });

    // --- Rewrite notifications -------------------------------------------
    let enabled = adw::SwitchRow::builder()
        .title("Rewrite notifications")
        .subtitle("Send each announcement to the endpoint below and speak what comes back")
        .use_markup(false)
        .active(cfg.reword.enabled)
        .build();
    let r = enabled.clone();
    ui.row(move |_, cfg| r.set_active(cfg.reword.enabled));
    let u = ui.downgrade();
    enabled.connect_active_notify(move |row| {
        let on = row.is_active();
        u.on_user_change(|u| u.apply(|c| c.reword.enabled = on));
    });
    group.add(&enabled);

    // --- Endpoint ---------------------------------------------------------
    let endpoint = adw::EntryRow::builder()
        .title("Endpoint")
        .use_markup(false)
        .show_apply_button(true)
        .text(&*cfg.reword.base_url)
        .build();
    // A plain popover of buttons rather than a `GMenu`: this daemon has no
    // `GtkApplication` and therefore no action group to hang menu items on,
    // and inventing one for six URLs would be more machinery than the
    // feature.
    let presets = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let popover = gtk::Popover::builder().child(&presets).build();
    let menu = gtk::MenuButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("Known endpoints")
        .valign(gtk::Align::Center)
        .popover(&popover)
        .build();
    menu.add_css_class("flat");
    for (name, url, _takes_key) in ENDPOINT_PRESETS {
        let item = gtk::Button::builder()
            .label(format!("{name} — {url}"))
            .css_classes(["flat"])
            .build();
        let u = ui.downgrade();
        let pop = popover.downgrade();
        // Weak, like every other widget a handler here refers to: this
        // button is inside the popover the MenuButton owns, which is a
        // suffix of the row.
        let field = endpoint.downgrade();
        item.connect_clicked(move |_| {
            if let Some(pop) = pop.upgrade() {
                pop.popdown();
            }
            let Some(field) = field.upgrade() else { return };
            // `on_user_change` rather than the bare `with` every other
            // widget-owning closure in this file could get away with: a
            // button click is a user change like any other row's, and nothing
            // here makes it safe to skip the `echo()` guard except that a
            // redraw never clicks a button. That safety margin is free to
            // keep and costly to lose track of the one time this handler is
            // copied somewhere it is not so safe.
            u.on_user_change(|ui| {
                // The visible text *and* the config. A preset that filled
                // the field and left it waiting for the apply button would
                // look applied and not be.
                ui.quietly(|| field.set_text(url));
                ui.apply(|c| c.reword.base_url = url.to_string());
            });
        });
        presets.append(&item);
    }
    endpoint.add_suffix(&menu);
    bind_entry(
        ui,
        &endpoint,
        |cfg| cfg.reword.base_url.clone(),
        |cfg, v| cfg.reword.base_url = v,
    );
    group.add(&endpoint);

    // --- Model ------------------------------------------------------------
    let model_row = adw::EntryRow::builder()
        .title("Model")
        .use_markup(false)
        .show_apply_button(true)
        .text(&*cfg.reword.model)
        .build();
    bind_entry(
        ui,
        &model_row,
        |cfg| cfg.reword.model.clone(),
        |cfg, v| cfg.reword.model = v,
    );
    group.add(&model_row);

    // --- API key ----------------------------------------------------------
    // Shown only where a key can be used, which is `reword_key_row_applies`'s
    // rule and not this layer's: a credential field in front of a server
    // that takes no credential invites putting a secret into a file the
    // window rewrites wholesale, for nothing. It never hides a key the file
    // already holds -- this row is the only way to read or clear one -- and
    // the group description above says where the key is coming from in the
    // case where the row is not there to say it.
    //
    // The visibility follows the *committed* endpoint, not what is being
    // typed into it (see `bind_entry`), so it changes at most once per
    // deliberate change of endpoint rather than flickering under the cursor.
    let key = adw::PasswordEntryRow::builder()
        .title("API key")
        .use_markup(false)
        .show_apply_button(true)
        .text(&*cfg.reword.api_key)
        .build();
    // A tooltip and not a subtitle: `AdwEntryRow` has no subtitle at all
    // (`AdwPreferencesRow` does not define one -- checked against
    // libadwaita 1.9), so the sentence has to go somewhere that exists.
    key.set_tooltip_text(Some(
        "Sent to the endpoint as a bearer token. It is stored in config.toml, \
         which this window rewrites in full; a key in the environment variable \
         named in the description above is used instead and is never written here.",
    ));
    key.set_visible(reword_key_row_applies(&cfg.reword));
    let r = key.clone();
    ui.row(move |_, cfg| r.set_visible(reword_key_row_applies(&cfg.reword)));
    bind_entry(
        ui,
        &key,
        |cfg| cfg.reword.api_key.clone(),
        |cfg, v| cfg.reword.api_key = v,
    );
    group.add(&key);

    // --- Deadline ---------------------------------------------------------
    let deadline = Spin::new(
        "Deadline",
        "Milliseconds a rewrite may take before the original is spoken instead",
        REWORD_TIMEOUT_MIN,
        REWORD_TIMEOUT_MAX,
        REWORD_TIMEOUT_STEP,
        0,
    );
    // `Spin` is shared with four other groups, whose rows show nothing but
    // numbers; this group's rule is that every row in it is non-markup, so
    // it is set here rather than by changing what every spin row does.
    deadline.row.set_use_markup(false);
    deadline.show(cfg.reword.timeout_ms as f64);
    let s = deadline.clone();
    ui.row(move |_, cfg| s.show(cfg.reword.timeout_ms as f64));
    let u = ui.downgrade();
    deadline.row.connect_value_notify(move |row| {
        let value = row.value() as u64;
        u.on_user_change(|u| u.apply(|c| c.reword.timeout_ms = value));
    });
    group.add(&deadline.row);

    // --- Longest text to rewrite -----------------------------------------
    let ceiling = Spin::new(
        "Longest text to rewrite",
        "Characters; anything longer is spoken as written",
        REWORD_MAX_CHARS_MIN,
        REWORD_MAX_CHARS_MAX,
        REWORD_MAX_CHARS_STEP,
        0,
    );
    ceiling.row.set_use_markup(false);
    ceiling.show(cfg.reword.max_chars as f64);
    let s = ceiling.clone();
    ui.row(move |_, cfg| s.show(cfg.reword.max_chars as f64));
    let u = ui.downgrade();
    ceiling.row.connect_value_notify(move |row| {
        let value = row.value() as usize;
        u.on_user_change(|u| u.apply(|c| c.reword.max_chars = value));
    });
    group.add(&ceiling.row);

    // --- Test -------------------------------------------------------------
    // Every failure in the design's §8 degrades to "speak the original",
    // which is correct and indistinguishable from the feature being switched
    // off. A typo in the endpoint, a stale key, a model name the provider
    // does not have: all of them produce a daemon that behaves exactly as it
    // did before, with nothing in this window to look at. This row is where
    // the difference becomes visible -- and it is the only place anybody ever
    // learns what their provider actually costs, because nothing else in this
    // project measures end-to-end provider latency.
    let test = adw::EntryRow::builder()
        .title("Test")
        .use_markup(false)
        .text(REWORD_TEST_DEFAULT)
        .build();
    let run = gtk::Button::builder()
        .label("Test")
        .valign(gtk::Align::Center)
        .tooltip_text("Send this text to the endpoint above")
        .build();
    test.add_suffix(&run);
    // Not registered with `Ui::row`: this is scratch, not a view of the
    // config, and clobbering what the user typed on every edit elsewhere
    // would be its own bug. The default comes back when the window is
    // rebuilt, which is what "restored whenever the window opens" means --
    // the window is built on demand and freed on close.
    group.add(&test);

    // --- The result -------------------------------------------------------
    // `use_markup(false)` because both labels are provider-supplied: the
    // title is the model's own answer and the subtitle can carry a transport
    // error or a message the provider wrote. `title_lines(0)` and
    // `subtitle_lines(0)` so a rewritten sentence and a long failure reason
    // *wrap* rather than being ellipsised -- the text is the point of the
    // row, and a row that cuts it off has reported the outcome without
    // reporting the thing the outcome is about.
    //
    // Not registered with `Ui::row`, for a stronger reason than the Test row
    // above: it is the one thing in this window that reports something other
    // than the config's own state, so a redraw would have nothing to draw it
    // from and would simply erase the answer the user is reading.
    //
    // A row rather than a toast because a toast cannot be re-read, and the
    // whole activity here is compare, edit an endpoint, press again.
    let result = adw::ActionRow::builder()
        .use_markup(false)
        .title_lines(0)
        .subtitle_lines(0)
        .activatable(false)
        .visible(false)
        // Named so the test can find it by identity rather than by title --
        // its title is the thing under test, and every other property it has
        // is shared with rows this window already builds elsewhere.
        .name(RESULT_ROW_NAME)
        .build();
    // It does not speak on its own: synthesis would drag a ~1.27 GB ORT
    // session load and a queue interaction into a settings check that has
    // nothing to do with it. But a rewrite is written to be *heard*, and
    // reading it is not the same, so hearing it is one click.
    let speak = gtk::Button::builder()
        .label("Speak")
        .valign(gtk::Align::Center)
        .tooltip_text("Hear the rewritten text")
        .build();
    result.add_suffix(&speak);
    let u = ui.downgrade();
    let e = engine.clone();
    // Weak, because `speak` is a suffix *of* `result`: a strong clone would
    // be the row holding a button holding the row, which outlives the window
    // that used to contain it.
    let r = result.downgrade();
    speak.connect_clicked(move |_| {
        let Some(row) = r.upgrade() else { return };
        // The row's title, not the Test field's text: what is worth hearing
        // is what came back.
        u.with(|ui| audition(ui, &e, &row.title()));
    });
    group.add(&result);

    // One action, two ways to ask for it. `Rc` because both handlers need it
    // and a `Fn` closure cannot be cloned into two of them; it closes no
    // cycle, because everything it captures is weak.
    let u = ui.downgrade();
    let field = test.downgrade();
    let row = result.downgrade();
    let button = run.downgrade();
    let start_test = Rc::new(move || {
        let (Some(field), Some(row), Some(button)) =
            (field.upgrade(), row.upgrade(), button.upgrade())
        else {
            return;
        };
        let Some(state) = u.0.upgrade() else { return };
        let ui = Ui(state);

        // Commit whatever entry has focus first, so an endpoint typed and not
        // yet applied is what gets tested. Moving focus away is what fires
        // the rows' focus controllers (see `bind_entry`); `test_reword` then
        // reads the *pending* config, which by this point holds it.
        //
        // Written out rather than as a method call because `GtkWindowExt` and
        // `RootExt` both define `set_focus` and `adw::prelude` brings both
        // into scope.
        gtk::prelude::GtkWindowExt::set_focus(&ui.window, None::<&gtk::Widget>);

        button.set_sensitive(false);
        row.set_title("Testing…");
        row.set_subtitle("");
        row.set_visible(true);

        let rx = ui.model.test_reword(field.text().to_string());
        let weak_row = row.downgrade();
        let weak_button = button.downgrade();
        // The main thread does not wait. The request is blocking and runs on
        // the daemon's blocking pool; this future only awaits its answer. If
        // the window closes mid-flight the receiver is dropped with it and
        // the delivery is discarded, and the job ends on its own at the
        // client's own ceiling at the latest.
        glib::spawn_future_local(async move {
            let outcome = rx.recv().await;
            if let Some(button) = weak_button.upgrade() {
                button.set_sensitive(true);
            }
            let Some(row) = weak_row.upgrade() else {
                return;
            };
            match outcome {
                Ok(outcome) => {
                    // Two labels and a visibility, and no rule of its own:
                    // every number and every string in them was produced in
                    // `settings::model`, which is the layer with tests. If a
                    // truncation, a unit or a comparison is wanted, it goes
                    // there.
                    row.set_title(&outcome.title());
                    row.set_subtitle(&outcome.subtitle());
                }
                // The sender was dropped without answering, which means the
                // model's thread died. Nothing to diagnose from here.
                Err(_) => {
                    row.set_title("The test did not complete");
                    row.set_subtitle("");
                }
            }
        });
    });
    let go = start_test.clone();
    run.connect_clicked(move |_| go());
    // Pressing Enter in the field is the same action as pressing the button;
    // a test row you have to reach for the mouse to use is a test row nobody
    // uses twice.
    test.connect_entry_activated(move |_| start_test());

    group
}

/// The allowlist: an entry to add a name, and one row per name already on it.
///
/// A group of its own rather than more rows under the switches, for three
/// reasons. It is the only part of the window whose row *count* changes, and
/// a group boundary is what keeps a rebuild from having to know which of a
/// mixed group's children were its own. It is the only part that needs a
/// paragraph of explanation -- an empty list speaks nothing, which is a trap
/// worth a description rather than a subtitle on somebody else's row. And at
/// the window's 520px it reads as a list: libadwaita draws each group as its
/// own rounded card, so a dozen one-line rows sit under their own heading
/// instead of turning the Notifications card into a wall.
fn allowlist_group(ui: &Ui, cfg: &Config) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Applications to announce")
        .description(
            "Matched against the name an application gives itself, ignoring case. \
             Nothing is spoken while this list is empty: switch announcements on with \
             it empty and sayd logs every name it declines, which is how to find them.",
        )
        .build();

    let entry = adw::EntryRow::builder().title("Application name").build();
    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Add this application to the list")
        .build();
    add.add_css_class("flat");
    entry.add_suffix(&add);
    // Not registered with `Ui::row`, for the reason the Test row is not: what
    // the user has typed is not a view of the config, and clobbering it on
    // every unrelated edit would be its own bug.
    group.add(&entry);

    let u = ui.downgrade();
    // Weak for the reason the Test row's Speak button is: `add` is a suffix
    // *of* `entry`, so a strong clone would be the row holding a button
    // holding the row, and the pair would outlive the window.
    let field = entry.downgrade();
    add.connect_clicked(move |_| {
        let Some(field) = field.upgrade() else { return };
        u.with(|ui| add_to_allowlist(ui, &field));
    });
    let u = ui.downgrade();
    // Enter in the field is the same action as the button, as in the Test row.
    entry.connect_entry_activated(move |row| u.with(|ui| add_to_allowlist(ui, row)));

    // The rows the closure below has put in the group, so a rebuild takes
    // away exactly what it added and never the entry row above.
    let shown: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let g = group.clone();
    let draw = move |ui: &Ui, cfg: &Config| {
        // Rebuilt wholesale rather than diffed: the list is a handful of
        // names, and a diff would have to decide what "the same row" means
        // across a rename -- another rule, in the layer that is not allowed
        // to hold any.
        //
        // Every caller of a row's redraw closure is inside `Ui::quietly`
        // (see `Ui::redraw`), and building an `ActionRow` emits nothing that
        // would write anyway, so this cannot re-enter its own handlers. The
        // one path that looks like it might -- a Remove button whose click
        // ends up destroying the very row it is attached to -- is safe
        // because the emission holds its own reference to the button for the
        // length of the call; see `remove` below.
        for row in shown.borrow_mut().drain(..) {
            g.remove(&row);
        }
        for name in &cfg.notifications.allow {
            let row = adw::ActionRow::builder()
                .title(name)
                // `AdwPreferencesRow:use-markup` defaults to true and these
                // titles are application-controlled, which is a bad pair:
                // measured, an entry of `Ada & Co` makes GTK refuse the
                // title outright ("Failed to set text ... escape ampersand
                // as &amp;") and the row renders *blank*, so the one row
                // whose name the user most needs to read to remove it is
                // the one they cannot see.
                .use_markup(false)
                .build();
            let remove = gtk::Button::builder()
                .icon_name("list-remove-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("Stop announcing this application")
                .build();
            remove.add_css_class("flat");
            row.add_suffix(&remove);
            let u = ui.downgrade();
            let name = name.clone();
            remove.connect_clicked(move |_| {
                // Cloned out of the closure's environment before the edit,
                // because the edit redraws and the redraw destroys this row
                // and this button with it. GTK holds a reference to the
                // instance for the length of a signal emission, so the
                // closure outlives the call either way -- taking the copy
                // first means nothing here depends on knowing that.
                let name = name.clone();
                u.with(|ui| ui.apply(move |c| allow_remove(c, &name)));
            });
            g.add(&row);
            shown.borrow_mut().push(row);
        }
    };
    // Populated before the handler above could ever fire, and before this
    // closure is handed over, in the same order every other row uses.
    draw(ui, cfg);
    ui.row(draw);

    group
}

/// Put whatever the entry holds on the allowlist.
///
/// Every rule about *what* that means -- an empty name, a name already
/// there in some other casing, the surrounding whitespace -- is
/// `allow_add`'s, in `model.rs`. This asks only the one question a widget
/// has to answer for itself: whether to clear the field.
fn add_to_allowlist(ui: &Ui, entry: &adw::EntryRow) {
    let name = entry.text().to_string();
    ui.apply(|c| allow_add(c, &name));
    // Cleared only once the list really holds the name -- which covers the
    // duplicate case too, where nothing was added but the name is listed and
    // the field has served its purpose. When it does not (an empty field, or
    // an edit the model refused and toasted), what was typed stays put to be
    // fixed rather than vanishing.
    if allow_contains(&ui.model.current(), &name) {
        // The entry has no handler `set_text` could re-enter today --
        // `entry_activated` fires on Enter, not on a programmatic set -- but
        // this is a widget write made by this code, and the window has one
        // way of saying so.
        ui.quietly(|| entry.set_text(""));
    }
}

/// One half of the suggestions: a row per application of `kind`, each with
/// its icon, its name, and a button that puts it on the allowlist above.
///
/// `kind` is `Suggestion::seen` -- see [`SUGGESTION_GROUPS`] for why the two
/// halves are drawn as separate groups and what each says for itself.
///
/// Which applications those are, in what order, with what already filtered
/// out and what an icon string means, is entirely `SettingsModel::
/// suggestions`'s. This function decides only what a row looks like, which is
/// the one question that cannot be answered without a display.
fn suggestions_group(
    ui: &Ui,
    cfg: &Config,
    kind: SuggestionKind,
    title: &str,
    description: &str,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(title)
        .description(description)
        .build();

    // The rows this closure has put in the group, so a rebuild takes away
    // exactly what it added -- the same bookkeeping `allowlist_group` does,
    // and needed here for the same reason even though this group has no
    // permanent row of its own to protect: a `PreferencesGroup` offers no
    // "remove everything" call.
    let shown: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let g = group.clone();
    let draw = move |ui: &Ui, _cfg: &Config| {
        // `_cfg` is unused because what this group shows comes from the
        // suggestion cache, which the model computed from its own
        // `current()` -- it has to, since the filter it applies is against
        // the allowlist *and* the seen registry, which no `Config` carries.
        // That is not a second source of truth: every caller of
        // `Ui::redraw` passes the config the model is currently holding
        // (see the call sites), so the two agree.
        //
        // Rebuilt wholesale rather than diffed, as the allowlist is, and
        // safe to re-enter for the same two reasons: every redraw closure
        // runs inside `Ui::quietly`, and building an `ActionRow` emits
        // nothing that writes. The one path that looks circular -- an Add
        // button whose click redraws and so destroys the very row it is
        // attached to -- is the shape `allowlist_group`'s Remove button
        // already has, and is safe because the emission holds its own
        // reference to the button for the length of the call.
        for row in shown.borrow_mut().drain(..) {
            g.remove(&row);
        }
        // Collected out of the cache rather than iterated in place: the Add
        // button below redraws, and a redraw refreshes that very cache
        // (`Ui::refresh_suggestions`), so holding the borrow across the
        // loop would make the click a panic waiting for a coincidence
        // rather than a plain sequence of calls.
        let mine: Vec<Suggestion> = ui
            .suggestions
            .borrow()
            .iter()
            .filter(|s| s.kind == kind)
            .cloned()
            .collect();
        let any = !mine.is_empty();
        for s in mine {
            let row = adw::ActionRow::builder()
                .title(&s.app_name)
                // `AdwPreferencesRow:use-markup` defaults to true and this
                // title is a string an *application* chose, which is the
                // worst possible pair: an `app_name` of `Ada & Co` makes GTK
                // refuse the title outright and render the row blank. The
                // allowlist rows set it for the same reason, one step later
                // in the same string's life.
                .use_markup(false)
                .build();
            row.add_prefix(&suggestion_icon(&s.icons));
            let add = gtk::Button::builder()
                .icon_name("list-add-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(format!("Announce notifications from {}", s.app_name))
                .build();
            add.add_css_class("flat");
            row.add_suffix(&add);
            let u = ui.downgrade();
            let name = s.app_name.clone();
            add.connect_clicked(move |_| {
                // The same call the entry row's Add makes, deliberately: a
                // suggestion and a typed name are one operation, and every
                // rule about what adding means (the trim, the duplicate, the
                // empty name) stays in `allow_add` rather than being decided
                // twice. Cloned out of the environment first because the
                // edit redraws, and the redraw destroys this row.
                let name = name.clone();
                u.with(|ui| ui.apply(move |c| allow_add(c, &name)));
            });
            g.add(&row);
            shown.borrow_mut().push(row);
        }
        // Hidden rather than shown empty: a group heading and a paragraph
        // about applications, over nothing at all, is what a user sees the
        // moment they have allowed everything -- which is the state this
        // whole feature is meant to lead to, so it is the state it must not
        // look broken in.
        g.set_visible(any);
    };
    // Populated before the closure is handed over, in the order every other
    // row here uses.
    draw(ui, cfg);
    ui.suggestion_row(draw);

    group
}

/// The image shown beside a suggestion: the first of its candidates that
/// actually draws, or [`FALLBACK_ICON`].
///
/// `model.rs` decides which strings to try and in what order (see
/// `icon_candidates`); this walks them, because the two questions that
/// separate a candidate that draws from one that does not are the two only
/// this layer can ask -- does the display's icon theme have this name, and
/// is this path a readable image of a sane size. `model.rs` deliberately
/// classifies the *string* without touching either.
///
/// Falling through to `FALLBACK_ICON` covers the cases `gtk::Image` would
/// otherwise draw as its own broken-image glyph -- a theme that does not have
/// the named icon, a file that is no longer there -- and now also the case
/// where every candidate a sender offered was unusable.
fn suggestion_icon(icons: &[IconSource]) -> gtk::Image {
    for candidate in icons {
        let drawn = match candidate {
            IconSource::Named(name) if theme_has(name) => Some(gtk::Image::from_icon_name(name)),
            IconSource::File(path) => image_from_file(path),
            _ => None,
        };
        if let Some(image) = drawn {
            image.set_pixel_size(SUGGESTION_ICON_PX);
            return image;
        }
    }
    let image = gtk::Image::from_icon_name(FALLBACK_ICON);
    image.set_pixel_size(SUGGESTION_ICON_PX);
    image
}

/// Load an application-supplied image file, if it is one this thread can
/// afford to decode.
///
/// CRITICAL 3: `gtk::Image::from_file` decodes whatever the path names,
/// synchronously, on the glib main thread, at whatever size the file
/// declares. `path` comes from a notification, which means the *sender*
/// chose it. Measured through the same gdk-pixbuf path GTK uses: a 435 KB
/// PNG declaring 12000x12000 decodes in 442 ms and 432 MB, and a group of
/// `MAX_SEEN` such rows -- all alive at once, rebuilt on every redraw -- is
/// tens of seconds and tens of gigabytes. Three things stop that here:
///
/// - the byte count comes from the `stat` this was already doing, and is
///   checked first because it is the one that costs nothing.
/// - `Pixbuf::file_info` reads only enough of the file to learn its format
///   and declared size, so the decision costs a header rather than an image
///   (20 ms for that PNG, against 238 ms and 432 MB to decode it).
/// - `icon_pixels_within_limit` (the rules, in `model.rs`) rejects anything
///   bigger than a real icon.
/// - what is left is loaded with `from_file_at_scale`, which scales during
///   decode, so the buffer that reaches the row is icon-sized rather than
///   whatever the file declared and then scaled down for display.
///
/// A file that is not an image at all, or one whose header does not parse,
/// is `None` and falls back like anything else -- `file_info` answering
/// nothing is exactly the case a broken-image glyph used to be drawn for.
/// The `is_file` check that used to guard this is kept, in the form of the
/// metadata call: it correctly rejects a FIFO, which would otherwise block
/// this thread on a reader that never comes.
fn image_from_file(path: &Path) -> Option<gtk::Image> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || !icon_file_size_within_limit(metadata.len()) {
        return None;
    }
    let (_format, width, height) = gtk::gdk_pixbuf::Pixbuf::file_info(path)?;
    if !icon_pixels_within_limit(width, height) {
        return None;
    }
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(
        path,
        SUGGESTION_ICON_PX,
        SUGGESTION_ICON_PX,
        true,
    )
    .ok()?;
    Some(gtk::Image::from_paintable(Some(
        &gtk::gdk::Texture::for_pixbuf(&pixbuf),
    )))
}

/// Does the display's icon theme actually have `name`?
///
/// `true` with no display at all, which cannot happen while a window is being
/// drawn: it keeps the answer identical to what `gtk::Image::from_icon_name`
/// would have done on its own, rather than inventing a fallback for a case
/// where nothing is being shown to anybody.
fn theme_has(name: &str) -> bool {
    gtk::gdk::Display::default()
        .map(|display| gtk::IconTheme::for_display(&display).has_icon(name))
        .unwrap_or(true)
}

/// The one thing in this file that can be tested without a user in front of
/// it, and the one thing in it that must be: what an application-supplied
/// image file is allowed to cost.
///
/// Everything else here is a widget arrangement whose correctness is "does
/// it look right", which is why this file otherwise has no tests and every
/// rule lives in `model.rs`. [`image_from_file`] is different -- it decides
/// whether to hand a stranger's file to an image decoder on the main
/// thread, and that is a decision with a measurable answer.
///
/// Two later additions ride in the same harness, for the same reason: they
/// need a real window under a real compositor, which is the only place they
/// can be wrong. [`a_newly_seen_application_appears_while_the_window_is_open`]
/// covers the suggestion poll, and the Reword group's pair covers what
/// nothing else in this window does -- a two-way text entry (when it
/// commits, and that a redraw does not clobber what is being typed) and
/// whether closing the window still frees every widget in it.
///
/// **One test function, deliberately**, and one that skips unless it can
/// have the main thread. GTK may only be initialised once and only from the
/// thread that will use it (gtk4-rs panics with "attempted to initialize GTK
/// from two different threads" otherwise), and the default test harness runs
/// each test on a worker thread, where `gtk::init` fails -- so under a plain
/// `cargo test` this skips, exactly as `notify::monitor`'s tests skip a
/// machine with no `dbus-daemon`. To actually run it, give it the main
/// thread and a compositor:
///
/// ```text
/// WLR_BACKENDS=headless WLR_RENDERER=pixman sway &
/// WAYLAND_DISPLAY=wayland-1 cargo test -p sayd --bin sayd \
///     settings::window -- --test-threads=1
/// ```
#[cfg(test)]
mod tests {
    use super::*;
    // `TestOutcome` only to name the one row this module has to *skip* -- see
    // `press_test`. The window itself never matches on a variant.
    use crate::settings::model::{TestOutcome, MAX_ICON_PIXELS};
    use gtk::gdk_pixbuf::{Colorspace, Pixbuf};

    /// Write a PNG of exactly `width`x`height` to `path`.
    fn png(path: &std::path::Path, width: i32, height: i32) {
        let pixbuf = Pixbuf::new(Colorspace::Rgb, false, 8, width, height).expect("a pixbuf");
        pixbuf.fill(0x4444_44ff);
        pixbuf.savev(path, "png", &[]).expect("writing a png");
    }

    /// A model backed by a temporary config file, and the engine it writes
    /// through. The engine is returned so the caller can shut it down.
    fn model_in(dir: &std::path::Path) -> (Arc<SettingsModel>, EngineHandle) {
        let engine = EngineHandle::spawn(
            Config::default(),
            Box::new(sayd_core::synth::StubSynthesizer::new()),
            Box::new(sayd_core::audio::VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(crate::config_watch::ConfigStore::new(
            dir.join("config.toml"),
            engine.clone(),
            Config::default(),
        ));
        let model = Arc::new(SettingsModel::new(
            store,
            dir.to_path_buf(),
            Config::default(),
        ));
        (model, engine)
    }

    /// The row of type `T` under `widget` with this title, so a test can
    /// drive the real widget rather than the model it was built from.
    ///
    /// Generic over the row type because the two it is asked for differ only
    /// in that: an `AdwEntryRow` (and `AdwPasswordEntryRow`, which is one)
    /// and an `AdwSpinRow`.
    fn find_row<T: IsA<adw::PreferencesRow> + IsA<gtk::Widget>>(
        widget: &gtk::Widget,
        title: &str,
    ) -> Option<T> {
        if let Some(row) = widget.downcast_ref::<T>() {
            if row.title() == title {
                return Some(row.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            if let Some(found) = find_row::<T>(&w, title) {
                return Some(found);
            }
            child = w.next_sibling();
        }
        None
    }

    /// A model that answers the Test row from `answer` after `delay`,
    /// instead of from a provider.
    ///
    /// The Test row's whole contract is a thing that takes time and then
    /// reports a number, and neither half can be driven against a real
    /// endpoint in a committed test. Injecting the client is what makes the
    /// latency sentence -- this task's deliverable -- assertable at all, and
    /// makes it assert the same thing with and without `--features reword`
    /// (where a real endpoint would report `Unavailable` and nothing else).
    fn model_answering(
        dir: &std::path::Path,
        answer: &str,
        delay: Duration,
    ) -> (Arc<SettingsModel>, EngineHandle) {
        struct Canned(String, Duration);
        impl crate::reword::Rewriter for Canned {
            fn reword(&self, _text: &str) -> Result<String, crate::reword::RewordError> {
                // On the model's own thread, never this one -- which is
                // exactly what the "did not block" assertion below is about.
                std::thread::sleep(self.1);
                Ok(self.0.clone())
            }
        }

        let engine = EngineHandle::spawn(
            Config::default(),
            Box::new(sayd_core::synth::StubSynthesizer::new()),
            Box::new(sayd_core::audio::VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(crate::config_watch::ConfigStore::new(
            dir.join("config.toml"),
            engine.clone(),
            Config::default(),
        ));
        let canned: Arc<dyn crate::reword::Rewriter> = Arc::new(Canned(answer.to_string(), delay));
        let model = Arc::new(SettingsModel::new_with_rewriter(
            store,
            dir.to_path_buf(),
            Config::default(),
            Arc::new(move |_| Ok(canned.clone())),
        ));
        (model, engine)
    }

    /// The `AdwPreferencesGroup` with this title.
    ///
    /// Needed because a walk from the window is not specific enough: two rows
    /// in this window are titled "Test" -- the voice group's and the Reword
    /// group's -- and the first one a depth-first walk meets is whichever
    /// group `build` added first.
    fn find_group(widget: &gtk::Widget, title: &str) -> Option<adw::PreferencesGroup> {
        if let Some(group) = widget.downcast_ref::<adw::PreferencesGroup>() {
            if group.title() == title {
                return Some(group.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            if let Some(found) = find_group(&w, title) {
                return Some(found);
            }
            child = w.next_sibling();
        }
        None
    }

    /// The Reword group's result row, by the name `reword_group` gave it.
    ///
    /// By identity and not by title, because its title *is* the thing under
    /// test; and not by shape either, because `AdwSwitchRow` and
    /// `AdwSpinRow` are both `AdwActionRow`s and this group has three of
    /// them.
    fn find_result_row(widget: &gtk::Widget) -> Option<adw::ActionRow> {
        if let Some(row) = widget.downcast_ref::<adw::ActionRow>() {
            if row.widget_name() == RESULT_ROW_NAME {
                return Some(row.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            if let Some(found) = find_result_row(&w) {
                return Some(found);
            }
            child = w.next_sibling();
        }
        None
    }

    /// The `GtkButton` under `widget` with this label.
    fn find_button(widget: &gtk::Widget, label: &str) -> Option<gtk::Button> {
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            if button.label().is_some_and(|l| l == label) {
                return Some(button.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            if let Some(found) = find_button(&w, label) {
                return Some(found);
            }
            child = w.next_sibling();
        }
        None
    }

    /// Every `GtkLabel` anywhere under `widget`.
    fn labels(widget: &gtk::Widget) -> Vec<gtk::Label> {
        let mut out = Vec::new();
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            out.push(label.clone());
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            out.extend(labels(&w));
            child = w.next_sibling();
        }
        out
    }

    /// Every `GtkLabel`'s text anywhere under `widget`.
    ///
    /// What a *rendered* string is: a label GTK refused to set because the
    /// markup did not parse is empty here, which is the whole failure mode
    /// `use_markup(false)` exists for and the one an assertion on the config
    /// would sail straight past.
    fn label_texts(widget: &gtk::Widget) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            out.push(label.text().to_string());
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            out.extend(label_texts(&w));
            child = w.next_sibling();
        }
        out
    }

    /// A weak reference to every widget in the tree, for the leak
    /// measurement: a `WeakRef` that still upgrades after the last strong
    /// `Ui` is gone is a widget that was not freed.
    fn weak_widgets(widget: &gtk::Widget, out: &mut Vec<glib::WeakRef<gtk::Widget>>) {
        out.push(widget.downgrade());
        let mut child = widget.first_child();
        while let Some(w) = child {
            weak_widgets(&w, out);
            child = w.next_sibling();
        }
    }

    /// Every `AdwActionRow` title anywhere under `widget`.
    fn row_titles(widget: &gtk::Widget) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(row) = widget.downcast_ref::<adw::ActionRow>() {
            out.push(row.title().to_string());
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            out.extend(row_titles(&w));
            child = w.next_sibling();
        }
        out
    }

    /// Turn the main loop for up to `limit`, stopping as soon as `done`.
    fn spin_until(limit: Duration, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + limit;
        let ctx = glib::MainContext::default();
        while std::time::Instant::now() < deadline {
            while ctx.iteration(false) {}
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// IMPORTANT 7: an application that notifies while the window is open
    /// must appear under "Seen notifying" without the window being closed
    /// and reopened. Every `Ui::redraw` call site is driven by a config
    /// change, and an application notifying is not one -- so the README's
    /// own walkthrough (leave Settings open, trigger a notification) showed
    /// nothing at all, which is exactly the discovery loop these
    /// suggestions exist to close.
    ///
    /// Drives the real window: `build` puts the real groups together and
    /// registers the real poll, `seen::record` is the same call the monitor
    /// makes off the bus, and the assertion walks the actual widget tree
    /// for the row rather than asking the cache whether it thinks a row
    /// exists.
    ///
    /// IMPORTANT 6 rides along: with the two groups drawing from one
    /// partitioned list, an application that is both seen and curated
    /// appears exactly once, where the two independent `suggestions()`
    /// calls this replaced could show it in neither.
    fn a_newly_seen_application_appears_while_the_window_is_open(dir: &std::path::Path) {
        let (model, engine) = model_in(dir);
        let ui = build(model, engine.clone());

        let before = row_titles(ui.window.upcast_ref());
        assert!(
            !before.iter().any(|t| t == "win1-JustNotified"),
            "the application has not notified yet, so there must be no row for it"
        );
        assert_eq!(
            before.iter().filter(|t| *t == "Signal").count(),
            1,
            "a curated application appears once, from one of the two groups"
        );

        seen::record(&crate::notify::Notification {
            app_name: "win1-JustNotified".into(),
            desktop_entry: String::new(),
            image_path: String::new(),
            app_icon: String::new(),
            summary: "s".into(),
            body: "b".into(),
        });

        spin_until(SEEN_POLL_INTERVAL * 5, || {
            row_titles(ui.window.upcast_ref())
                .iter()
                .any(|t| t == "win1-JustNotified")
        });

        let after = row_titles(ui.window.upcast_ref());
        assert!(
            after.iter().any(|t| t == "win1-JustNotified"),
            "an application that notified while the window was open must show up in it"
        );
        assert_eq!(
            after.iter().filter(|t| *t == "Signal").count(),
            1,
            "and the two groups must still not disagree about a curated one"
        );

        drop(ui);
        engine.shutdown();
    }

    /// The three properties that make a two-way text row different from
    /// every other row in this window, and the one that makes it dangerous.
    ///
    /// There was no text entry bound to a config field anywhere here before
    /// this milestone. Committing per keystroke would write a dozen
    /// half-URLs to the file and hand each one to the validator; a redraw
    /// that set the text unconditionally would move the cursor out from
    /// under someone typing; and a value with an `&` in it -- which is what
    /// a URL with a query string is -- renders blank on
    /// `AdwPreferencesRow:use-markup`'s default.
    ///
    /// Drives the real widgets under a real compositor: `set_text` is
    /// typing, `apply` is the button and Enter, and `grab_focus` elsewhere
    /// is tabbing away. Asserting on the model without touching a widget
    /// would test nothing this task added.
    fn the_reword_entry_rows_commit_on_apply_and_never_clobber_typing(dir: &std::path::Path) {
        let dir = dir.join("reword-rows");
        std::fs::create_dir_all(&dir).expect("a config directory of its own");
        let (model, engine) = model_in(&dir);
        let ui = build(model.clone(), engine.clone());
        // Presented, because half of this drives focus and an unmapped
        // window has none to give.
        ui.window.present();
        spin_until(Duration::from_secs(2), || ui.window.is_mapped());

        let titles = row_titles(ui.window.upcast_ref());
        for expected in [
            "Rewrite notifications",
            "Deadline",
            "Longest text to rewrite",
        ] {
            assert!(
                titles.iter().any(|t| t == expected),
                "the Reword group must carry a {expected} row: {titles:?}"
            );
        }

        let endpoint =
            find_row::<adw::EntryRow>(ui.window.upcast_ref(), "Endpoint").expect("an Endpoint row");
        let model_row =
            find_row::<adw::EntryRow>(ui.window.upcast_ref(), "Model").expect("a Model row");
        let opened_on = model.current().reword.base_url.clone();
        assert_eq!(
            endpoint.text(),
            opened_on,
            "a row is populated from the config before its handler is connected"
        );

        // Typing alone must change nothing: this is the whole commit rule.
        endpoint.set_text("http://box.lan:11434/v1");
        assert_eq!(
            model.current().reword.base_url,
            opened_on,
            "typing must not write; a base URL is invalid for almost the whole \
             time you are typing it"
        );

        // Applying commits it.
        endpoint.emit_by_name::<()>("apply", &[]);
        assert_eq!(model.current().reword.base_url, "http://box.lan:11434/v1");

        // So does tabbing away, which is what a user who does not notice the
        // apply button does. `grab_focus` on the row focuses the `GtkText`
        // inside it; the controller is on the row, and its `leave` covers
        // the whole subtree.
        let model_opened_on = model.current().reword.model.clone();
        assert!(model_row.grab_focus(), "the Model row must be focusable");
        model_row.set_text("qwen2.5:7b");
        assert_eq!(
            model.current().reword.model,
            model_opened_on,
            "still nothing written while the field has the focus"
        );
        assert!(endpoint.grab_focus(), "focus must move to another row");
        assert_eq!(
            model.current().reword.model,
            "qwen2.5:7b",
            "leaving a field commits what is in it"
        );

        // An unrelated edit redraws every row. The endpoint row must come
        // back holding what the config holds -- and, crucially, must not be
        // rewritten when it already agrees.
        model.edit(|c| c.reword.timeout_ms = 900).expect("edit");
        ui.redraw(&model.current());
        assert_eq!(endpoint.text(), "http://box.lan:11434/v1");

        // A value with an `&` in it, in the row and in the group description
        // built from it. Both are strings GTK parses as markup unless told
        // otherwise, and both fail silently when it cannot: the row renders
        // blank, and the group description -- which has no `use-markup` to
        // turn off, see `group_description` -- keeps the endpoint it was
        // last set to and so describes the wrong host.
        model
            .edit(|c| c.reword.base_url = "http://ada&co.lan:11434/v1".into())
            .expect("edit");
        ui.redraw(&model.current());
        assert_eq!(endpoint.text(), "http://ada&co.lan:11434/v1");
        let labels = label_texts(ui.window.upcast_ref());
        assert!(
            labels.iter().any(|l| l.contains("ada&co.lan")),
            "the group description must render the host it names: {labels:?}"
        );

        // The password row is bound the same way, and is offered only where
        // a key can be used -- `reword_key_row_applies`'s rule, in
        // `model.rs`. That endpoint is not this machine, so it is there.
        let key =
            find_row::<adw::EntryRow>(ui.window.upcast_ref(), "API key").expect("an API key row");
        assert!(key.is_visible(), "a remote endpoint may want a key");
        key.set_text("sk-typed");
        assert_eq!(model.current().reword.api_key, "");
        key.emit_by_name::<()>("apply", &[]);
        assert_eq!(model.current().reword.api_key, "sk-typed");

        // Back to this machine with the key still in the file: the row stays,
        // because it is the only way to read or clear it.
        model
            .edit(|c| c.reword.base_url = "http://localhost:11434/v1".into())
            .expect("edit");
        ui.redraw(&model.current());
        assert!(key.is_visible(), "a key the file holds keeps its row");
        assert_eq!(key.text(), "sk-typed", "and shows what the file holds");

        // Cleared, and now there is nothing for it to offer.
        key.set_text("");
        key.emit_by_name::<()>("apply", &[]);
        assert_eq!(model.current().reword.api_key, "");
        assert!(
            !key.is_visible(),
            "a local endpoint with no key stored takes no credential"
        );

        // The two spin rows offer exactly the model's bounds, and cannot
        // produce a value outside them. The deadline's ceiling is the one
        // that matters: it is arithmetic in `sayd_core::config` against
        // sayd-cli's own D-Bus timeout, it moved once already, and a literal
        // in this file is how the two would drift apart.
        for (title, min, max, step, held) in [
            (
                "Deadline",
                REWORD_TIMEOUT_MIN,
                REWORD_TIMEOUT_MAX,
                REWORD_TIMEOUT_STEP,
                (|c: &Config| c.reword.timeout_ms as f64) as fn(&Config) -> f64,
            ),
            (
                "Longest text to rewrite",
                REWORD_MAX_CHARS_MIN,
                REWORD_MAX_CHARS_MAX,
                REWORD_MAX_CHARS_STEP,
                |c: &Config| c.reword.max_chars as f64,
            ),
        ] {
            let row = find_row::<adw::SpinRow>(ui.window.upcast_ref(), title).expect("a spin row");
            let adjustment = row.adjustment();
            assert_eq!(
                (
                    adjustment.lower(),
                    adjustment.upper(),
                    adjustment.step_increment()
                ),
                (min, max, step),
                "{title} must offer the bounds `model.rs` names"
            );
            row.set_value(max + step);
            assert_eq!(row.value(), max, "{title} must stop at its ceiling");
            assert_eq!(
                held(&model.current()),
                max,
                "{title} writes what it shows, through the model like every other row"
            );
        }

        ui.window.destroy();
        drop(ui);
        engine.shutdown();
    }

    /// The API key row's visibility, driven from the preset *buttons*
    /// themselves rather than from an edited `base_url` -- the path a
    /// review finding measured wrong for vLLM: its preset is a loopback
    /// `base_url`, which `reword_key_row_applies` used to treat as
    /// certain proof of "no credential", hiding the only row that could
    /// hold a key for `vllm serve --api-key …`.
    ///
    /// [`ENDPOINT_PRESETS`]' third field is §6's Key column, collapsed to a
    /// bool, and it is also this test's expectation: "ignored" (`false`)
    /// for the three purely local servers, "as configured" or `sk-…`
    /// (`true`) for vLLM and the two remote providers. Reading the
    /// expectation back out of the same table the buttons are built from
    /// means a preset added there without a visibility check present would
    /// fail loudly here rather than needing a seventh row spelled out by
    /// hand.
    fn the_key_row_visibility_follows_the_clicked_preset(dir: &std::path::Path) {
        let dir = dir.join("reword-preset-key-visibility");
        std::fs::create_dir_all(&dir).expect("a config directory of its own");
        let (model, engine) = model_in(&dir);
        let ui = build(model.clone(), engine.clone());
        // Presented, like `the_reword_entry_rows_commit_on_apply_and_never_
        // clobber_typing` -- a row's own `visible` property is set
        // synchronously by `set_visible`, but reading it back through an
        // unmapped, unpresented top-level is not this test's business to
        // rely on either way, and every other real-window test in this
        // module presents first.
        ui.window.present();
        spin_until(Duration::from_secs(2), || ui.window.is_mapped());

        let key =
            find_row::<adw::EntryRow>(ui.window.upcast_ref(), "API key").expect("an API key row");

        for (name, url, takes_key) in ENDPOINT_PRESETS {
            let label = format!("{name} — {url}");
            let button = find_button(ui.window.upcast_ref(), &label)
                .unwrap_or_else(|| panic!("a preset button labelled {label:?}"));
            button.emit_clicked();
            assert_eq!(
                model.current().reword.base_url,
                url,
                "{name}'s preset button must commit its own URL"
            );
            assert_eq!(
                key.is_visible(),
                takes_key,
                "{name} ({url}): key row visible = {}, want {takes_key} per §6's Key column",
                key.is_visible()
            );
        }

        ui.window.destroy();
        drop(ui);
        engine.shutdown();
    }

    /// An `AdwActionRow`'s subtitle. `Option<GString>` because libadwaita
    /// distinguishes "no subtitle" from an empty one; nothing here does.
    fn subtitle_of(row: &adw::ActionRow) -> String {
        row.subtitle().unwrap_or_default().to_string()
    }

    /// Wait for a test already in flight to report, pressing again only if
    /// what came back was `Busy`.
    ///
    /// `Busy` is not flakiness in the row but sharing in the suite:
    /// `crate::reword::state()`'s two permits are process-wide, and
    /// `settings::model`'s own tests take them from another thread of the
    /// same binary. It is a real row with its own wording, asserted from the
    /// variant in `model.rs`; what it is not is a thing these tests are
    /// about. Retrying it is safe for the first-request caveat too: `Busy` is
    /// returned before `note_endpoint`, so a refused press cannot consume the
    /// flag.
    ///
    /// Waits *before* pressing, and one press is one request: `emit_clicked`
    /// ignores sensitivity, so a helper that pressed on the way in would put
    /// a second request in flight behind the caller's own and leave which
    /// answer landed last to chance -- and the second is never the first
    /// against its endpoint.
    fn await_answer(run: &gtk::Button, result: &adw::ActionRow) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            spin_until(Duration::from_secs(20), || result.title() != "Testing…");
            assert_ne!(
                result.title(),
                "Testing…",
                "the result never arrived; the glib future is not being driven"
            );
            if result.title() != TestOutcome::Busy.title() || std::time::Instant::now() > deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
            run.emit_clicked();
        }
    }

    /// Press Test and wait for what it reports.
    fn press_test(run: &gtk::Button, result: &adw::ActionRow) {
        run.emit_clicked();
        await_answer(run, result);
    }

    /// The Test row's whole contract, driven through the real widgets.
    ///
    /// The latency sentence is what this task exists for. Nothing in this
    /// project measures end-to-end provider latency and nothing else will, so
    /// this row is the only route by which anyone learns what their own
    /// provider costs -- and the comparison against the *configured* deadline
    /// is the only thing that answers the question the number is for. A row
    /// that reported the outcome but not the latency, or the latency but not
    /// the comparison, would satisfy nobody, so all three are asserted on the
    /// string that actually reached a `GtkLabel`.
    ///
    /// Against an injected client rather than a socket: the number has to be
    /// compared against a deadline this test chooses, on both sides of it,
    /// and the first-request caveat has to be seen appearing once and not
    /// twice. None of that is drivable against a real provider, and against a
    /// dead port the whole row would collapse to one failure sentence -- in
    /// the default build, to `Unavailable`, which tests nothing at all.
    fn the_test_row_reports_the_latency_against_the_deadline(dir: &std::path::Path) {
        let dir = dir.join("reword-test-row");
        std::fs::create_dir_all(&dir).expect("a config directory of its own");
        // Long, because a row that ellipsised instead of wrapping would be
        // cutting off the one thing the row is for -- and with an `&` in it,
        // because `AdwPreferencesRow:use-markup` defaults to `true` and a
        // title it cannot parse renders *blank*. This is the one row in the
        // window whose title a provider writes, so it is the one where that
        // character cannot be ruled out.
        let answer = "Alice & Bob are asking whereabouts you would like to go for dinner";
        let (model, engine) = model_answering(&dir, answer, Duration::from_millis(300));
        model
            .edit(|c| {
                // An endpoint key nothing else in this binary uses. Whether a
                // request is the *first* against an endpoint is process-wide
                // state keyed `base_url|model`, and this test asserts on the
                // caveat that fact produces.
                c.reword.model = "window-test-latency".into();
                // Under the stub's own delay, so the first answers are
                // `Slower` -- the row this whole task exists for.
                c.reword.timeout_ms = 200;
            })
            .expect("edit");
        let ui = build(model.clone(), engine.clone());
        ui.window.present();
        spin_until(Duration::from_secs(2), || ui.window.is_mapped());

        let group = find_group(ui.window.upcast_ref(), "Reword").expect("a Reword group");
        let test = find_row::<adw::EntryRow>(group.upcast_ref(), "Test").expect("a Test row");
        assert_eq!(
            test.text(),
            REWORD_TEST_DEFAULT,
            "pressing Test once without typing anything must already be a \
             meaningful test"
        );
        let run = find_button(group.upcast_ref(), "Test").expect("a Test button");
        let result = find_result_row(ui.window.upcast_ref()).expect("a result row");
        assert!(
            !result.is_visible(),
            "the result row is hidden until the first test"
        );

        // The main thread does not wait. The stub sleeps 300 ms on the
        // model's own thread; a handler that waited for it would show up
        // here, and so would one that had put the request on this thread.
        let started = std::time::Instant::now();
        run.emit_clicked();
        let handler_took = started.elapsed();
        assert!(
            handler_took < Duration::from_millis(100),
            "the button handler blocked the main thread for {handler_took:?}"
        );
        assert!(result.is_visible(), "a test in flight has to be visible");
        assert_eq!(result.title(), "Testing…");
        assert!(
            !run.is_sensitive(),
            "a test in flight disables its own button"
        );

        await_answer(&run, &result);
        assert!(
            run.is_sensitive(),
            "the button has to come back, or the row works once per window"
        );

        // The rewritten text is the title, because it is the point.
        assert_eq!(result.title(), answer);
        let subtitle = subtitle_of(&result);
        assert!(
            subtitle.starts_with("Rewritten in "),
            "the measured latency is the first thing the sentence says: {subtitle:?}"
        );
        assert!(
            subtitle.contains(
                "longer than the 0.2 s deadline, so a real notification would \
                 have been spoken as written"
            ),
            "the number has to be compared against the *configured* deadline, \
             in the same sentence: {subtitle:?}"
        );
        assert!(
            subtitle.ends_with("(first request — includes connection setup)"),
            "a first request pays for connection setup and has to say so, or \
             the row condemns a provider on a number that will not happen \
             again: {subtitle:?}"
        );

        // ...and it reached the screen. Both labels carry a string this
        // window did not write -- the model's answer and a sentence built
        // around a provider's own numbers -- and a label GTK refused to set
        // because the markup did not parse is empty, which is the failure
        // `use_markup(false)` exists for and the one an assertion on the
        // outcome would sail straight past.
        let rendered = label_texts(result.upcast_ref());
        assert!(
            rendered.iter().any(|l| l == answer),
            "the rewritten text must render: {rendered:?}"
        );
        assert!(
            rendered.contains(&subtitle),
            "the latency sentence must render: {rendered:?}"
        );

        // Wrapped, not ellipsised. The text is the point of the row, and a
        // provider's answer and a transport error are both longer than one
        // line at this window's 520 px.
        for wanted in [answer.to_string(), subtitle.clone()] {
            let label = labels(result.upcast_ref())
                .into_iter()
                .find(|l| l.text() == wanted)
                .expect("the label that carries it");
            assert!(label.wraps(), "{wanted:?} must wrap");
            assert_eq!(
                label.ellipsize(),
                gtk::pango::EllipsizeMode::None,
                "{wanted:?} must not be ellipsised"
            );
            // `GtkLabel:lines` only truncates when it is positive:
            // libadwaita passes `title-lines` through unchanged, so 0 here is
            // what "no limit" looks like coming from a `title_lines(0)` row,
            // and -1 is what it looks like on a label nobody set. Both are
            // unlimited; anything above 0 is a line count this row must not
            // have.
            assert!(
                label.lines() <= 0,
                "{wanted:?} must not be cut off at a line count, but lines is {}",
                label.lines()
            );
        }

        // A second press against the same endpoint: the same sentence,
        // without the caveat. Saying "first request" every time would be
        // exactly as useless as saying it never.
        press_test(&run, &result);
        let warm = subtitle_of(&result);
        assert!(
            warm.starts_with("Rewritten in ") && !warm.contains("first request"),
            "a warm endpoint's latency is reported without the caveat: {warm:?}"
        );

        // Widen the deadline past the stub's delay: the comparison must
        // follow the number the user configured, not a constant.
        model.edit(|c| c.reword.timeout_ms = 1500).expect("edit");
        press_test(&run, &result);
        let inside = subtitle_of(&result);
        assert!(
            inside.contains("inside the 1.5 s deadline"),
            "the deadline named is the one the Deadline row holds: {inside:?}"
        );

        // Enter in the field is the same action as the button.
        result.set_title("cleared");
        test.emit_by_name::<()>("entry-activated", &[]);
        assert_eq!(
            result.title(),
            "Testing…",
            "pressing Enter in the field runs the test too"
        );
        await_answer(&run, &result);

        // Speak submits the *row's title*, not the Test field's text. Proved
        // by making them disagree: a field that trims to nothing submits
        // nothing at all, so anything reaching the engine came from the row.
        assert_eq!(result.title(), answer);
        test.set_text("   ");
        let speak = find_button(result.upcast_ref(), "Speak").expect("a Speak button");
        speak.emit_clicked();
        spin_until(Duration::from_secs(5), || {
            engine.snapshot().current_text == answer
        });
        assert_eq!(
            engine.snapshot().current_text,
            answer,
            "Speak must audition what came back, not what is in the Test field"
        );

        ui.window.destroy();
        drop(ui);
        engine.shutdown();
    }

    /// Closing the window with a request still in flight frees it anyway.
    ///
    /// The one lifetime the Test row adds: a `glib::spawn_future_local` that
    /// outlives the click, holding what it needs to report into. Held
    /// strongly, that future is a widget tree that cannot be freed until a
    /// provider answers -- up to `REWORD_HTTP_CEILING`, ten seconds, per
    /// opening, on a daemon whose whole arrangement is to carry no GTK
    /// resources between openings.
    fn the_window_is_freed_while_a_test_is_in_flight(dir: &std::path::Path) {
        let dir = dir.join("reword-inflight-leak");
        std::fs::create_dir_all(&dir).expect("a config directory of its own");
        // Long enough that the request is *still pending* when the count is
        // taken, which is the whole point: a future that held the row
        // strongly would release it the moment it completed, so a
        // measurement taken after the answer arrived would report zero
        // whether the capture was weak or not. Measured -- a deliberately
        // strong capture is invisible to a 5 s spin against a 1.5 s stub and
        // holds all 861 widgets against this one.
        let (model, engine) = model_answering(&dir, "answered", Duration::from_secs(4));
        model
            .edit(|c| c.reword.model = "window-test-inflight".into())
            .expect("edit");

        let mut widgets = Vec::new();
        let window = {
            let ui = build(model, engine.clone());
            let group = find_group(ui.window.upcast_ref(), "Reword").expect("a Reword group");
            let run = find_button(group.upcast_ref(), "Test").expect("a Test button");
            let result = find_result_row(ui.window.upcast_ref()).expect("a result row");
            run.emit_clicked();
            assert_eq!(result.title(), "Testing…", "the test is in flight");

            weak_widgets(ui.window.upcast_ref(), &mut widgets);
            let window = ui.window.downgrade();
            ui.window.destroy();
            window
        };
        let total = widgets.len();
        // Comfortably inside the stub's delay: finalisation happens on a turn
        // of the main loop and takes milliseconds, so two seconds is slack
        // against a slow machine rather than time for the request to land.
        let started = std::time::Instant::now();
        spin_until(Duration::from_secs(2), || {
            window.upgrade().is_none() && widgets.iter().all(|w| w.upgrade().is_none())
        });
        let alive = widgets.iter().filter(|w| w.upgrade().is_some()).count();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the premise: the request must still be pending when the count is \
             taken, and it is not after {:?}",
            started.elapsed()
        );
        assert_eq!(
            alive, 0,
            "{alive} of {total} widgets survived a close with a test in flight; \
             the future is holding the row rather than a weak reference to it"
        );
        engine.shutdown();
    }

    /// The leak M5 paid for, guarded at the two places a new one would
    /// appear: the entry rows' focus controllers and the preset popover's
    /// buttons, both of which refer to widgets that refer back.
    ///
    /// Measured then: 533 of 533 widgets alive after `destroy()`, from two
    /// reference cycles rather than one, once per opening, for the life of a
    /// daemon that is supposed to carry no GTK resources between openings. A
    /// `WeakRef` that still upgrades after the only strong `Ui` is dropped is
    /// that bug returning, which is why this counts the whole tree rather
    /// than only the window.
    fn the_window_is_freed_after_the_reword_group_has_been_built(dir: &std::path::Path) {
        let dir = dir.join("reword-leak");
        std::fs::create_dir_all(&dir).expect("a config directory of its own");
        let (model, engine) = model_in(&dir);
        let mut widgets = Vec::new();
        let window = {
            let ui = build(model, engine.clone());
            weak_widgets(ui.window.upcast_ref(), &mut widgets);
            let window = ui.window.downgrade();
            ui.window.destroy();
            window
        };
        let total = widgets.len();
        // Finalisation happens on a turn of the main loop, not at the drop.
        spin_until(Duration::from_secs(2), || {
            window.upgrade().is_none() && widgets.iter().all(|w| w.upgrade().is_none())
        });
        let alive = widgets.iter().filter(|w| w.upgrade().is_some()).count();
        assert!(
            window.upgrade().is_none(),
            "the settings window is still alive after close: something in the \
             Reword group holds a strong reference back to it"
        );
        assert_eq!(
            alive, 0,
            "{alive} of {total} widgets survived the close; a handler is holding \
             a strong Ui, or a controller the widget it is attached to"
        );
        engine.shutdown();
    }

    /// Two properties, in one test for the reason the module doc gives.
    ///
    /// CRITICAL 3: `Image::from_file` decoded whatever the *sender* named,
    /// synchronously, on this thread, at whatever size the file declared.
    /// Measured through the same gdk-pixbuf entry points: a 32 KB PNG
    /// declaring 12000x12000 takes 238 ms and 432 MB to decode, once per
    /// row, with every row alive at the same time and rebuilt on every
    /// redraw. An image past the limit must not be loaded at all -- the row
    /// falls back to the generic glyph, which is what it already did for a
    /// file that is not there.
    ///
    /// CRITICAL 1: and the candidate walk that replaced the single
    /// `app_icon` -- the first icon that actually draws wins, an unusable
    /// one is skipped rather than being the row's answer, and a list with
    /// nothing usable in it is the fallback glyph. That last case is what
    /// every row of this feature drew, for every real application, while
    /// the icon hints were being discarded.
    #[test]
    fn a_suggestion_draws_its_first_usable_icon_and_never_an_oversized_one() {
        if gtk::init().is_err() {
            eprintln!(
                "skipping: GTK needs the main thread and a display -- see this \
                 module's doc comment for how to run this test"
            );
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");

        let ordinary = dir.path().join("ordinary.png");
        png(&ordinary, 48, 48);
        let drawn = image_from_file(&ordinary).expect("a real icon must still draw");
        assert!(
            drawn.paintable().is_some(),
            "and must draw as an actual image rather than an empty widget"
        );

        let oversized = dir.path().join("oversized.png");
        png(&oversized, MAX_ICON_PIXELS + 1, 64);
        assert!(
            image_from_file(&oversized).is_none(),
            "an image past the pixel limit must not reach the decoder"
        );
        assert!(
            image_from_file(&dir.path().join("not-there.png")).is_none(),
            "a path that is not there is a fallback, not a broken-image glyph"
        );
        assert!(
            image_from_file(dir.path()).is_none(),
            "and neither is a directory"
        );

        // A name no theme has, followed by a file that is there: the file
        // must win rather than the row settling for the fallback.
        let image = suggestion_icon(&[
            IconSource::Named("sayd-no-such-icon-anywhere".into()),
            IconSource::File(ordinary.clone()),
        ]);
        assert_eq!(image.pixel_size(), SUGGESTION_ICON_PX);
        assert!(
            image.icon_name().is_none(),
            "a drawable file must beat a theme name that does not resolve"
        );

        // An oversized file is not "usable but big": the walk must carry on
        // past it to whatever else the sender offered.
        let image = suggestion_icon(&[
            IconSource::File(oversized.clone()),
            IconSource::Named(FALLBACK_ICON.into()),
        ]);
        assert_eq!(
            image.icon_name().map(|n| n.to_string()),
            Some(FALLBACK_ICON.to_string())
        );

        // Nothing at all is the fallback, and so is a list of unusable
        // candidates.
        for icons in [
            Vec::new(),
            vec![IconSource::Named("sayd-no-such-icon-anywhere".into())],
            vec![IconSource::File(dir.path().join("gone.png"))],
        ] {
            assert_eq!(
                suggestion_icon(&icons).icon_name().map(|n| n.to_string()),
                Some(FALLBACK_ICON.to_string()),
                "an unusable candidate list is the generic glyph, not a broken image"
            );
        }

        // The rest of what needs a real window, run from here rather than
        // from a `#[test]` of its own for the same one-init reason.
        adw::init().expect("libadwaita initialises once GTK has");
        a_newly_seen_application_appears_while_the_window_is_open(dir.path());
        the_reword_entry_rows_commit_on_apply_and_never_clobber_typing(dir.path());
        the_key_row_visibility_follows_the_clicked_preset(dir.path());
        the_test_row_reports_the_latency_against_the_deadline(dir.path());
        the_window_is_freed_after_the_reword_group_has_been_built(dir.path());
        the_window_is_freed_while_a_test_is_in_flight(dir.path());
    }
}
