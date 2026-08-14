//! MPRIS2, so media keys, `playerctl` and waybar's mpris module work.
//!
//! A thin mapping onto the same engine the control interface drives. There is
//! no seeking: an utterance is synthesised chunk by chunk and there is no
//! addressable buffer to seek within, so `CanSeek` and `CanGoPrevious` are
//! false and the corresponding methods do nothing rather than lying.
//!
//! `mpris-server`'s `RootInterface` and `PlayerInterface` return
//! `zbus::fdo::Result<T>` for every read (and for the plain actions like
//! `next`/`pause`/`quit`), but the four property *setters* that have a
//! side effect beyond the property itself (`set_fullscreen`,
//! `set_loop_status`, `set_rate`, `set_volume`, `set_shuffle`) return the
//! bare `zbus::Result<T>` instead -- a real split in the vendored trait, not
//! a typo here. `FdoResult`/`ZResult` name the two so each method below uses
//! whichever the trait actually declares.

use mpris_server::zbus::fdo::Result as FdoResult;
use mpris_server::zbus::Result as ZResult;
use mpris_server::{
    LoopStatus, Metadata, PlaybackStatus, PlayerInterface, RootInterface, Server, Time, TrackId,
};
use std::sync::Arc;

use sayd_core::engine::{Command, State};
use sayd_core::handle::EngineHandle;

use crate::config_watch::{persist_in_background, ConfigStore};

/// How much of an utterance to advertise as the track title.
const TITLE_CHARS: usize = 120;

pub fn playback_status_for(state: State) -> PlaybackStatus {
    match state {
        State::Speaking => PlaybackStatus::Playing,
        State::Paused => PlaybackStatus::Paused,
        // Idle and Error are both "nothing is playing". Reporting Playing
        // while errored would make playerctl and waybar lie.
        State::Idle | State::Error => PlaybackStatus::Stopped,
    }
}

pub fn title_for(text: &str) -> String {
    if text.trim().is_empty() {
        return "sayd".into();
    }
    let mut out: String = text.chars().take(TITLE_CHARS).collect();
    if text.chars().count() > TITLE_CHARS {
        out.push('…');
    }
    out
}

/// A per-utterance MPRIS track id, so `mpris:trackid` -- which the spec
/// requires whenever there is a current track -- actually changes between
/// utterances instead of holding one constant placeholder throughout.
/// `Snapshot::current_id` is `0` while nothing is current (see
/// `sayd-core`'s engine.rs); `TrackId::NO_TRACK` is the spec's own value
/// for exactly that case.
fn trackid_for(id: u64) -> TrackId {
    if id == 0 {
        return TrackId::NO_TRACK;
    }
    TrackId::try_from(format!("/sh/sayd/Track{id}")).unwrap_or(TrackId::NO_TRACK)
}

/// Build the `Metadata` the `Metadata` property (and the publish loop's
/// `Property::Metadata` emission) both report, from the same two snapshot
/// fields, so the two never drift the way `tray.rs`'s module doc warns
/// about for its own menu content.
pub fn metadata_for(current_id: u64, current_text: &str) -> Metadata {
    let mut m = Metadata::new();
    m.set_trackid(Some(trackid_for(current_id)));
    m.set_title(Some(title_for(current_text)));
    m
}

pub struct SaydPlayer {
    engine: EngineHandle,
    /// Where `Rate` goes, so a speed set through MPRIS survives the next
    /// config apply -- see `set_rate`.
    store: Arc<ConfigStore>,
}

impl SaydPlayer {
    pub fn new(engine: EngineHandle, store: Arc<ConfigStore>) -> Self {
        SaydPlayer { engine, store }
    }
}

impl RootInterface for SaydPlayer {
    async fn identity(&self) -> FdoResult<String> {
        Ok("sayd".into())
    }
    async fn can_quit(&self) -> FdoResult<bool> {
        Ok(true)
    }
    async fn quit(&self) -> FdoResult<()> {
        self.engine.send(Command::Shutdown);
        Ok(())
    }
    async fn can_raise(&self) -> FdoResult<bool> {
        Ok(false)
    }
    async fn raise(&self) -> FdoResult<()> {
        Ok(())
    }
    async fn has_track_list(&self) -> FdoResult<bool> {
        Ok(false)
    }
    async fn desktop_entry(&self) -> FdoResult<String> {
        Ok("sayd".into())
    }
    async fn supported_uri_schemes(&self) -> FdoResult<Vec<String>> {
        Ok(Vec::new())
    }
    async fn supported_mime_types(&self) -> FdoResult<Vec<String>> {
        Ok(Vec::new())
    }
    async fn fullscreen(&self) -> FdoResult<bool> {
        Ok(false)
    }
    async fn set_fullscreen(&self, _: bool) -> ZResult<()> {
        Ok(())
    }
    async fn can_set_fullscreen(&self) -> FdoResult<bool> {
        Ok(false)
    }
}

impl PlayerInterface for SaydPlayer {
    async fn playback_status(&self) -> FdoResult<PlaybackStatus> {
        Ok(playback_status_for(self.engine.snapshot().state))
    }

    async fn metadata(&self) -> FdoResult<Metadata> {
        let s = self.engine.snapshot();
        Ok(metadata_for(s.current_id, &s.current_text))
    }

    async fn next(&self) -> FdoResult<()> {
        self.engine.send(Command::Next);
        Ok(())
    }
    async fn pause(&self) -> FdoResult<()> {
        self.engine.send(Command::Pause);
        Ok(())
    }
    async fn play(&self) -> FdoResult<()> {
        self.engine.send(Command::Resume);
        Ok(())
    }
    async fn play_pause(&self) -> FdoResult<()> {
        self.engine.send(Command::PlayPause);
        Ok(())
    }
    async fn stop(&self) -> FdoResult<()> {
        self.engine.send(Command::Stop);
        Ok(())
    }

    // No addressable buffer to seek within; advertised false below.
    async fn previous(&self) -> FdoResult<()> {
        Ok(())
    }
    async fn seek(&self, _: Time) -> FdoResult<()> {
        Ok(())
    }
    async fn set_position(&self, _: TrackId, _: Time) -> FdoResult<()> {
        Ok(())
    }
    async fn open_uri(&self, _: String) -> FdoResult<()> {
        Ok(())
    }
    async fn position(&self) -> FdoResult<Time> {
        Ok(Time::ZERO)
    }

    async fn rate(&self) -> FdoResult<f64> {
        // I1: must read `configured_speed`, not `speed`. `speed` (Finding 1)
        // deliberately tracks the *current utterance's* own speed override
        // while one is playing, so a `SetSpeed` issued mid-utterance would
        // not show up here until that utterance finished -- inside the
        // advertised `[0.5, 2.0]` range, so not clamped, just silently not
        // reflected, and indistinguishable from "ignored" to a client.
        // `configured_speed` is what `SetSpeed` writes and is current the
        // instant it lands (see `Snapshot::configured_speed`'s doc comment).
        Ok(self.engine.snapshot().configured_speed as f64)
    }
    /// IMPORTANT 2: persisted, like every other `speed` writer.
    ///
    /// This used to send `Command::SetSpeed` alone, which changed `cfg.speed`
    /// inside the engine and nowhere else -- so the next `ApplyConfig` from
    /// the settings window or a hand edit reverted it, complete with a
    /// `PropertiesChanged` announcing the new value, leaving a client unable
    /// to tell who had changed it. Measured: set `Rate = 1.75`, edit the
    /// config's *voice*, and `Rate` is 1.0 again. `speed` is a config field
    /// the settings window already writes, so the three writers of it should
    /// agree, and the only way for them to agree is the file.
    ///
    /// Off this thread because it writes to disk; the trait's `ZResult`
    /// leaves nowhere to report a failed *write* anyway, and a rate that
    /// could not be written still takes effect (see `ConfigStore::update`).
    async fn set_rate(&self, rate: f64) -> ZResult<()> {
        let rate = rate as f32;
        persist_in_background(self.store.clone(), move |s| s.set_speed(rate));
        Ok(())
    }
    async fn minimum_rate(&self) -> FdoResult<f64> {
        Ok(0.5)
    }
    async fn maximum_rate(&self) -> FdoResult<f64> {
        Ok(2.0)
    }

    // Volume belongs to PipeWire's per-application control, not here.
    async fn volume(&self) -> FdoResult<f64> {
        Ok(1.0)
    }
    async fn set_volume(&self, _: f64) -> ZResult<()> {
        Ok(())
    }

    async fn can_go_next(&self) -> FdoResult<bool> {
        Ok(true)
    }
    async fn can_go_previous(&self) -> FdoResult<bool> {
        Ok(false)
    }
    async fn can_play(&self) -> FdoResult<bool> {
        Ok(true)
    }
    async fn can_pause(&self) -> FdoResult<bool> {
        Ok(true)
    }
    async fn can_seek(&self) -> FdoResult<bool> {
        Ok(false)
    }
    async fn can_control(&self) -> FdoResult<bool> {
        Ok(true)
    }

    async fn loop_status(&self) -> FdoResult<LoopStatus> {
        Ok(LoopStatus::None)
    }
    async fn set_loop_status(&self, _: LoopStatus) -> ZResult<()> {
        Ok(())
    }
    async fn shuffle(&self) -> FdoResult<bool> {
        Ok(false)
    }
    async fn set_shuffle(&self, _: bool) -> ZResult<()> {
        Ok(())
    }
}

/// Register `org.mpris.MediaPlayer2.sayd` on the session bus.
///
/// Fails cleanly (as `Err`, never a panic) rather than aborting startup --
/// as with the tray, the caller is expected to log once and carry on serving
/// the control interface without media-key/playerctl support.
pub async fn spawn(
    engine: EngineHandle,
    store: Arc<ConfigStore>,
) -> Result<Server<SaydPlayer>, String> {
    Server::new("sayd", SaydPlayer::new(engine, store))
        .await
        .map_err(|e| format!("could not start the MPRIS server: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpris_server::PlaybackStatus;
    use sayd_core::engine::State;

    #[test]
    fn playback_status_maps_every_state() {
        assert_eq!(playback_status_for(State::Idle), PlaybackStatus::Stopped);
        assert_eq!(
            playback_status_for(State::Speaking),
            PlaybackStatus::Playing
        );
        assert_eq!(playback_status_for(State::Paused), PlaybackStatus::Paused);
        // An error is not playing; Stopped is the honest answer.
        assert_eq!(playback_status_for(State::Error), PlaybackStatus::Stopped);
    }

    #[test]
    fn a_title_is_produced_from_the_current_text() {
        let t = title_for("the quick brown fox jumps over the lazy dog");
        assert!(t.contains("quick brown fox"), "got {t}");
    }

    #[test]
    fn a_long_title_is_truncated_on_a_character_boundary() {
        let t = title_for(&"日本語".repeat(200));
        assert!(t.chars().count() <= 121, "got {} chars", t.chars().count());
    }

    #[test]
    fn an_empty_utterance_still_yields_a_usable_title() {
        assert!(
            !title_for("").is_empty(),
            "players show an empty title badly"
        );
    }

    #[test]
    fn trackid_is_no_track_while_nothing_is_current() {
        assert_eq!(trackid_for(0), TrackId::NO_TRACK);
    }

    #[test]
    fn trackid_differs_between_utterances() {
        assert_ne!(trackid_for(1), trackid_for(2));
        assert_ne!(trackid_for(1), TrackId::NO_TRACK);
    }

    /// A player over a real engine and a store rooted in `dir`, so
    /// `set_rate` writes somewhere harmless. The store is told the same
    /// config the engine was spawned with, as `ConfigStore::new` requires.
    fn player_in(dir: &std::path::Path) -> (SaydPlayer, EngineHandle) {
        let h = EngineHandle::spawn(
            sayd_core::config::Config::default(),
            Box::new(sayd_core::synth::StubSynthesizer::new()),
            Box::new(sayd_core::audio::VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(ConfigStore::new(
            dir.join("config.toml"),
            h.clone(),
            sayd_core::config::Config::default(),
        ));
        (SaydPlayer::new(h.clone(), store), h)
    }

    /// Poll a `PlayerInterface` read until `f` holds or the deadline passes.
    async fn wait_for<T, F>(mut read: impl FnMut() -> F, f: impl Fn(&T) -> bool) -> T
    where
        F: std::future::Future<Output = FdoResult<T>>,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let v = read().await.expect("read must not error");
            if f(&v) {
                return v;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting");
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn rate_reflects_configured_speed_not_the_playing_utterances_override() {
        // I1, exercised end to end through `SaydPlayer`: a per-utterance
        // speed override must not leak into `Rate`, and a `SetRate` issued
        // while that utterance is still playing must be visible right away.
        let dir = tempfile::tempdir().expect("tempdir");
        let (player, h) = player_in(dir.path());
        h.submit(
            "An utterance with its own speed override.".into(),
            sayd_core::engine::SayOpts {
                speed: Some(1.8),
                ..Default::default()
            },
        )
        .expect("accepted");
        wait_for(
            || player.playback_status(),
            |s| *s == PlaybackStatus::Playing,
        )
        .await;

        let rate = player.rate().await.expect("rate must not error");
        assert_eq!(
            rate,
            sayd_core::config::Config::default().speed as f64,
            "Rate must report the configured default, not the playing utterance's 1.8 override"
        );

        player
            .set_rate(1.75)
            .await
            .expect("set_rate must not error");
        wait_for(|| player.rate(), |r| (*r - 1.75).abs() < 1e-6).await;

        h.shutdown();
    }

    /// IMPORTANT 2: `Rate` used to be engine-only, so any config apply
    /// silently reverted it -- measured, setting `Rate = 1.75` and then
    /// editing the config's *voice* put it back to 1.0, with a
    /// `PropertiesChanged` announcing the reversion, so a client could not
    /// tell who had changed it. `speed` is a config field the settings
    /// window already writes; the three writers of it have to agree, and
    /// the file is the only place they can.
    #[tokio::test]
    async fn a_rate_set_through_mpris_persists_and_survives_a_config_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let (player, h) = player_in(dir.path());
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            h.clone(),
            sayd_core::config::Config::default(),
        ));

        player
            .set_rate(1.75)
            .await
            .expect("set_rate must not error");
        wait_for(|| player.rate(), |r| (*r - 1.75).abs() < 1e-6).await;

        // The file, not just the engine.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (cfg, _) = sayd_core::config::Config::load_from(&path);
            if (cfg.speed - 1.75).abs() < f32::EPSILON {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the rate never reached the file"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // An unrelated hand edit, made the way an editor makes one.
        let (mut edited, err) = sayd_core::config::Config::load_from(&path);
        assert_eq!(err, None);
        edited.voice = "am_fenrir".into();
        edited.save_to(&path).expect("hand edit");
        assert_eq!(
            store.reload(),
            crate::config_watch::ReloadOutcome::Applied,
            "the hand edit must be applied for this to test anything"
        );

        let rate = wait_for(|| player.rate(), |r| (*r - 1.75).abs() < 1e-6).await;
        assert!(
            (rate - 1.75).abs() < 1e-6,
            "an unrelated config change must not revert Rate, got {rate}"
        );
        h.shutdown();
    }

    /// A client is free to ignore `MinimumRate`/`MaximumRate`; the file must
    /// not then hold a speed the engine would only clamp again on the way
    /// back in (finding 9's disagreement, in the one place this path can
    /// still create it).
    #[tokio::test]
    async fn an_out_of_range_rate_is_clamped_before_it_reaches_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let (player, h) = player_in(dir.path());

        player.set_rate(9.0).await.expect("set_rate must not error");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (cfg, _) = sayd_core::config::Config::load_from(&path);
            if (cfg.speed - 2.0).abs() < f32::EPSILON {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the clamped rate never reached the file, or was not clamped"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        h.shutdown();
    }

    #[tokio::test]
    async fn metadata_and_status_show_the_about_to_play_utterance_immediately() {
        // I2, exercised through the real EngineHandle/mpris boundary. Only
        // one utterance is ever submitted, so whether the engine thread has
        // ticked `current` into place yet or is still relying on I2's
        // queue-head fallback, both report the same id/text -- this is
        // deterministic despite running against a real background thread,
        // not a race against it.
        let dir = tempfile::tempdir().expect("tempdir");
        let (player, h) = player_in(dir.path());
        h.submit(
            "The utterance about to play.".into(),
            sayd_core::engine::SayOpts::default(),
        )
        .expect("accepted");

        wait_for(
            || player.playback_status(),
            |s| *s == PlaybackStatus::Playing,
        )
        .await;

        let meta = player.metadata().await.expect("metadata must not error");
        assert_ne!(
            meta.trackid(),
            Some(TrackId::NO_TRACK),
            "must not be NoTrack while Playing"
        );
        let title = meta.title().unwrap_or_default();
        assert!(title.contains("about to play"), "got {title:?}");

        h.shutdown();
    }
}
