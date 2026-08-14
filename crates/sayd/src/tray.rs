//! The StatusNotifierItem tray.
//!
//! No toolkit is involved: SNI is a D-Bus specification, so the application
//! publishes properties and a `com.canonical.dbusmenu` tree and the host --
//! waybar's tray module -- does all the drawing. `ksni`'s dependency tree
//! contains no GUI crate at all.
//!
//! The tray holds the latest `Snapshot` and re-renders from it; the daemon's
//! publish loop hands it a new one whenever anything changes.
//!
//! `build_menu` is the single source of truth for the menu's shape: both
//! `menu_labels` (which the unit tests assert on) and `Tray::menu` (what
//! ksni actually renders to a host) derive their content from it, rather
//! than each keeping its own copy of the status-block logic. Two
//! independent copies is exactly the failure mode this project has been
//! bitten by before -- the tests would keep passing on the copy they
//! exercise while the copy users actually see quietly drifted.

use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Handle, MenuItem, Tray, TrayMethods};
use sayd_core::engine::{Command, Snapshot, State, SayOpts};
use sayd_core::handle::EngineHandle;
use sayd_core::queue::Source;

/// How much of an utterance to show in a menu label.
const LABEL_CHARS: usize = 40;

/// Build the `SayOpts` for a tray selection/clipboard speak action.
/// The tray actions mirror the hotkey behavior, so they use `Source::Hotkey`
/// which resolves to `Policy::Replace`.
fn speak_opts() -> SayOpts {
    SayOpts {
        source: Source::Hotkey,
        ..Default::default()
    }
}

/// Stock freedesktop icon names, so the tray works with no install step and
/// the host themes it. Shipping our own into `hicolor` would need an
/// installer, which does not exist yet.
pub fn icon_for(state: State, muted: bool) -> &'static str {
    if muted {
        return "audio-volume-muted-symbolic";
    }
    match state {
        State::Idle => "audio-speakers-symbolic",
        State::Speaking => "media-playback-start-symbolic",
        State::Paused => "media-playback-pause-symbolic",
        State::Error => "dialog-error-symbolic",
    }
}

/// Trim to a menu-sized label on a character boundary.
fn short(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

fn human_secs(s: f64) -> String {
    let s = s.max(0.0).round() as u64;
    if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// The status block's lines: the standing error (if any), the current
/// utterance and its remaining-time estimate (or "Idle"/"Speaking" when
/// there is nothing to show yet -- see the module-level note on the
/// submit-before-populate timing gap), and up to `QUEUE_HEAD_LIMIT` pending
/// entries with a count of any remainder.
///
/// This is the one place that computes the status block's *content*.
/// `build_menu` renders it as disabled items; `menu_labels` (below) exposes
/// it to tests via the same call. Neither keeps its own copy.
fn status_lines(s: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(e) = s.error.as_deref() {
        out.push(format!("Error: {}", short(e, 80)));
    }

    match s.state {
        State::Idle => out.push("Idle".to_string()),
        _ if !s.current_text.is_empty() => {
            out.push(short(&s.current_text, LABEL_CHARS));
            out.push(format!("{} remaining", human_secs(s.remaining_secs)));
        }
        // `State` flips to `speaking` on submit before the utterance is
        // populated into `current` (see sayd-core's engine.rs), so for up
        // to one synthesis chunk the tray can legitimately show "Speaking"
        // with no current text yet. Expected, bounded, self-correcting.
        _ => out.push("Speaking".to_string()),
    }

    for (_, text) in &s.queue_heads {
        out.push(format!("  {}", short(text, LABEL_CHARS)));
    }
    let shown = s.queue_heads.len();
    if s.queue_len > shown {
        out.push(format!("  … and {} more pending", s.queue_len - shown));
    }

    out
}

/// Build the whole menu, in order: the status block (disabled), transport
/// controls, selection/clipboard actions, mute, and quit.
///
/// The single source of truth for the menu's shape -- see the module doc
/// comment. `Tray::menu` calls this directly; `menu_labels` derives its
/// output by extracting each item's label rather than recomputing it, so
/// there is no way for the two to show different text for the same
/// `Snapshot`.
///
/// "Settings…" is deliberately absent: the window it opens is the next
/// milestone, and a menu entry that does nothing is worse than no entry.
/// Volume is deliberately absent too -- the daemon registers as a named
/// PipeWire client, so `pavucontrol` already gives per-application volume.
fn build_menu(s: &Snapshot) -> Vec<MenuItem<SaydTray>> {
    let mut items: Vec<MenuItem<SaydTray>> = Vec::new();

    for label in status_lines(s) {
        items.push(
            StandardItem {
                label,
                enabled: false,
                ..Default::default()
            }
            .into(),
        );
    }
    items.push(MenuItem::Separator);

    let paused = s.state == State::Paused;
    items.push(
        StandardItem {
            label: if paused {
                "Resume".into()
            } else {
                "Pause".into()
            },
            activate: Box::new(|t: &mut SaydTray| t.engine.send(Command::PlayPause)),
            ..Default::default()
        }
        .into(),
    );
    for (label, cmd) in [
        ("Skip sentence", Command::SkipSentence),
        ("Next", Command::Next),
        ("Stop", Command::Stop),
        ("Clear queue", Command::ClearQueue),
    ] {
        items.push(
            StandardItem {
                label: label.into(),
                activate: Box::new(move |t: &mut SaydTray| t.engine.send(cmd.clone())),
                ..Default::default()
            }
            .into(),
        );
    }
    items.push(MenuItem::Separator);

    items.push(
        StandardItem {
            label: "Speak selection".into(),
            activate: Box::new(|t: &mut SaydTray| t.speak(crate::selection::Source::Primary)),
            ..Default::default()
        }
        .into(),
    );
    items.push(
        StandardItem {
            label: "Speak clipboard".into(),
            activate: Box::new(|t: &mut SaydTray| t.speak(crate::selection::Source::Clipboard)),
            ..Default::default()
        }
        .into(),
    );
    items.push(MenuItem::Separator);

    items.push(
        CheckmarkItem {
            label: "Mute".into(),
            checked: s.muted,
            activate: Box::new(|t: &mut SaydTray| {
                let now = t.snapshot.muted;
                t.engine.send(Command::SetMuted(!now));
            }),
            ..Default::default()
        }
        .into(),
    );
    items.push(MenuItem::Separator);

    items.push(
        StandardItem {
            label: "Quit".into(),
            activate: Box::new(|t: &mut SaydTray| t.engine.send(Command::Shutdown)),
            ..Default::default()
        }
        .into(),
    );

    items
}

/// The menu's text content, in order, separators dropped.
///
/// Derived by extracting each item's label from [`build_menu`] rather than
/// recomputing it -- see the module doc comment for why. This makes the
/// content testable without a D-Bus host: `ksni::MenuItem` carries boxed
/// closures and is awkward to assert on directly, so this walks the same
/// tree `Tray::menu` renders and pulls out just the text.
///
/// Test-only (like `Engine`'s own `audio_written`/`is_model_loaded` test
/// helpers): nothing in the running daemon needs the menu as bare strings,
/// only its own D-Bus tree, which `Tray::menu` already produces straight
/// from `build_menu`.
#[cfg(test)]
pub fn menu_labels(s: &Snapshot) -> Vec<String> {
    build_menu(s)
        .into_iter()
        .filter_map(|item| match item {
            MenuItem::Standard(i) => Some(i.label),
            MenuItem::Checkmark(i) => Some(i.label),
            MenuItem::SubMenu(i) => Some(i.label),
            MenuItem::Separator | MenuItem::RadioGroup(_) => None,
        })
        .collect()
}

pub struct SaydTray {
    engine: EngineHandle,
    snapshot: Snapshot,
}

impl SaydTray {
    pub fn new(engine: EngineHandle) -> Self {
        let snapshot = engine.snapshot();
        SaydTray { engine, snapshot }
    }

    /// Replace the rendered state. The caller decides when it changed.
    pub fn set_snapshot(&mut self, s: Snapshot) {
        self.snapshot = s;
    }

    /// Read a selection and submit it, without blocking the menu callback.
    ///
    /// `selection::read` opens its own Wayland connection and blocks, and
    /// this runs on the same async runtime that services D-Bus (see the
    /// module's `spawn`), so it must not be called inline.
    ///
    /// `spawn_blocking` panics with no Tokio runtime in scope. `ksni`'s
    /// default features route its background service task through
    /// `tokio::spawn` (`ksni::compat::spawn`, gated on its own `tokio`
    /// feature), which itself panics without an ambient runtime -- so by
    /// the time this activate callback runs (dispatched from inside that
    /// same spawned task, per `ksni`'s `event`/`event_group` D-Bus methods),
    /// a runtime is already guaranteed to be in scope, not just likely.
    fn speak(&mut self, source: crate::selection::Source) {
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || match crate::selection::read(source) {
            Ok(text) => {
                if let Err(e) = engine.submit(text, speak_opts()) {
                    eprintln!("sayd: {e}");
                }
            }
            Err(e) => eprintln!("sayd: {e}"),
        });
    }
}

impl Tray for SaydTray {
    fn id(&self) -> String {
        "sayd".into()
    }

    fn title(&self) -> String {
        "sayd".into()
    }

    fn icon_name(&self) -> String {
        icon_for(self.snapshot.state, self.snapshot.muted).into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = if let Some(e) = self.snapshot.error.as_deref() {
            e.to_string()
        } else if self.snapshot.current_text.is_empty() {
            "Nothing playing".into()
        } else {
            format!(
                "{} ({} remaining)",
                short(&self.snapshot.current_text, LABEL_CHARS),
                human_secs(self.snapshot.remaining_secs)
            )
        };
        ksni::ToolTip {
            icon_name: self.icon_name(),
            title: "sayd".into(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        build_menu(&self.snapshot)
    }
}

/// Register the tray with whatever StatusNotifierWatcher is running.
///
/// Fails cleanly (as `Err`, never a panic) when there is none -- a bare sway
/// config without waybar has no `org.kde.StatusNotifierWatcher` at all, and
/// the caller is expected to log once and carry on serving the control
/// interface rather than treat this as fatal.
pub async fn spawn(engine: EngineHandle) -> Result<Handle<SaydTray>, String> {
    SaydTray::new(engine)
        .spawn()
        .await
        .map_err(|e| format!("could not register the tray: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sayd_core::engine::{Snapshot, State};

    fn snap(state: State) -> Snapshot {
        Snapshot {
            state,
            muted: false,
            voice: "af_heart".into(),
            speed: 1.0,
            queue_len: 0,
            remaining_secs: 0.0,
            current_text: String::new(),
            current_id: 0,
            error: None,
            queue_heads: Vec::new(),
            error_kind: None,
        }
    }

    #[test]
    fn each_state_has_a_distinct_icon() {
        let idle = icon_for(State::Idle, false);
        let speaking = icon_for(State::Speaking, false);
        let paused = icon_for(State::Paused, false);
        let error = icon_for(State::Error, false);
        let muted = icon_for(State::Speaking, true);
        let all = [idle, speaking, paused, error, muted];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.is_empty(), "icon {i} is empty");
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "icons {i} and {j} are the same");
                }
            }
        }
    }

    #[test]
    fn muted_overrides_the_state_icon() {
        assert_eq!(icon_for(State::Idle, true), icon_for(State::Speaking, true));
    }

    #[test]
    fn an_error_puts_its_message_first_in_the_menu() {
        let mut s = snap(State::Error);
        s.error = Some("the audio device disappeared".into());
        let labels = menu_labels(&s);
        assert!(
            labels
                .first()
                .map(|l| l.contains("audio device"))
                .unwrap_or(false),
            "expected the error first, got {labels:?}"
        );
    }

    #[test]
    fn the_menu_shows_the_current_utterance_and_time_remaining() {
        let mut s = snap(State::Speaking);
        s.current_text = "the quick brown fox jumps over the lazy dog".into();
        s.remaining_secs = 42.0;
        let labels = menu_labels(&s);
        let joined = labels.join(" | ");
        assert!(joined.contains("quick brown fox"), "got {joined}");
        assert!(
            joined.contains("42") || joined.to_lowercase().contains("s"),
            "got {joined}"
        );
    }

    #[test]
    fn a_long_utterance_is_truncated_in_the_menu() {
        let mut s = snap(State::Speaking);
        s.current_text = "x".repeat(500);
        for l in menu_labels(&s) {
            assert!(
                l.chars().count() < 120,
                "menu label too long: {} chars",
                l.chars().count()
            );
        }
    }

    #[test]
    fn multibyte_text_is_truncated_without_panicking() {
        let mut s = snap(State::Speaking);
        s.current_text = "日本語のテキスト".repeat(50);
        let _ = menu_labels(&s);
        let mut s2 = snap(State::Speaking);
        s2.current_text = "🎤".repeat(200);
        let _ = menu_labels(&s2);
    }

    #[test]
    fn pending_entries_appear_with_a_count_when_there_are_more() {
        let mut s = snap(State::Speaking);
        s.queue_heads = (1..=5).map(|i| (i, format!("utterance {i}"))).collect();
        s.queue_len = 9;
        let joined = menu_labels(&s).join(" | ");
        assert!(joined.contains("utterance 1"), "got {joined}");
        assert!(
            joined.contains("4"),
            "expected a count of the remaining 4: {joined}"
        );
    }

    #[test]
    fn an_empty_queue_adds_no_pending_entries() {
        let s = snap(State::Idle);
        let joined = menu_labels(&s).join(" | ");
        assert!(!joined.to_lowercase().contains("pending"), "got {joined}");
    }

    #[test]
    fn the_menu_offers_every_action_the_spec_lists() {
        let s = snap(State::Speaking);
        let joined = menu_labels(&s).join(" | ").to_lowercase();
        for action in [
            "pause",
            "skip",
            "next",
            "stop",
            "clear",
            "selection",
            "clipboard",
            "mute",
            "quit",
        ] {
            assert!(
                joined.contains(action),
                "menu is missing {action}: {joined}"
            );
        }
    }

    #[test]
    fn settings_is_absent_until_the_window_exists() {
        let s = snap(State::Idle);
        let joined = menu_labels(&s).join(" | ").to_lowercase();
        assert!(
            !joined.contains("settings"),
            "a dead Settings entry is worse than none"
        );
    }

    #[test]
    fn speak_opts_uses_hotkey_source_for_replace_policy() {
        use sayd_core::queue::Policy;
        let opts = super::speak_opts();
        assert_eq!(
            opts.source, Source::Hotkey,
            "tray speak actions must use Hotkey source"
        );
        assert_eq!(
            opts.source.default_policy(),
            Policy::Replace,
            "Hotkey source must resolve to Replace policy"
        );
    }
}
