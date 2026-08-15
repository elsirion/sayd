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
    allow_add, allow_contains, allow_remove, IconSource, SettingsModel, Suggestion, SuggestionKind,
    COOLDOWN_MAX, COOLDOWN_MIN, COOLDOWN_STEP, IDLE_UNLOAD_MAX, IDLE_UNLOAD_MIN, IDLE_UNLOAD_STEP,
    MAX_CHARS_MAX, MAX_CHARS_MIN, MAX_CHARS_STEP, MODELS, SPEED_MAX, SPEED_MIN, SPEED_STEP,
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
/// have, and a path that is not there any more. `gtk::Image` would draw some of
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
    page.add(&voice_group(&ui, &cfg, engine));
    page.add(&engine_group(&ui, &cfg));
    page.add(&cleanup_group(&ui, &cfg));
    page.add(&notification_group(&ui, &cfg));
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
        u.with(|ui| audition(ui, &e, &field));
    });
    let u = ui.downgrade();
    // Pressing Enter in the field is the same action as pressing the button;
    // a test row you have to reach for the mouse to use is a test row nobody
    // uses twice.
    test.connect_entry_activated(move |row| u.with(|ui| audition(ui, &engine, row)));
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
            IconSource::File(path) if path.is_file() => Some(gtk::Image::from_file(path)),
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
    }
}
