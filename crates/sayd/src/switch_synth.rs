//! One `Synthesizer` over two engines, Kokoro and Pocket TTS, so the config's
//! `engine` setting can change without a restart.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sayd_core::config::Config;
use sayd_core::synth::Synthesizer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveEngine {
    Kokoro,
    Pocket,
}

impl ActiveEngine {
    fn of(cfg: &Config) -> ActiveEngine {
        if cfg.pocket_active() {
            ActiveEngine::Pocket
        } else {
            ActiveEngine::Kokoro
        }
    }

    fn label(self) -> &'static str {
        match self {
            ActiveEngine::Kokoro => "Kokoro",
            ActiveEngine::Pocket => "Pocket TTS",
        }
    }
}

/// Holds both engines but only ever loads the active one.
///
/// A switch is never immediate. `reconfigure` records it as `pending` and
/// asks for an unload; `Engine` performs that unload only once no utterance
/// is in flight (`sayd-core`'s `ApplyConfig`, IMPORTANT 1), and `unload` is
/// where `pending` becomes `active`. So every chunk of one utterance --
/// `refit`'s phonemize/fits at pop time and each chunk's phonemize/synth at
/// play time -- runs on the same engine, and Kokoro phonemes never reach
/// Pocket. The queue holds only text and a voice name, never phonemes.
///
/// The inactive engine is never asked to synthesize, so it is never loaded:
/// `is_loaded` and `unload` speak for the active one alone, which keeps the
/// idle-unload policy truthful.
pub struct SwitchingSynthesizer {
    kokoro: Box<dyn Synthesizer>,
    pocket: Box<dyn Synthesizer>,
    active: ActiveEngine,
    pending: Option<ActiveEngine>,
    /// `cfg.voice`: Kokoro's default and its fallback for a foreign name.
    kokoro_voice: String,
    /// `cfg.pocket.voice`: the same for Pocket.
    pocket_voice: String,
    /// Names whose substitution has been logged already.
    warned: HashSet<String>,
    /// Set in the daemon; probes for the Pocket bundle so a missing one
    /// reads as "download it" rather than as a missing clip.
    models_dir: Option<PathBuf>,
    /// Where substitution notes go in tests, instead of stderr.
    #[cfg(test)]
    notes: Option<Vec<String>>,
}

/// What to do when Pocket TTS is chosen and its model is not on disk.
pub fn pocket_bundle_missing(models_dir: &Path) -> String {
    format!(
        "Pocket TTS is the selected engine but its model is not installed \
         (not every file is in {}). Open Settings from the tray and press \
         Download Pocket TTS, or switch the engine back to Kokoro.",
        sayd_pocket::bundle_dir(models_dir).display()
    )
}

impl SwitchingSynthesizer {
    pub fn new(
        kokoro: Box<dyn Synthesizer>,
        pocket: Box<dyn Synthesizer>,
        cfg: &Config,
    ) -> SwitchingSynthesizer {
        SwitchingSynthesizer {
            kokoro,
            pocket,
            active: ActiveEngine::of(cfg),
            pending: None,
            kokoro_voice: cfg.voice.clone(),
            pocket_voice: cfg.pocket.voice.clone(),
            warned: HashSet::new(),
            models_dir: None,
            #[cfg(test)]
            notes: None,
        }
    }

    /// Enable the missing-bundle hint for models in `models_dir`.
    pub fn with_models_dir(mut self, models_dir: &Path) -> SwitchingSynthesizer {
        self.models_dir = Some(models_dir.to_path_buf());
        self
    }

    /// The hint, when Pocket TTS is the engine in force (or about to be)
    /// and its bundle is absent.
    fn missing_bundle(&self, engine: ActiveEngine) -> Option<String> {
        let dir = self.models_dir.as_deref()?;
        (engine == ActiveEngine::Pocket && !crate::pocket_synth::bundle_installed(dir))
            .then(|| pocket_bundle_missing(dir))
    }

    /// The engine a new submission will be spoken on: the pending one if a
    /// switch is waiting, since it lands before the next utterance starts.
    fn target(&self) -> ActiveEngine {
        self.pending.unwrap_or(self.active)
    }

    fn inner(&self, engine: ActiveEngine) -> &dyn Synthesizer {
        match engine {
            ActiveEngine::Kokoro => self.kokoro.as_ref(),
            ActiveEngine::Pocket => self.pocket.as_ref(),
        }
    }

    fn active_mut(&mut self) -> &mut dyn Synthesizer {
        match self.active {
            ActiveEngine::Kokoro => self.kokoro.as_mut(),
            ActiveEngine::Pocket => self.pocket.as_mut(),
        }
    }

    /// (this engine's default, the other engine's default)
    fn defaults(&self, engine: ActiveEngine) -> (&str, &str) {
        match engine {
            ActiveEngine::Kokoro => (&self.kokoro_voice, &self.pocket_voice),
            ActiveEngine::Pocket => (&self.pocket_voice, &self.kokoro_voice),
        }
    }

    /// `voice` if the active engine has it, else the active engine's
    /// configured default (user decision, spec "An explicit voice while
    /// Pocket TTS is active").
    ///
    /// The substitution is logged once per name, with one exception: the
    /// *other* engine's configured default is substituted silently.
    /// `Engine::submit` stores the configured engine's default on every
    /// utterance that names no voice, so an utterance queued just before a
    /// switch arrives here carrying the old engine's default, and nobody
    /// asked for that voice by name. Any other foreign name was asked for,
    /// so it gets the note.
    fn resolve(&mut self, voice: &str) -> String {
        let engine = self.active;
        if self.inner(engine).voice_exists(voice) {
            return voice.to_string();
        }
        let (fallback, other) = self.defaults(engine);
        let (fallback, quiet) = (fallback.to_string(), voice == other);
        if !quiet && self.warned.insert(voice.to_string()) {
            self.note(format!(
                "note: voice '{voice}' is not a {} voice; speaking with '{fallback}'",
                engine.label()
            ));
        }
        fallback
    }

    fn note(&mut self, msg: String) {
        #[cfg(test)]
        if let Some(n) = self.notes.as_mut() {
            n.push(msg);
            return;
        }
        eprintln!("{msg}");
    }

    /// Problems with the configured defaults of the engine in force, worded
    /// for the daemon's log at startup. Checks only that engine: the other
    /// one's defaults reject nothing while it is not speaking.
    pub fn startup_warnings(&self) -> Vec<String> {
        let models = self
            .models_dir
            .as_deref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "the models directory".into());
        let mut out = Vec::new();
        match self.active {
            ActiveEngine::Kokoro => {
                // M21: a bad *configured* default voice is checked once here,
                // so an operator notices a typo before every default-voice
                // submission is rejected by `Engine::submit`. The settings
                // window lists the packs, or offers to download them on a
                // fresh install where there is nothing to pick.
                if !self.kokoro.voice_exists(&self.kokoro_voice) {
                    out.push(format!(
                        "configured default voice '{}' has no installed voice pack; \
                         every submission that does not override it with its own \
                         --voice will be rejected until this is fixed. The tray's \
                         Settings window lists the packs in {models}/voices, and \
                         offers to download them if there are none.",
                        self.kokoro_voice
                    ));
                }
            }
            ActiveEngine::Pocket => {
                // `unavailable` refuses every submission while this holds.
                if let Some(hint) = self.missing_bundle(ActiveEngine::Pocket) {
                    out.push(format!("{hint} Until then every submission is refused."));
                } else if !self.pocket.voice_exists(&self.pocket_voice) {
                    let dir = self
                        .models_dir
                        .as_deref()
                        .map(|d| crate::pocket_synth::voices_dir(d).display().to_string())
                        .unwrap_or_else(|| "the Pocket voices directory".into());
                    out.push(format!(
                        "configured Pocket TTS voice '{}' has no clip in {dir}; every \
                         submission that does not name another clip there with its \
                         own --voice will be rejected until this is fixed. Put a \
                         '{0}.wav' there, or pick another voice in the tray's \
                         Settings window.",
                        self.pocket_voice
                    ));
                }
            }
        }
        out
    }
}

impl Synthesizer for SwitchingSynthesizer {
    fn phonemize(&mut self, text: &str, voice: &str) -> String {
        // Kokoro picks its G2P dialect from the voice name, so it must see
        // the voice it will actually speak with.
        let voice = self.resolve(voice);
        self.active_mut().phonemize(text, &voice)
    }

    fn fits(&mut self, phonemes: &str) -> bool {
        self.active_mut().fits(phonemes)
    }

    fn synth(&mut self, phonemes: &str, voice: &str, speed: f32) -> Result<Vec<f32>, String> {
        let voice = self.resolve(voice);
        // A safety net only: `unavailable` keeps a submission from getting
        // this far without the model. One queued before the bundle went
        // missing (or before a switch to Pocket) can still arrive here, and
        // should fail with the hint rather than a file-not-found.
        self.active_mut()
            .synth(phonemes, &voice, speed)
            .map_err(|e| self.missing_bundle(self.active).unwrap_or(e))
    }

    fn sample_rate(&self) -> u32 {
        self.inner(self.active).sample_rate()
    }

    fn unload(&mut self) {
        self.active_mut().unload();
        if let Some(next) = self.pending.take() {
            self.active = next;
        }
    }

    fn is_loaded(&self) -> bool {
        self.inner(self.active).is_loaded()
    }

    /// Pocket TTS chosen (or about to be, with a switch pending) and its
    /// model not fully on disk: `Engine::submit` refuses each submission
    /// with the "Download Pocket TTS" hint, and takes them again as soon as
    /// the download has put the files in place. Kokoro is never
    /// unavailable here; a missing Kokoro pack is `voice_exists`'s business.
    fn unavailable(&self) -> Option<String> {
        self.missing_bundle(self.target())
    }

    /// Whether a submission naming `voice` can be spoken on the engine it
    /// will be spoken on (the pending one, if a switch is waiting).
    /// `Engine::submit` asks `unavailable` first, so a missing Pocket model
    /// never reaches here as an "unknown voice".
    ///
    /// - Pocket TTS: any name, provided the `pocket.voice` clip exists to
    ///   fall back to, so a hook passing a Kokoro name keeps working after
    ///   a switch.
    /// - Kokoro: a Kokoro pack, or a Pocket clip name (the spec's mirror
    ///   rule) provided the configured Kokoro pack exists to fall back to.
    ///   A name neither engine has is rejected exactly as before (M21), so
    ///   an unchanged Kokoro config behaves as it always did.
    fn voice_exists(&self, voice: &str) -> bool {
        match self.target() {
            ActiveEngine::Pocket => {
                self.pocket.voice_exists(voice) || self.pocket.voice_exists(&self.pocket_voice)
            }
            ActiveEngine::Kokoro => {
                self.kokoro.voice_exists(voice)
                    || (self.pocket.voice_exists(voice)
                        && self.kokoro.voice_exists(&self.kokoro_voice))
            }
        }
    }

    fn reconfigure(&mut self, cfg: &Config) -> bool {
        let kokoro = self.kokoro.reconfigure(cfg);
        let pocket = self.pocket.reconfigure(cfg);
        self.kokoro_voice = cfg.voice.clone();
        self.pocket_voice = cfg.pocket.voice.clone();
        let want = ActiveEngine::of(cfg);
        self.pending = (want != self.active).then_some(want);
        // The inactive engine is not loaded, so its own reload request needs
        // no unload; it reloads with the new settings when it next speaks.
        let reload = match self.active {
            ActiveEngine::Kokoro => kokoro,
            ActiveEngine::Pocket => pocket,
        };
        self.pending.is_some() || reload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pocket_synth::{voices_dir, PocketSynthesizer};
    use sayd_core::audio::VecSink;
    use sayd_core::engine::{Command, Engine, SayOpts, State};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    /// Records every call as `"<name> <call> <args>"`. `phonemize` wraps the
    /// text in `<name>(...)` so a test can tell whose phonemes reached
    /// whose `synth`.
    struct Rec {
        name: &'static str,
        voices: &'static [&'static str],
        log: Log,
        loaded: Arc<AtomicBool>,
        /// The other stub's `loaded`, to catch both engines held at once.
        other: Arc<AtomicBool>,
    }

    impl Synthesizer for Rec {
        fn phonemize(&mut self, text: &str, voice: &str) -> String {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} phonemize {voice} {text}", self.name));
            format!("{}({text})", self.name)
        }

        fn fits(&mut self, phonemes: &str) -> bool {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} fits {phonemes}", self.name));
            true
        }

        fn synth(&mut self, phonemes: &str, voice: &str, _speed: f32) -> Result<Vec<f32>, String> {
            self.loaded.store(true, Ordering::SeqCst);
            let mut log = self.log.lock().unwrap();
            if self.other.load(Ordering::SeqCst) {
                log.push("BOTH LOADED".into());
            }
            log.push(format!("{} synth {voice} {phonemes}", self.name));
            Ok(vec![0.0; 240])
        }

        fn unload(&mut self) {
            self.loaded.store(false, Ordering::SeqCst);
            self.log
                .lock()
                .unwrap()
                .push(format!("{} unload", self.name));
        }

        fn is_loaded(&self) -> bool {
            self.loaded.load(Ordering::SeqCst)
        }

        fn voice_exists(&self, voice: &str) -> bool {
            self.voices.contains(&voice)
        }
    }

    const KOKORO_VOICES: &[&str] = &["af_heart", "am_fenrir"];
    const POCKET_VOICES: &[&str] = &["fenrir"];

    struct Pair {
        log: Log,
        kokoro_loaded: Arc<AtomicBool>,
        pocket_loaded: Arc<AtomicBool>,
    }

    fn stubs() -> (Rec, Rec, Pair) {
        let log: Log = Arc::default();
        let k = Arc::new(AtomicBool::new(false));
        let p = Arc::new(AtomicBool::new(false));
        let kokoro = Rec {
            name: "kokoro",
            voices: KOKORO_VOICES,
            log: log.clone(),
            loaded: k.clone(),
            other: p.clone(),
        };
        let pocket = Rec {
            name: "pocket",
            voices: POCKET_VOICES,
            log: log.clone(),
            loaded: p.clone(),
            other: k.clone(),
        };
        (
            kokoro,
            pocket,
            Pair {
                log,
                kokoro_loaded: k,
                pocket_loaded: p,
            },
        )
    }

    fn switching(cfg: &Config) -> (SwitchingSynthesizer, Pair) {
        let (k, p, pair) = stubs();
        (
            SwitchingSynthesizer::new(Box::new(k), Box::new(p), cfg),
            pair,
        )
    }

    fn pocket_cfg() -> Config {
        Config {
            engine: "pocket".into(),
            ..Config::default()
        }
    }

    fn last(log: &Log) -> String {
        log.lock().unwrap().last().cloned().unwrap_or_default()
    }

    #[test]
    fn switch_waits_for_unload_and_queued_kokoro_voice_is_substituted() {
        let (mut s, pair) = switching(&Config::default());
        assert!(s.reconfigure(&pocket_cfg()), "a switch asks for an unload");
        assert!(
            s.reconfigure(&pocket_cfg()),
            "and keeps asking while it is pending"
        );

        s.synth("hello", "am_fenrir", 1.0).unwrap();
        assert_eq!(last(&pair.log), "kokoro synth am_fenrir hello");

        s.unload();
        assert!(!pair.kokoro_loaded.load(Ordering::SeqCst));
        assert!(!s.is_loaded(), "unload dropped the engine that was loaded");

        s.synth("hello", "am_fenrir", 1.0).unwrap();
        assert_eq!(last(&pair.log), "pocket synth fenrir hello");
        assert!(s.is_loaded());
        assert!(!pair.kokoro_loaded.load(Ordering::SeqCst));
        assert!(!s.reconfigure(&pocket_cfg()), "nothing pending any more");
    }

    #[test]
    fn voice_exists_accepts_names_valid_on_either_side_via_fallback() {
        let (mut s, _pair) = switching(&Config::default());
        assert!(s.voice_exists("am_fenrir"));
        assert!(s.voice_exists("fenrir"), "falls back to af_heart");
        assert!(
            !s.voice_exists("af_hart"),
            "a name neither engine has is still rejected under Kokoro (M21)"
        );

        assert!(s.reconfigure(&pocket_cfg()));
        s.unload();
        assert!(s.voice_exists("fenrir"));
        assert!(s.voice_exists("am_fenrir"), "falls back to fenrir");

        let mut gone = pocket_cfg();
        gone.pocket.voice = "gone".into();
        let _ = s.reconfigure(&gone);
        assert!(s.voice_exists("fenrir"), "a real clip needs no fallback");
        assert!(
            !s.voice_exists("am_fenrir"),
            "neither the name nor the fallback is a Pocket voice"
        );

        // A switch still pending: a new submission is spoken after it, so it
        // is judged by the engine it will be spoken on.
        let (mut s, _pair) = switching(&Config::default());
        assert!(s.reconfigure(&gone));
        assert!(!s.voice_exists("af_heart"));
        assert!(s.voice_exists("fenrir"));
    }

    #[test]
    fn substitution_logs_once_per_name() {
        let (mut s, _pair) = switching(&pocket_cfg());
        s.notes = Some(Vec::new());
        s.synth("a", "am_fenrir", 1.0).unwrap();
        let _ = s.phonemize("a", "am_fenrir");
        s.synth("b", "am_fenrir", 1.0).unwrap();
        s.synth("c", "bf_emma", 1.0).unwrap();
        s.synth("d", "fenrir", 1.0).unwrap();
        // The configured Kokoro default is what every utterance without its
        // own --voice carries; nobody asked for it by name.
        s.synth("e", "af_heart", 1.0).unwrap();
        assert_eq!(
            s.notes.as_deref().unwrap(),
            [
                "note: voice 'am_fenrir' is not a Pocket TTS voice; speaking with 'fenrir'",
                "note: voice 'bf_emma' is not a Pocket TTS voice; speaking with 'fenrir'",
            ]
        );

        // The mirror case.
        let (mut s, pair) = switching(&Config::default());
        s.notes = Some(Vec::new());
        s.synth("a", "clone", 1.0).unwrap();
        s.synth("b", "clone", 1.0).unwrap();
        assert_eq!(last(&pair.log), "kokoro synth af_heart b");
        s.synth("c", "fenrir", 1.0).unwrap();
        assert_eq!(last(&pair.log), "kokoro synth af_heart c");
        assert_eq!(
            s.notes.as_deref().unwrap(),
            ["note: voice 'clone' is not a Kokoro voice; speaking with 'af_heart'"]
        );
    }

    #[test]
    fn utterance_in_flight_finishes_on_kokoro_and_the_next_speaks_on_pocket() {
        let mut cfg = Config::default();
        cfg.chunking.target_chars = 25;
        let (s, pair) = switching(&cfg);
        let mut e = Engine::new(
            cfg.clone(),
            Box::new(s),
            Box::new(VecSink::new(24_000 * 600)),
        );
        e.submit(
            "First sentence here. Second sentence here. Third one too.".into(),
            SayOpts::default(),
        )
        .unwrap();
        e.tick();
        // B is queued while Kokoro is still the configured engine, so it
        // carries Kokoro's default voice into the switch: Pocket has to
        // substitute its own.
        e.submit("Now on pocket. And then more.".into(), SayOpts::default())
            .unwrap();
        let mut p = cfg.clone();
        p.engine = "pocket".into();
        e.handle(Command::ApplyConfig(p));
        for _ in 0..50 {
            e.tick();
        }

        let log = pair.log.lock().unwrap().clone();
        assert!(!log.contains(&"BOTH LOADED".to_string()), "{log:#?}");
        let switch = log
            .iter()
            .position(|l| l == "kokoro unload")
            .unwrap_or_else(|| panic!("no switch: {log:#?}"));
        let (before, after) = log.split_at(switch + 1);
        assert!(before.iter().all(|l| l.starts_with("kokoro ")), "{log:#?}");
        assert!(after.iter().all(|l| l.starts_with("pocket ")), "{log:#?}");
        let synths = |side: &[String], who: &str| {
            side.iter()
                .filter(|l| l.starts_with(&format!("{who} synth")))
                .count()
        };
        assert!(synths(before, "kokoro") >= 2, "A had several chunks");
        assert!(synths(after, "pocket") >= 1, "B spoke on Pocket");
        assert!(
            after.iter().all(|l| !l.contains("kokoro(")),
            "Kokoro phonemes reached Pocket: {log:#?}"
        );
        assert!(
            after
                .iter()
                .filter(|l| l.starts_with("pocket synth"))
                .all(|l| l.starts_with("pocket synth fenrir ")),
            "B, queued with Kokoro's default voice, was not spoken with Pocket's: {log:#?}"
        );
        assert!(
            !pair.pocket_loaded.load(Ordering::SeqCst)
                || !pair.kokoro_loaded.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn kokoro_rejects_a_typo_exactly_as_before_but_takes_a_pocket_clip_name() {
        let (s, pair) = switching(&Config::default());
        let mut e = Engine::new(
            Config::default(),
            Box::new(s),
            Box::new(VecSink::new(24_000 * 600)),
        );
        let named = |v: &str| SayOpts {
            voice: Some(v.into()),
            ..Default::default()
        };
        assert_eq!(
            e.submit("hi".into(), named("af_hart")).unwrap_err(),
            "unknown voice 'af_hart'; check the voices installed in the \
             daemon's models directory (its voices/ subdirectory has one \
             file per installed voice) and pick one of those"
        );
        e.submit("hi.".into(), named("fenrir"))
            .expect("a Pocket clip name falls back to the Kokoro default");
        for _ in 0..10 {
            e.tick();
        }
        assert_eq!(last(&pair.log), "kokoro synth af_heart kokoro(hi.)");
    }

    /// Writes a stand-in for every file the download puts in place: the
    /// bundle (as one-byte files, enough for `bundle_installed`) and the
    /// Fenrir clip.
    fn install_pocket(models: &Path) {
        let bundle = sayd_pocket::bundle_dir(models);
        std::fs::create_dir_all(&bundle).unwrap();
        for f in crate::pocket_synth::BUNDLE_FILES {
            std::fs::write(bundle.join(f), b"x").unwrap();
        }
        std::fs::create_dir_all(voices_dir(models)).unwrap();
        std::fs::write(voices_dir(models).join("fenrir.wav"), b"x").unwrap();
    }

    /// Final review I1: with Pocket TTS chosen and its model not
    /// downloaded, a submission is refused with the hint -- and refused
    /// only, never left behind as an error, so once the download lands the
    /// next one is taken without a dismiss.
    #[test]
    fn pocket_without_its_model_refuses_until_it_is_downloaded() {
        let tmp = tempfile::tempdir().unwrap();
        let models = tmp.path().join("models");
        std::fs::create_dir_all(&models).unwrap();
        let cfg = pocket_cfg();
        let (kokoro, _, _) = stubs();
        let pocket = PocketSynthesizer::new(&models, &tmp.path().join("cache"), &cfg);
        let mut s = SwitchingSynthesizer::new(Box::new(kokoro), Box::new(pocket), &cfg)
            .with_models_dir(&models);

        let warnings = s.startup_warnings();
        assert!(
            warnings.iter().any(|w| w.contains("Download Pocket TTS")),
            "{warnings:?}"
        );
        let e = s.synth("Hello.", "fenrir", 1.0).unwrap_err();
        assert!(e.contains("Download Pocket TTS"), "the safety net: {e}");

        let mut e = Engine::new(
            cfg.clone(),
            Box::new(s),
            Box::new(VecSink::new(24_000 * 600)),
        );
        let named = SayOpts {
            voice: Some("am_fenrir".into()),
            ..Default::default()
        };
        for opts in [SayOpts::default(), named] {
            let err = e.submit("Hello there.".into(), opts).unwrap_err();
            assert!(err.contains("Download Pocket TTS"), "{err}");
        }
        e.tick();
        let snap = e.snapshot();
        assert_eq!(snap.error, None, "a refusal, not a sticky error: {snap:?}");
        assert_eq!(snap.state, State::Idle);

        // The download finishes. Nothing is ticked after this: the stand-in
        // files would only fail to load.
        install_pocket(&models);
        e.submit("Hello again.".into(), SayOpts::default())
            .expect("taken as soon as the model is on disk");
        assert_eq!(e.snapshot().error, None);
    }

    /// Kokoro keeps the trait's default: never unavailable, so an existing
    /// Kokoro config is refused for nothing new, bundle or no bundle. A
    /// pending switch to Pocket is judged by Pocket.
    #[test]
    fn only_pocket_without_its_model_is_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let models = tmp.path().join("models");
        std::fs::create_dir_all(&models).unwrap();
        let (k, p, _) = stubs();
        let mut s = SwitchingSynthesizer::new(Box::new(k), Box::new(p), &Config::default())
            .with_models_dir(&models);
        assert_eq!(s.unavailable(), None);
        assert!(s.reconfigure(&pocket_cfg()), "a switch is pending");
        assert!(s
            .unavailable()
            .is_some_and(|w| w.contains("Download Pocket TTS")));
        install_pocket(&models);
        assert_eq!(s.unavailable(), None);
    }

    #[test]
    fn startup_warnings_check_the_active_engines_own_default() {
        let tmp = tempfile::tempdir().unwrap();
        let models = tmp.path().join("models");
        let bundle = sayd_pocket::bundle_dir(&models);
        std::fs::create_dir_all(&bundle).unwrap();
        for f in crate::pocket_synth::BUNDLE_FILES {
            std::fs::write(bundle.join(f), b"x").unwrap();
        }
        let warn = |cfg: &Config| {
            let (k, p, _) = stubs();
            SwitchingSynthesizer::new(Box::new(k), Box::new(p), cfg)
                .with_models_dir(&models)
                .startup_warnings()
        };

        assert!(warn(&Config::default()).is_empty());
        let mut c = Config {
            voice: "zz_none".into(),
            ..Config::default()
        };
        let w = warn(&c);
        assert!(
            w.len() == 1 && w[0].contains("'zz_none' has no installed voice pack"),
            "{w:?}"
        );
        c.engine = "pocket".into();
        assert!(
            warn(&c).is_empty(),
            "Kokoro's pack does not matter to Pocket"
        );
        c.pocket.voice = "nobody".into();
        let w = warn(&c);
        assert!(w.len() == 1 && w[0].contains("'nobody'"), "{w:?}");
        assert!(
            w[0].contains(&voices_dir(&models).display().to_string()),
            "{w:?}"
        );
    }

    #[test]
    fn a_partial_bundle_gets_the_download_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let models = tmp.path().join("models");
        let bundle = sayd_pocket::bundle_dir(&models);
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("bundle.json"), b"x").unwrap();
        let mut cfg = pocket_cfg();
        cfg.pocket.voice = "fenrir".into();
        let (k, p, _) = stubs();
        let s = SwitchingSynthesizer::new(Box::new(k), Box::new(p), &cfg).with_models_dir(&models);
        let w = s.startup_warnings();
        assert!(w.iter().any(|w| w.contains("Download Pocket TTS")), "{w:?}");
    }
}
