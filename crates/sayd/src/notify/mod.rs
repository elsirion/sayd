//! Speaking desktop notifications.
//!
//! `sayd` watches the session bus rather than owning
//! `org.freedesktop.Notifications`: mako keeps receiving every notification
//! and keeps answering the applications that send them, so nothing here can
//! stop a notification being *displayed* -- only heard.
//!
//! Three layers, each testable without the one above it: `decode` turns a
//! D-Bus message into a `Notification`, `policy` decides whether and how to
//! speak it, and `monitor` owns the connection and the loop. `seen` sits
//! beside them rather than in the chain -- it does not affect whether or
//! what gets spoken, it only remembers which applications have notified and
//! their icon, for the settings window to suggest from.

pub mod decode;
pub mod monitor;
pub mod policy;
pub mod seen;

pub use decode::Notification;

/// Longest `app_name` prefix anything in this module remembers or prints.
///
/// Bounds the cost of one entry, not just the count of them: a name a
/// kilobyte long costs as much to hash, store and print as a short one
/// otherwise, and the freedesktop spec places no length limit on it.
///
/// Shared by `monitor`'s discovery log and `seen`'s registry because the two
/// have the same problem with the same field. `seen` claimed parity with
/// this cap and had only the count half of it (CRITICAL 2), which is the
/// half that does the *least* work: measured with 1 MB names, one `record`
/// held the registry lock for 2.9 ms and one `snapshot` 21.3 ms, 64 entries
/// retained 128 MB, and -- because those names reach a Pango label -- one
/// redraw of a full list cost roughly 11 seconds of frozen main thread.
pub const MAX_APP_NAME_LEN: usize = 256;

/// Truncate `s` to at most `max_chars` characters, on a `char` boundary.
///
/// Plain byte slicing can land inside a multi-byte UTF-8 sequence and panic;
/// `app_name` is attacker-controlled text off the bus (`decode`'s doc
/// comment), so this has to be correct for arbitrary input, not just ASCII.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Truncation is by `char`, not by byte: a multi-byte sequence must
    /// never be cut in half, which would panic rather than shorten.
    #[test]
    fn truncation_lands_on_a_char_boundary() {
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("🙂🙂🙂", 2), "🙂🙂");
    }
}
