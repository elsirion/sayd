//! The settings window: a view of the config file.
//!
//! `model` holds every decision -- what the valid values are, what a change
//! means, what gets written. `window` is only widgets: it reads the model
//! and calls it. That split is not stylistic. The window cannot run without
//! a display, so anything that lives in it cannot be tested here.

pub mod model;
pub mod window;

use std::sync::{Arc, OnceLock};

use sayd_core::handle::EngineHandle;

use model::SettingsModel;

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
