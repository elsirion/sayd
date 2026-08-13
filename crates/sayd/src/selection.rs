//! Reading the PRIMARY selection and the clipboard.
//!
//! Uses `wlr-data-control`, which lets a client read selections without
//! holding keyboard focus -- the same mechanism clipboard managers use. That
//! is what allows a sway keybind to be a bare verb: no `$(wl-paste)` command
//! substitution, so no selection text ever passes through a shell and no
//! quoting can mangle it.
//!
//! These calls open their own short-lived Wayland connection and block, so
//! callers on an async runtime must run them on a blocking thread.
//!
//! Nothing in `main` calls this yet -- the D-Bus interface and the `say`
//! CLI that will invoke `read` land in later tasks. Until then the compiler
//! sees an unreached public API in a binary crate, hence the blanket allow
//! below; remove it once a caller exists.
#![allow(dead_code)]

use std::fmt;
use std::io::Read;

use wl_clipboard_rs::paste::{get_contents, ClipboardType, Error, MimeType, Seat};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The mouse-selection buffer. Set by simply selecting text.
    Primary,
    /// The explicit copy buffer.
    Clipboard,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Primary => write!(f, "primary selection"),
            Source::Clipboard => write!(f, "clipboard"),
        }
    }
}

/// How much text we are willing to pull out of a selection.
///
/// A guard against a runaway paste; the engine applies its own `max_chars`
/// limit afterwards, which is the one the user configures.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub fn read(source: Source) -> Result<String, String> {
    let clipboard = match source {
        Source::Primary => ClipboardType::Primary,
        Source::Clipboard => ClipboardType::Regular,
    };

    let (mut reader, _mime) = match get_contents(clipboard, Seat::Unspecified, MimeType::Text) {
        Ok(v) => v,
        Err(Error::ClipboardEmpty) | Err(Error::NoMimeType) => {
            return Err(format!("the {source} is empty"))
        }
        Err(Error::PrimarySelectionUnsupported) => {
            return Err(
                "this compositor does not support the primary selection protocol".to_string()
            )
        }
        Err(Error::MissingProtocol { name, version }) => {
            return Err(format!(
                "this compositor does not support {name} version {version}; \
                 sway 1.9 or newer is required for the primary selection"
            ))
        }
        Err(e) => return Err(format!("could not read the {source}: {e}")),
    };

    let mut buf = String::new();
    reader
        .by_ref()
        .take(MAX_BYTES)
        .read_to_string(&mut buf)
        .map_err(|e| format!("could not read the {source}: {e}"))?;

    if buf.trim().is_empty() {
        return Err(format!("the {source} is empty"));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_are_stable() {
        // These strings appear in D-Bus errors and CLI output.
        assert_eq!(Source::Primary.to_string(), "primary selection");
        assert_eq!(Source::Clipboard.to_string(), "clipboard");
    }

    #[test]
    fn reading_without_a_wayland_display_is_an_error_not_a_panic() {
        // No compositor is reachable in the test environment, so this
        // exercises the failure path. It must return a readable reason.
        let r = read(Source::Primary);
        assert!(r.is_err(), "expected an error with no compositor, got {r:?}");
        let msg = r.unwrap_err();
        assert!(!msg.is_empty());
        assert!(
            msg.chars().next().map(|c| c.is_lowercase() || c.is_numeric()).unwrap_or(false),
            "error messages are sentence fragments, not capitalised: {msg:?}"
        );
    }
}
