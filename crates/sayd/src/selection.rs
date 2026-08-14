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

use std::fmt;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

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
///
/// The cut is lossy at byte boundaries: if a multi-byte UTF-8 character
/// straddles the limit, it is replaced with U+FFFD (replacement character).
/// Invalid UTF-8 elsewhere in the selection is also replaced lossily.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// How long to wait, after the most recently received byte, for the
/// selection owner to send more before giving up.
///
/// `get_contents` hands back a pipe fed by *another* Wayland client -- the
/// application that currently owns the selection -- not by the compositor.
/// If that client is wedged, or simply never responds, nothing ever arrives
/// on the pipe. An unbounded `read_to_end` against it blocks forever; since
/// `SaySelection`/`SayClipboard` run this on a `spawn_blocking` worker
/// (`dbus.rs`), a stalled owner leaked one blocking-pool thread per attempt
/// -- `say selection`'s own D-Bus timeout only bounds the *caller's* wait,
/// not this read on the daemon's side, so the thread stayed pinned for the
/// life of the process.
///
/// This is measured from the last time *any* bytes arrived, not from the
/// start of the read, so a large but genuinely slow paste keeps extending
/// its own deadline as data trickles in, while an owner that produces
/// nothing at all is cut off after this long.
///
/// That reset-on-any-byte behaviour is also this deadline's blind spot: an
/// owner that writes a single byte every `SELECTION_READ_TIMEOUT` minus a
/// hair never trips it, no matter how long the read has been running.
/// [`SELECTION_READ_OVERALL_CAP`] exists to bound that case; this constant
/// alone only bounds a silent, fully wedged owner.
const SELECTION_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The absolute longest a single selection read may run in total, measured
/// from when the read started -- independent of how recently data arrived.
///
/// [`SELECTION_READ_TIMEOUT`] resets on every byte received, so an owner
/// that dribbles one byte every few seconds forever never trips it: each
/// byte resets the inactivity clock without ever satisfying it, so the read
/// -- and the `spawn_blocking` thread behind it (see that constant's doc
/// comment) -- could otherwise be held open until `MAX_BYTES` at whatever
/// rate the owner feels like. That is not meaningfully different from the
/// unbounded hang this module exists to prevent.
///
/// Set to six times `SELECTION_READ_TIMEOUT`: generous enough that a real,
/// large, honestly-slow transfer -- one that pauses for most of an
/// inactivity window several times over -- still completes, while short
/// enough that a blocking-pool thread can never be pinned for minutes over
/// a single selection read.
const SELECTION_READ_OVERALL_CAP: Duration =
    Duration::from_secs(SELECTION_READ_TIMEOUT.as_secs() * 6);

/// Convert raw bytes to a String, replacing invalid UTF-8 sequences lossily.
fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Put `fd` into non-blocking mode so reads on it return
/// [`io::ErrorKind::WouldBlock`] instead of parking the thread.
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is a valid, open file descriptor for the lifetime of this
    // call -- it comes from `AsRawFd` on a reader we hold by value.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Block until `fd` is readable or `timeout` elapses, whichever comes first.
///
/// Returns `Ok(true)` if the fd became readable, `Ok(false)` if `timeout`
/// elapsed (or a spurious wakeup happened) with nothing to show for it --
/// either way, safe for the caller to just retry the read and let its own
/// deadline bookkeeping decide whether to give up.
fn wait_readable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `pfd` is a single, valid, stack-local `pollfd`; `poll` only
    // reads/writes through the pointer for the duration of the call.
    let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ret < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            // A signal interrupted the wait, not a real timeout. Report
            // "nothing happened" so the caller's own deadline check decides
            // whether that counts as giving up.
            return Ok(false);
        }
        return Err(e);
    }
    Ok(ret > 0)
}

/// Read `reader` to EOF, bounded by two independent deadlines:
///
/// - `inactivity_timeout`: give up if no bytes arrive for this long at a
///   stretch. Reset by every successful read, however small -- see
///   [`SELECTION_READ_TIMEOUT`].
/// - `overall_cap`: give up once this much wall-clock time has passed since
///   the read began, no matter how recently data arrived -- see
///   [`SELECTION_READ_OVERALL_CAP`]. This is what bounds an owner that
///   dribbles data just fast enough to keep resetting the inactivity clock
///   without ever tripping it.
///
/// If `overall_cap` is what ends the read and at least one byte had
/// already arrived, that partial data is returned as `Ok` -- a partial
/// selection is more useful spoken aloud than discarded outright. If
/// nothing at all had arrived by then, it is reported as an error rather
/// than an empty `Ok`, so it cannot be mistaken for a genuinely empty
/// selection by the `buf.trim().is_empty()` check in [`read`].
///
/// Generic over `Read + AsRawFd` rather than named against
/// `wl_clipboard_rs`'s `os_pipe::PipeReader` so this logic can be unit
/// tested against a plain OS pipe, with no Wayland connection involved.
fn read_with_deadline<R: Read + AsRawFd>(
    mut reader: R,
    inactivity_timeout: Duration,
    overall_cap: Duration,
) -> Result<Vec<u8>, String> {
    let fd = reader.as_raw_fd();
    set_nonblocking(fd).map_err(|e| format!("could not prepare to read: {e}"))?;

    let mut bytes = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let start = Instant::now();
    let mut last_progress = start;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(bytes), // EOF: the writer closed its end.
            Ok(n) => {
                bytes.extend_from_slice(&buf[..n]);
                last_progress = Instant::now();
                if bytes.len() as u64 >= MAX_BYTES {
                    bytes.truncate(MAX_BYTES as usize);
                    return Ok(bytes);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let now = Instant::now();

                let inactive_for = now.duration_since(last_progress);
                if inactive_for >= inactivity_timeout {
                    return Err(format!(
                        "the selection owner sent no data for {:.0}s; giving up",
                        inactivity_timeout.as_secs_f64()
                    ));
                }

                let running_for = now.duration_since(start);
                if running_for >= overall_cap {
                    if bytes.is_empty() {
                        return Err(format!(
                            "the selection owner did not send any data within \
                             {:.0}s overall; giving up",
                            overall_cap.as_secs_f64()
                        ));
                    }
                    // Progress kept arriving, just slowly enough to keep
                    // resetting the inactivity clock without ever tripping
                    // it -- the dribble case. Speak what arrived rather
                    // than hold the thread (or the caller) open any
                    // longer.
                    return Ok(bytes);
                }

                // Neither deadline has passed yet; wait for whichever one
                // is closer, then loop back and let the checks above be
                // the single source of truth for whether that counts as
                // giving up.
                let wait_for = (inactivity_timeout - inactive_for).min(overall_cap - running_for);
                if let Err(e) = wait_readable(fd, wait_for) {
                    return Err(format!("could not wait for data: {e}"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("{e}")),
        }
    }
}

pub fn read(source: Source) -> Result<String, String> {
    let clipboard = match source {
        Source::Primary => ClipboardType::Primary,
        Source::Clipboard => ClipboardType::Regular,
    };

    let (reader, _mime) = match get_contents(clipboard, Seat::Unspecified, MimeType::Text) {
        Ok(v) => v,
        Err(Error::ClipboardEmpty) | Err(Error::NoMimeType) => {
            return Err(format!("the {source} is empty"))
        }
        Err(Error::PrimarySelectionUnsupported) => {
            return Err(
                "this compositor does not support the primary selection protocol".to_string(),
            )
        }
        Err(Error::MissingProtocol { name, version }) => {
            return Err(format!(
                "this compositor does not support {name} version {version}; \
                 sway 1.9 or newer is required for the primary selection"
            ))
        }
        Err(e) => {
            let msg = e.to_string();
            let lowercase_msg = if let Some(first_char) = msg.chars().next() {
                format!(
                    "{}{}",
                    first_char.to_lowercase(),
                    &msg[first_char.len_utf8()..]
                )
            } else {
                msg
            };
            return Err(format!("could not read the {source}: {lowercase_msg}"));
        }
    };

    let bytes = read_with_deadline(reader, SELECTION_READ_TIMEOUT, SELECTION_READ_OVERALL_CAP)
        .map_err(|e| format!("could not read the {source}: {e}"))?;

    let buf = bytes_to_string(bytes);

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
        assert!(
            r.is_err(),
            "expected an error with no compositor, got {r:?}"
        );
        let msg = r.unwrap_err();
        assert!(!msg.is_empty());
        assert!(
            msg.chars()
                .next()
                .map(|c| c.is_lowercase() || c.is_numeric())
                .unwrap_or(false),
            "error messages are sentence fragments, not capitalised: {msg:?}"
        );
    }

    #[test]
    fn bytes_to_string_handles_valid_utf8() {
        let bytes = b"hello world".to_vec();
        let s = bytes_to_string(bytes);
        assert_eq!(s, "hello world");
    }

    #[test]
    fn bytes_to_string_handles_multibyte_character_cut_at_boundary() {
        // UTF-8 emoji "🦀" is bytes [0xF0, 0xA4, 0xAD, 0x80].
        // Cut after the first two bytes, leaving an incomplete sequence.
        let mut bytes = b"hello ".to_vec();
        bytes.extend_from_slice(&[0xF0, 0xA4]); // Incomplete emoji
        let s = bytes_to_string(bytes);
        // Should contain the valid prefix and a replacement character for the incomplete sequence.
        assert!(s.contains("hello "));
        assert!(s.contains('\u{FFFD}')); // U+FFFD replacement character
    }

    #[test]
    fn bytes_to_string_handles_invalid_utf8_in_middle() {
        // Mix of valid UTF-8 with an invalid byte sequence in the middle.
        let mut bytes = b"hello ".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE]); // Invalid UTF-8
        bytes.extend_from_slice(b" world");
        let s = bytes_to_string(bytes);
        // Should contain the valid parts with replacement characters for invalid bytes.
        assert!(s.contains("hello"));
        assert!(s.contains("world"));
        assert!(s.contains('\u{FFFD}')); // Replacement characters for invalid bytes
    }

    #[test]
    fn bytes_to_string_handles_empty_input() {
        let bytes = Vec::new();
        let s = bytes_to_string(bytes);
        assert_eq!(s, "");
    }

    /// Finding 1: the ordinary case. A writer that sends everything and
    /// then closes its end must still be read to completion, well within
    /// the deadline.
    #[test]
    fn read_with_deadline_returns_full_contents_when_the_writer_finishes_and_closes() {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let expected = b"hello from the selection owner".to_vec();
        let to_send = expected.clone();
        let writer_thread = std::thread::spawn(move || {
            use std::io::Write;
            writer.write_all(&to_send).expect("write succeeds");
            // Dropping `writer` here closes the write end, which is what
            // signals EOF to the reader side.
        });

        let got = read_with_deadline(reader, Duration::from_secs(5), Duration::from_secs(30))
            .expect("read succeeds");
        writer_thread.join().expect("writer thread does not panic");
        assert_eq!(got, expected);
    }

    /// Finding 1's core case: a selection owner that holds the write end
    /// open but never sends anything -- the hang this deadline exists to
    /// bound. Without it, this read would block forever.
    #[test]
    fn read_with_deadline_times_out_when_the_writer_sends_nothing() {
        let (reader, writer) = std::io::pipe().expect("pipe");

        let start = Instant::now();
        let result = read_with_deadline(reader, Duration::from_millis(200), Duration::from_secs(5));
        let elapsed = start.elapsed();
        drop(writer); // keep the write end alive until here, on purpose

        assert!(result.is_err(), "expected a timeout error, got {result:?}");
        assert!(
            elapsed < Duration::from_secs(2),
            "read_with_deadline did not bound the wait: took {elapsed:?}"
        );
    }

    /// A slow-but-alive writer must not be cut off by a fixed overall
    /// deadline: each chunk that arrives resets the inactivity clock, so a
    /// transfer that takes longer than any single per-chunk gap -- but never
    /// stalls for that long at a stretch -- still completes.
    #[test]
    fn read_with_deadline_survives_a_slow_writer_that_keeps_making_progress() {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let writer_thread = std::thread::spawn(move || {
            use std::io::Write;
            for chunk in [b"one ", b"two ", b"thre"] {
                writer.write_all(chunk).expect("write succeeds");
                std::thread::sleep(Duration::from_millis(100));
            }
            // Drop here closes the write end, signalling EOF.
        });

        // Each gap between chunks (100ms) is well inside the 500ms
        // inactivity deadline, even though the whole transfer (~300ms+)
        // exceeds it. The overall cap is set generously (5s) so it plays
        // no part here -- that is the next test's job.
        let got = read_with_deadline(reader, Duration::from_millis(500), Duration::from_secs(5))
            .expect("read succeeds");
        writer_thread.join().expect("writer thread does not panic");
        assert_eq!(got, b"one two thre".to_vec());
    }

    /// Finding: the gap this change closes. A writer that dribbles data
    /// steadily enough to keep resetting the inactivity clock -- never
    /// stalling for a whole `inactivity_timeout` at a stretch -- must still
    /// be cut off once `overall_cap` elapses. Without the overall cap, this
    /// pattern is unbounded: it can hold the read (and the blocking-pool
    /// thread behind it) open indefinitely, which is not meaningfully
    /// different from the silent-hang case the inactivity deadline alone
    /// was meant to fix.
    ///
    /// Both deadlines are scaled down (milliseconds, not the real
    /// 5s/30s constants) so the test proves the shape of the behaviour
    /// without waiting for it in real time.
    #[test]
    fn read_with_deadline_is_cut_off_by_the_overall_cap_despite_dribbling_progress() {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let inactivity_timeout = Duration::from_millis(150);
        let overall_cap = Duration::from_millis(400);

        let writer_thread = std::thread::spawn(move || {
            use std::io::Write;
            // One byte every 50ms: comfortably inside the 150ms inactivity
            // deadline (so it never trips), but the loop runs far longer
            // than the 400ms overall cap if left uninterrupted.
            //
            // Once the overall cap fires, `read_with_deadline` drops the
            // reader and closes its end of the pipe (see the module's
            // Drop-based fd cleanup); any write after that legitimately
            // fails with a broken-pipe error. That is expected, not a bug
            // in the writer, so it just stops instead of panicking.
            for _ in 0..40 {
                if writer.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let start = Instant::now();
        let result = read_with_deadline(reader, inactivity_timeout, overall_cap);
        let elapsed = start.elapsed();
        writer_thread.join().expect("writer thread does not panic");

        let bytes = result.expect("partial data is returned rather than an error");
        assert!(
            !bytes.is_empty(),
            "some bytes should have trickled in before the cap fired"
        );
        assert!(
            bytes.len() < 40,
            "the writer's full run should have been cut short: got {} bytes",
            bytes.len()
        );
        assert!(
            elapsed >= overall_cap,
            "returned before the overall cap even elapsed: took {elapsed:?}"
        );
        assert!(
            elapsed < overall_cap + Duration::from_millis(300),
            "the overall cap did not bound the wait: took {elapsed:?}"
        );
    }
}
