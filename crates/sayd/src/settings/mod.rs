//! The settings window: a view of the config file.
//!
//! `model` holds every decision -- what the valid values are, what a change
//! means, what gets written. `window` is only widgets: it reads the model
//! and calls it. That split is not stylistic. The window cannot run without
//! a display, so anything that lives in it cannot be tested here.

pub mod model;
pub mod window;
