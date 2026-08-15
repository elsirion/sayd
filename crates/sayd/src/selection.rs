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

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
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

/// Lowercase the first character of a message so it reads as a sentence
/// fragment after our own `could not read the ...: ` prefix.
///
/// The libraries under this module capitalise their messages; we splice them
/// into the middle of ours.
fn lowercase_first(msg: String) -> String {
    match msg.chars().next() {
        Some(first) => format!("{}{}", first.to_lowercase(), &msg[first.len_utf8()..]),
        None => msg,
    }
}

/// Where a Wayland client looks for the compositor socket, given the two
/// environment variables that decide it.
///
/// Mirrors `wayland_client::Connection::connect_to_env`: `WAYLAND_DISPLAY` is
/// used as-is when absolute, and is otherwise resolved against
/// `XDG_RUNTIME_DIR`. `None` means the environment does not name a socket at
/// all.
fn compositor_socket_path(
    display: Option<&OsString>,
    runtime_dir: Option<&OsString>,
) -> Option<PathBuf> {
    let display = PathBuf::from(display?);
    if display.is_absolute() {
        return Some(display);
    }
    let mut path = PathBuf::from(runtime_dir?);
    if !path.is_absolute() {
        return None;
    }
    path.push(display);
    Some(path)
}

/// Explain a failed compositor connection in terms of what the operator can
/// actually change.
///
/// `wl-clipboard-rs` reports every connection failure as the same sentence --
/// "Couldn't connect to the Wayland compositor" -- whether the socket is
/// missing, the environment never named one, or the client library could not
/// be loaded. That is indistinguishable in a log, and the most common cause by
/// far is the one this spells out: a `sayd` started outside the graphical
/// session (a systemd user unit without the session environment imported, a
/// bare TTY, an ssh shell) has no `WAYLAND_DISPLAY`, so there is nothing to
/// connect to no matter how healthy the compositor is.
///
/// Takes the environment as arguments rather than reading it, so the tests can
/// cover every branch without mutating process-global state.
fn describe_wayland_env(display: Option<&OsString>, runtime_dir: Option<&OsString>) -> String {
    let Some(display) = display else {
        return "WAYLAND_DISPLAY is not set in sayd's environment, so no compositor \
                could be found. sayd has to run inside the graphical session: start it \
                from the sway config with `exec sayd`, or, from a systemd user unit, \
                import the session environment first (see \
                docs/sh.sayd.Sayd.service.example)"
            .to_string();
    };

    match compositor_socket_path(Some(display), runtime_dir) {
        Some(path) => format!(
            "nothing is listening on {}, the socket WAYLAND_DISPLAY={} names",
            path.display(),
            Path::new(display).display()
        ),
        None => format!(
            "WAYLAND_DISPLAY={} is a relative socket name and XDG_RUNTIME_DIR is not \
             set to an absolute path, so it cannot be resolved to a socket",
            Path::new(display).display()
        ),
    }
}

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

/// Read `source`, or in a test whatever [`test_seam`] has installed.
///
/// IMPORTANT 4: `dbus::SaydIface::say_selection` and `say_clipboard` could
/// not be driven from a test at all -- they end in a Wayland connection, and
/// there is no compositor in a test run -- so the rewrite step in both of
/// them was pinned by nothing and deleting it passed the whole suite. The
/// seam is one branch, compiled only into the test binary, and it is here
/// rather than as an injected reader on `SaydIface` because the thing under
/// test is those two D-Bus methods by name: a reader threaded through the
/// struct would let `say_selection` stop calling this and still pass.
pub fn read(source: Source) -> Result<String, String> {
    #[cfg(test)]
    if let Some(canned) = test_seam::installed(source) {
        return canned;
    }
    read_from_compositor(source)
}

/// A canned [`read`], for the tests that drive `SaySelection` and
/// `SayClipboard` end to end.
///
/// Process-global, because `read` is called on a `spawn_blocking` thread and
/// a thread-local would never be seen there. [`install`] therefore also holds
/// a mutex for the lifetime of its guard, so two tests that install one
/// cannot overlap; every other test in this binary is unaffected, because an
/// empty slot is exactly today's behaviour.
///
/// [`install`]: test_seam::install
#[cfg(test)]
pub(crate) mod test_seam {
    use super::Source;
    use std::sync::{Mutex, MutexGuard};

    type Reader = Box<dyn Fn(Source) -> Result<String, String> + Send + Sync>;

    static INSTALLED: Mutex<Option<Reader>> = Mutex::new(None);
    /// Held by [`Installed`], so the tests that install a reader run one at a
    /// time.
    static EXCLUSIVE: Mutex<()> = Mutex::new(());

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        match m.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The installed reader's answer for `source`, or `None` when the real
    /// one should run.
    pub(super) fn installed(source: Source) -> Option<Result<String, String>> {
        lock(&INSTALLED).as_ref().map(|f| f(source))
    }

    /// What `read` returns until the returned guard is dropped.
    pub(crate) fn install(
        f: impl Fn(Source) -> Result<String, String> + Send + Sync + 'static,
    ) -> Installed {
        let exclusive = lock(&EXCLUSIVE);
        *lock(&INSTALLED) = Some(Box::new(f));
        Installed(exclusive)
    }

    /// Uninstalls on drop, so a panicking test cannot leave a canned
    /// selection behind for the rest of the run.
    pub(crate) struct Installed(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl Drop for Installed {
        fn drop(&mut self) {
            *lock(&INSTALLED) = None;
        }
    }
}

fn read_from_compositor(source: Source) -> Result<String, String> {
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
        Err(Error::WaylandConnection(cause)) => {
            // The one failure worth diagnosing rather than just reporting:
            // see `describe_wayland_env`. `cause` is printed too, since it is
            // the library's own verdict and distinguishes a missing socket
            // from a client library that would not load.
            return Err(format!(
                "could not read the {source}: {} -- {}",
                lowercase_first(cause.to_string()),
                describe_wayland_env(
                    env::var_os("WAYLAND_DISPLAY").as_ref(),
                    env::var_os("XDG_RUNTIME_DIR").as_ref(),
                )
            ));
        }
        Err(e) => {
            return Err(format!(
                "could not read the {source}: {}",
                lowercase_first(e.to_string())
            ));
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

    /// End-to-end: `read` must never panic, whichever of its two legitimate
    /// outcomes fires. A sandbox with no compositor reachable exercises the
    /// error path below; a developer's real session -- the only kind of
    /// machine this daemon is actually for -- has a live compositor, and a
    /// successful read (of an empty selection or of real text) is exactly as
    /// valid an outcome there as an error is in the sandbox. This used to
    /// assert `is_err()` unconditionally, which is backwards: it passed only
    /// on the machines `sayd` is *not* for and failed on the ones it is,
    /// which is how a real desktop session got mistaken for a regression.
    ///
    /// Message-content assertions only make sense once there is a message,
    /// so they run only on the error path and are skipped entirely on
    /// success.
    ///
    /// Deliberately does not force one branch or the other by mutating
    /// `WAYLAND_DISPLAY`: `std::env::set_var` is process-global, and every
    /// other test in this binary would be racing it. The pure helpers
    /// `describe_wayland_env` and `compositor_socket_path` above already
    /// cover every shape that message can take, deterministically, against
    /// explicit arguments rather than the real environment -- this test's
    /// only remaining job is the thing they cannot cover: that the real,
    /// end-to-end path through `wl_clipboard_rs::paste::get_contents` never
    /// panics on either kind of machine, and produces a sane message on the
    /// ones where it fails.
    #[test]
    fn reading_the_primary_selection_never_panics() {
        let r = read(Source::Primary);
        let Err(msg) = r else {
            // A live compositor answered -- the case this daemon exists
            // for. Nothing further to check: whether the selection was
            // empty or held text is not this test's concern.
            return;
        };

        assert!(!msg.is_empty());
        assert!(
            msg.chars()
                .next()
                .map(|c| c.is_lowercase() || c.is_numeric())
                .unwrap_or(false),
            "error messages are sentence fragments, not capitalised: {msg:?}"
        );

        // A *connection* failure -- no compositor reachable at all, which
        // is the only failure a sandbox with no `WAYLAND_DISPLAY` can ever
        // produce -- is the one whose message `read` builds from two halves
        // joined by " -- " (see its `Error::WaylandConnection` arm): the
        // library's own cause, then `describe_wayland_env`'s verdict. No
        // other failure `read` returns (an empty selection, an unsupported
        // protocol, a stalled selection owner) joins two clauses that way,
        // so this is how the test tells "the compositor could not even be
        // reached" apart from every other reason a read can fail -- without
        // needing `read` to expose which `Error` variant it saw.
        if msg.contains(" -- ") {
            assert!(
                msg.contains("WAYLAND_DISPLAY"),
                "a connection failure must say what environment it looked at: {msg:?}"
            );
        }
    }

    /// The failure this diagnosis exists for: a daemon started outside the
    /// graphical session. The library's own message is the same one it gives
    /// for a dead socket, so the environment has to be named explicitly.
    #[test]
    fn no_wayland_display_is_reported_as_the_environment_problem_it_is() {
        let msg = describe_wayland_env(None, Some(&OsString::from("/run/user/1000")));
        assert!(
            msg.contains("WAYLAND_DISPLAY is not set"),
            "the missing variable must be named: {msg:?}"
        );
        assert!(
            msg.contains("systemd") && msg.contains("exec sayd"),
            "both ways of starting sayd inside the session should be pointed at: {msg:?}"
        );
    }

    /// The other common case: the environment is fine, the socket is not.
    /// Naming the path is what lets an operator check it.
    #[test]
    fn a_named_but_dead_socket_is_reported_with_its_resolved_path() {
        let msg = describe_wayland_env(
            Some(&OsString::from("wayland-1")),
            Some(&OsString::from("/run/user/1000")),
        );
        assert!(
            msg.contains("/run/user/1000/wayland-1"),
            "the resolved socket path must appear: {msg:?}"
        );
        assert!(
            !msg.contains("WAYLAND_DISPLAY is not set"),
            "this is not the unset case: {msg:?}"
        );
    }

    /// `WAYLAND_DISPLAY` may be an absolute path, in which case
    /// `XDG_RUNTIME_DIR` plays no part -- matching `connect_to_env`.
    #[test]
    fn an_absolute_wayland_display_is_used_as_the_socket_path_directly() {
        let path = compositor_socket_path(
            Some(&OsString::from("/tmp/sway-socket")),
            Some(&OsString::from("/run/user/1000")),
        );
        assert_eq!(path, Some(PathBuf::from("/tmp/sway-socket")));
    }

    /// A relative socket name with no runtime dir to resolve it against
    /// cannot name a path at all; saying so beats printing a bare "wayland-1".
    #[test]
    fn a_relative_socket_name_without_a_runtime_dir_cannot_be_resolved() {
        assert_eq!(
            compositor_socket_path(Some(&OsString::from("wayland-1")), None),
            None
        );
        let msg = describe_wayland_env(Some(&OsString::from("wayland-1")), None);
        assert!(
            msg.contains("XDG_RUNTIME_DIR"),
            "the variable that would fix it must be named: {msg:?}"
        );
    }

    #[test]
    fn lowercase_first_leaves_an_empty_message_alone() {
        assert_eq!(lowercase_first(String::new()), "");
        assert_eq!(lowercase_first("Couldn't".to_string()), "couldn't");
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
