//! The GTK4 settings window. Task 5 builds the real one.

/// Open (or re-present) the settings window. Must be called on the main
/// thread -- the glib loop in `main` is what guarantees that.
pub fn open() {
    eprintln!("sayd: settings requested (window not built yet)");
}
