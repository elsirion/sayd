//! The settings window: a view of the config file.
//!
//! `model` holds every decision -- what the valid values are, what a change
//! means, what gets written. `window` is only widgets: it reads the model
//! and calls it. That split is not stylistic. The window cannot run without
//! a display, so anything that lives in it cannot be tested here.

pub mod model;
pub mod window;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sayd_core::handle::EngineHandle;

use model::SettingsModel;

/// How long the shutdown path waits for a pending settings edit to reach
/// disk before giving up on it.
///
/// The flush this bounds skips `SettingsModel`'s own 250ms debounce, so this
/// only has to cover `ConfigStore::save`'s actual disk I/O -- a temp-write
/// and a rename of a small TOML file -- not the debounce window on top of
/// it. Generous anyway: shutdown is the one moment a slightly slower exit is
/// clearly the lesser cost against losing a change the user was shown as
/// saved.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// What the window is built against: the model it edits, and the engine its
/// Test row auditions the current settings through.
///
/// One `OnceLock` over the pair rather than one each. The window needs both
/// or neither, and two locks set one after the other could be observed
/// half-populated by a settings request that arrives in between -- an
/// unlikely race, but one with no upside to leaving open.
///
/// A static at all because the tray's menu callbacks are built deep inside
/// `ksni`'s tree and the window is built and destroyed repeatedly over the
/// daemon's life, so neither can own these.
struct Host {
    model: Arc<SettingsModel>,
    engine: EngineHandle,
}

static HOST: OnceLock<Host> = OnceLock::new();

/// Hand the settings layer what it needs. Called once, at startup.
///
/// A second call is ignored rather than treated as an error: there is only
/// ever one daemon, and a `OnceLock` that has already been set is already
/// holding the right thing.
pub fn install(model: Arc<SettingsModel>, engine: EngineHandle) {
    let _ = HOST.set(Host { model, engine });
}

/// The model and engine, or `None` if a settings request beat startup to it.
///
/// The caller is expected to say so and carry on: a window that cannot be
/// built is not a reason to bring the daemon down.
fn host() -> Option<(Arc<SettingsModel>, EngineHandle)> {
    HOST.get().map(|h| (h.model.clone(), h.engine.clone()))
}

/// Flush a settings edit still owed to disk, if any, before the daemon
/// exits.
///
/// Called once, from `run_daemon`'s single shutdown path (`main.rs`) --
/// `SettingsModel` lives in `HOST` for the process's whole life and so is
/// never otherwise dropped in production, which is exactly what its own
/// `Drop` impl (see `SettingsModel::flush`'s doc comment) exists to cover
/// for tests. Without this call, a settings change made in the last 250ms
/// before SIGTERM/`Quit()`/`say quit` -- already shown to the user, in the
/// window, as applied -- would simply never reach the file.
///
/// A no-op if `install` was never reached: that only happens when
/// `run_daemon` returns early from a startup failure, in which case there is
/// no window that could have shown anyone an edit as applied, and nothing to
/// flush.
pub fn flush_pending() {
    let Some(host) = HOST.get() else { return };
    if let Err(e) = host.model.flush(FLUSH_TIMEOUT) {
        eprintln!("warning: a settings change made just before shutdown was lost: {e}");
    }
}
