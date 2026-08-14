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
use sayd_core::engine::{Command, State};
use sayd_core::handle::EngineHandle;

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
}

impl SaydPlayer {
    pub fn new(engine: EngineHandle) -> Self {
        SaydPlayer { engine }
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
        Ok(self.engine.snapshot().speed as f64)
    }
    async fn set_rate(&self, rate: f64) -> ZResult<()> {
        self.engine.send(Command::SetSpeed(rate as f32));
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
pub async fn spawn(engine: EngineHandle) -> Result<Server<SaydPlayer>, String> {
    Server::new("sayd", SaydPlayer::new(engine))
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
}
