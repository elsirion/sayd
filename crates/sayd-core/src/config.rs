//! Persistent settings. TOML at `$XDG_CONFIG_HOME/sayd/config.toml`.
//!
//! Loading never fails: a missing file yields defaults silently, a malformed
//! one yields defaults plus a message for the UI to surface. The user's file
//! is never overwritten just because it failed to parse.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UrlPolicy {
    /// Replace a bare URL with the word "link".
    Link,
    /// Replace a bare URL with its host, e.g. "example.com".
    Domain,
    /// Leave it alone.
    Keep,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanupConfig {
    pub collapse_whitespace: bool,
    pub rejoin_hyphenation: bool,
    pub urls: UrlPolicy,
    pub strip_markdown: bool,
    pub drop_code_blocks: bool,
    pub spell_acronyms: bool,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        CleanupConfig {
            collapse_whitespace: true,
            rejoin_hyphenation: true,
            urls: UrlPolicy::Link,
            strip_markdown: true,
            drop_code_blocks: true,
            spell_acronyms: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChunkConfig {
    pub target_chars: usize,
    pub lookahead_chunks: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        ChunkConfig {
            target_chars: 400,
            lookahead_chunks: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    /// Off by default: narration changes how someone's desktop behaves, so it
    /// is asked for rather than assumed.
    pub enabled: bool,
    /// Application names to speak, matched case-insensitively against the
    /// `app_name` the application passes to `Notify`. Empty means silent --
    /// with `enabled = true` that is the intended way to discover names, since
    /// the daemon logs each one it declines to speak.
    pub allow: Vec<String>,
    /// Per-application rate-limit window. `0` switches rate limiting off;
    /// any other value is raised to [`NOTIFY_COOLDOWN_MIN_SECS`] on load.
    pub cooldown_secs: u64,
    pub speak_app_name: bool,
    /// Bodies are frequently several sentences and often restate the summary,
    /// so this is offered rather than assumed.
    pub speak_body: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        NotificationConfig {
            enabled: false,
            allow: Vec::new(),
            cooldown_secs: 30,
            speak_app_name: true,
            speak_body: false,
        }
    }
}

/// The window `reword.timeout_ms` is honoured in, applied by
/// [`Config::load_str`] as well as by `settings::model::clamp_ranges`.
///
/// Declared here rather than only beside the settings window's spin rows
/// because the ceiling is not a matter of taste: `sayd-cli` bounds every
/// D-Bus interaction at 3 s and `say --reword` waits for the rewrite
/// inline, so a hand-edited `timeout_ms = 86400000` -- which no spin row
/// can produce and no `load_str` caller used to reject -- turns a slow
/// provider into a CLI error instead of a spoken sentence. The value that
/// reaches `Duration::from_millis` must be inside this window whichever
/// door the config came through.
pub const REWORD_TIMEOUT_MIN_MS: u64 = 200;
/// See [`REWORD_TIMEOUT_MIN_MS`] for what this ceiling is for. Its *value*
/// is arithmetic, not taste, and it used to leave no margin at all.
///
/// A `Say` carrying `reword` is answered inline, so what `sayd-cli` waits
/// for is the rewrite, bounded by this, plus `EngineHandle::submit`, bounded
/// by `SUBMIT_REPLY_TIMEOUT` at 250 ms. (There used to be a third: the
/// daemon fetched its config with `EngineHandle::config()`, another 250 ms.
/// It reads `ConfigStore::current` now -- see `dbus::SaydIface::maybe_reword`
/// -- so that round trip is gone from this sum, and the margin below is
/// larger than the one 2000 was chosen against.) At 2500 the sum was
/// 250 + 2500 + 250 = 3000 ms -- exactly `sayd-cli`'s own 3 s bound, which is
/// to say zero theoretical margin for the bus round trip, for scheduling, or
/// for the difference between a budget and the moment the runtime notices it
/// has elapsed. The one failure this clamp exists to prevent was sitting on
/// its own boundary.
///
/// 2000 leaves about a second of it. What that costs a user with a genuinely
/// slower provider is visible to them: the settings window's Test row
/// reports the latency it measured, which is how someone discovers they need
/// a different provider rather than a larger number here.
///
/// MINOR 10: this arithmetic bounds `Say` and nothing else.
/// `SaySelection`/`SayClipboard` read a selection first, and that read
/// carries its own bounds -- `selection::SELECTION_READ_TIMEOUT` at 5 s of
/// inactivity and `SELECTION_READ_OVERALL_CAP` at 30 s overall -- either of
/// which is on its own past `sayd-cli`'s 3 s. A wedged selection owner is
/// therefore a "sayd is not responding" no matter what this constant says;
/// bounding that is the selection module's problem, not this one's.
pub const REWORD_TIMEOUT_MAX_MS: u64 = 2000;

/// `sayd-cli`'s own bound on any one D-Bus interaction, restated because
/// `sayd-cli` is a *binary* and nothing can import a constant from it.
///
/// It exists only for the assertion below. MINOR 6: [`REWORD_TIMEOUT_MAX_MS`]
/// derives its value from this number in prose, and nothing related the two
/// -- the test named as the pin
/// (`sayd::dbus::tests::a_reword_against_a_silent_provider_still_answers_inside_the_cli_bound`)
/// asserts only `elapsed < 3 s`, which the old, zero-margin 2500 satisfied
/// just as well. This is the relationship itself, checked at compile time in
/// the same style [`NOTIFY_COOLDOWN_MIN_SECS`] uses: raise the ceiling past
/// what the bound can carry, or lower `sayd-cli`'s `TIMEOUT` under it, and
/// the workspace stops building rather than shipping a `--reword` that
/// reports the daemon as dead.
const CLI_INTERACTION_BOUND_MS: u64 = 3000;

/// `SUBMIT_REPLY_TIMEOUT` from `crate::handle`, which is private there.
/// The one bounded engine round trip still inside a `--reword` `Say`.
const SUBMIT_REPLY_BOUND_MS: u64 = 250;

/// The margin the ceiling is chosen to leave: enough for the bus round trip,
/// for scheduling, and for the gap between a budget elapsing and the runtime
/// noticing. Half a second, which is what 2000 buys today.
const REWORD_CLI_MARGIN_MS: u64 = 500;

const _: () = assert!(
    REWORD_TIMEOUT_MAX_MS + SUBMIT_REPLY_BOUND_MS + REWORD_CLI_MARGIN_MS
        <= CLI_INTERACTION_BOUND_MS,
    "reword.timeout_ms's ceiling, plus the engine round trip that follows the \
     rewrite, plus the margin, must fit inside sayd-cli's own D-Bus timeout -- \
     or `say --reword` reports a daemon that is working fine as not responding"
);

/// The shortest non-zero `notifications.cooldown_secs` [`Config::load_str`]
/// will honour, and the one range in this table that is not about taste.
///
/// A coalescing window opens when a notification arrives and closes
/// `cooldown_secs` later, at which point the `"N more notifications"`
/// follow-up is composed and submitted *immediately* -- it is never
/// reworded. The notification that opened the window is submitted up to
/// `reword.timeout_ms` after it arrived, because its rewrite has to finish or
/// time out first. With a cooldown shorter than that budget the follow-up
/// therefore reaches the engine while its own opener is still in flight, and
/// `Source::Notification`'s `Policy::Front` does not save it: `Front` jumps
/// ahead of what is *pending*, not ahead of what is already playing (its own
/// doc comment: "Play next, but let the current utterance finish first"). On
/// an idle engine the follow-up starts playing at once, and the user hears
/// "Signal: 3 more notifications" before the notification it is counting
/// from. Measured with the shipped 1500 ms budget and `cooldown_secs = 1`.
///
/// One second past the reword ceiling, derived from it rather than written
/// out, so the two cannot drift: the opener is submitted at the latest
/// `REWORD_TIMEOUT_MAX_MS` after it arrived and this leaves the rest of that
/// second for the submission round trip. `0` is exempt because it means
/// something else entirely -- `Limiter::decide`'s `cooldown_secs == 0` arm
/// switches rate limiting off, so no window ever opens and no follow-up is
/// ever composed, and the ordering this floor protects does not exist.
pub const NOTIFY_COOLDOWN_MIN_SECS: u64 = REWORD_TIMEOUT_MAX_MS.div_ceil(1000) + 1;

/// Rewriting text for the ear before it is spoken.
///
/// Off by default, and pointed at a *local* endpoint by default. With
/// `enabled = false` nothing happens either way, but the configuration a
/// user first sees should be the one that keeps the README's promise;
/// choosing a remote endpoint should be an act.
///
/// This table is **not** gated on the `reword` cargo feature. The settings
/// window serialises the whole `Config` on every save, so a gated field
/// would be silently deleted the first time a feature-off daemon wrote the
/// file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RewordConfig {
    /// Rewrite notification announcements without being asked. `--reword`
    /// on a submission does not require this: `enabled` means "rewrite my
    /// notifications automatically", `--reword` is being asked.
    pub enabled: bool,
    /// Any OpenAI-compatible endpoint. PPQ, Ollama, llama.cpp's `server`,
    /// LM Studio and vLLM all speak the same request, so there is no
    /// `provider` field -- it would have one meaningful value and four ways
    /// to get it wrong. A trailing `/` is stripped before
    /// `/chat/completions` is appended, so both spellings work.
    pub base_url: String,
    pub model: String,
    /// Local servers ignore this. Prefer `api_key_env`: a key in a shell
    /// profile or a systemd `EnvironmentFile` can be rotated without
    /// touching a file the settings window rewrites wholesale, and it keeps
    /// the key out of that file entirely.
    pub api_key: String,
    /// If this names a variable that is set and non-empty, that value is
    /// used and `api_key` is ignored.
    pub api_key_env: String,
    /// How long a rewrite may take before the original is spoken instead.
    /// Clamped to [`REWORD_TIMEOUT_MIN_MS`]..=[`REWORD_TIMEOUT_MAX_MS`] on
    /// load as well as by `settings::model::clamp_ranges`: `sayd-cli`
    /// bounds every D-Bus interaction at 3 s, and `say --reword` waits for
    /// the rewrite inline, so a budget that could exceed it would turn a
    /// slow provider into a CLI error instead of a spoken sentence.
    pub timeout_ms: u64,
    /// Longer text is spoken as written. Clamped to 32..=2000. The default
    /// is the chunker's `target_chars`: one synthesis chunk, which is the
    /// natural unit here, and above it the submission is a document rather
    /// than a notification.
    pub max_chars: usize,
}

impl Default for RewordConfig {
    fn default() -> Self {
        RewordConfig {
            enabled: false,
            base_url: "http://localhost:11434/v1".into(),
            model: "llama3.2:3b".into(),
            api_key: String::new(),
            api_key_env: "SAYD_REWORD_API_KEY".into(),
            // A budget, not an observation: chosen to sit under sayd-cli's
            // 3 s D-Bus timeout with room for the bus round trip, and above
            // the first-token latency a small model is generally capable
            // of. End-to-end provider latency has not been measured -- the
            // settings window's Test row is how a user gets their own
            // number on their own setup and sets this against it.
            timeout_ms: 1500,
            max_chars: 400,
        }
    }
}

/// The key to send, or `None` for no `Authorization` header at all -- which
/// is exactly right for a local server and exactly wrong for a remote one.
///
/// Split from [`resolve_api_key`] so it can be tested without touching
/// process-global environment state, which every other test in this binary
/// shares.
pub fn resolve_api_key_with(
    cfg: &RewordConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if !cfg.api_key_env.is_empty() {
        if let Some(v) = env(&cfg.api_key_env) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    (!cfg.api_key.is_empty()).then(|| cfg.api_key.clone())
}

/// [`resolve_api_key_with`], against the real environment.
pub fn resolve_api_key(cfg: &RewordConfig) -> Option<String> {
    resolve_api_key_with(cfg, |name| std::env::var(name).ok())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub voice: String,
    pub speed: f32,
    /// `"model"` | `"stretch"`. How `speed` is realised. `"model"` (the
    /// default) hands `speed` to Kokoro's own `speed` input, which is what
    /// every prior release did. `"stretch"` synthesizes at `1.0` and
    /// WSOLA-stretches the result (`sayd_kokoro::audio::time_stretch`)
    /// instead.
    ///
    /// The default stays `"model"` on purpose: changing how everyone's audio
    /// is produced is not something to do silently. Measured reason to opt
    /// into `"stretch"` anyway -- at `speed = 1.3`, `af_heart`, "The quick
    /// brown fox…", Kokoro's own `speed` input renders the leading "The" 10
    /// dB quieter than at neighbouring speeds (it reads as the word being
    /// skipped), and `speed` is not even a linear tempo control there (1.3
    /// renders at roughly 1.17x, not 1.3x). `"stretch"` avoids both -- the
    /// same word came back 10.4 dB louder and the render hit the requested
    /// tempo -- but WSOLA has its own artifacts, so this is offered rather
    /// than substituted.
    pub speed_mode: String,
    /// `fp32` | `fp16` | `q8`. Measured: fp32 RTF 4.78, fp16 4.66, q8 1.40.
    pub model: String,
    /// Measured peak at 8; 16 and 24 both regress.
    pub threads: usize,
    /// Seconds of an empty queue before the ~1.27 GB ORT session is dropped.
    /// `0` disables idle unloading entirely (spec §8); see `maybe_unload`.
    pub idle_unload_secs: u64,
    pub muted: bool,
    /// Submissions longer than this are refused.
    pub max_chars: usize,
    pub cleanup: CleanupConfig,
    pub chunking: ChunkConfig,
    pub notifications: NotificationConfig,
    /// Boxed because `RewordConfig` pushed `Command::ApplyConfig(Config)`
    /// over clippy's `large_enum_variant` threshold, and `Config` grows by
    /// one nested table every settings-window milestone -- boxing here,
    /// inside `Config`, contains that growth to the field that keeps
    /// causing it, rather than an `#[allow]` on the whole `Command` enum
    /// that would also let `Command::Say`, the actually hot variant, grow
    /// unnoticed. `Box<T>` derefs transparently for field reads and writes,
    /// so this has no effect on any call site outside this file, and serde
    /// serialises/deserialises it exactly as it would the bare struct.
    pub reword: Box<RewordConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            voice: "af_heart".into(),
            speed: 1.0,
            speed_mode: "model".into(),
            model: "fp32".into(),
            threads: 8,
            idle_unload_secs: 600,
            muted: false,
            max_chars: 20_000,
            cleanup: CleanupConfig::default(),
            chunking: ChunkConfig::default(),
            notifications: NotificationConfig::default(),
            reword: Box::new(RewordConfig::default()),
        }
    }
}

impl Config {
    /// `$XDG_CONFIG_HOME/sayd/config.toml`, falling back to `~/.config`.
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("sayd").join("config.toml")
    }

    pub fn load() -> (Config, Option<String>) {
        Self::load_from(&Self::path())
    }

    /// Returns the config and, if the file existed but could not be parsed,
    /// a human-readable reason. A missing file is not an error.
    pub fn load_from(path: &Path) -> (Config, Option<String>) {
        let txt = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), None),
            Err(e) => return (Config::default(), Some(format!("{}: {e}", path.display()))),
        };
        let (cfg, err) = Self::load_str(&txt);
        (cfg, err.map(|e| format!("{}: {e}", path.display())))
    }

    /// Parse already-read text into a config, with no I/O of its own.
    ///
    /// Split out of `load_from` so a caller that must read the file itself
    /// first -- `config_watch::ConfigStore::reload`'s TOCTOU-safe path,
    /// which needs to see `NotFound` and an empty file separately from a
    /// parse error -- can parse the bytes it already has instead of paying
    /// for, and racing, a second read.
    pub fn load_str(txt: &str) -> (Config, Option<String>) {
        match toml::from_str::<Config>(txt) {
            Ok(mut c) => {
                // The one range this layer enforces itself. Everything else
                // out of range is a degradation the daemon can report and
                // carry on with (`settings::model::normalize`), and both
                // daemon entry points do exactly that. `reword.timeout_ms`
                // is different in kind: it is handed to
                // `Duration::from_millis` and awaited inline by `say
                // --reword`, so a value past `sayd-cli`'s 3 s D-Bus bound
                // does not degrade the answer, it replaces it with an
                // error. Enforced at the parse rather than at the use
                // because there are four `load_from` callers and only two
                // of them normalise.
                c.reword.timeout_ms = c
                    .reword
                    .timeout_ms
                    .clamp(REWORD_TIMEOUT_MIN_MS, REWORD_TIMEOUT_MAX_MS);
                // The same kind of range for the same kind of reason, and
                // the other half of the same interaction: a window that
                // closes before the notification that opened it has been
                // submitted inverts the two. `0` is left alone -- it is the
                // off switch, not a short window. See
                // `NOTIFY_COOLDOWN_MIN_SECS`.
                if c.notifications.cooldown_secs != 0 {
                    c.notifications.cooldown_secs =
                        c.notifications.cooldown_secs.max(NOTIFY_COOLDOWN_MIN_SECS);
                }
                (c, None)
            }
            Err(e) => (Config::default(), Some(e.to_string())),
        }
    }

    /// Write atomically: a temp file in the same directory, then rename. The
    /// caller is responsible for ignoring the resulting inotify event.
    ///
    /// A config carrying an inline `[reword] api_key` is written `0600`.
    /// Without this it lands with the process umask, which on most desktops
    /// is world-readable -- and the settings window's password row is a
    /// direct route to putting a key there.
    ///
    /// The mode is set on the *temp* file, before the rename, rather than on
    /// the destination afterwards. DEPARTURE from §6, which says "after the
    /// rename": setting it afterwards leaves a window in which the key is
    /// on disk at the umask's mode, and the observable requirement -- the
    /// file the user ends up with is `0600` -- is satisfied either way.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let txt = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, txt)?;
        if !self.reword.api_key.is_empty() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        std::fs::rename(&tmp, path)
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_measured_benchmarks() {
        let c = Config::default();
        assert_eq!(c.model, "fp32");
        assert_eq!(c.threads, 8);
        assert_eq!(c.speed, 1.0);
        assert_eq!(
            c.speed_mode, "model",
            "changing how everyone's audio is produced is opt-in, not silent"
        );
        assert_eq!(c.idle_unload_secs, 600);
        assert!(!c.muted);
        assert_eq!(c.chunking.lookahead_chunks, 2);
    }

    #[test]
    fn roundtrips_through_toml() {
        let c = Config {
            voice: "am_fenrir".into(),
            speed: 1.25,
            cleanup: CleanupConfig {
                urls: UrlPolicy::Domain,
                ..CleanupConfig::default()
            },
            ..Config::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");
        c.save_to(&p).expect("save");
        let (back, err) = Config::load_from(&p);
        assert_eq!(err, None);
        assert_eq!(back, c);
    }

    #[test]
    fn missing_file_yields_defaults_without_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (c, err) = Config::load_from(&dir.path().join("nope.toml"));
        assert_eq!(c, Config::default());
        assert_eq!(err, None, "a missing config is normal, not an error");
    }

    #[test]
    fn malformed_file_yields_defaults_and_reports_the_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "voice = [this is not toml").expect("write");
        let (c, err) = Config::load_from(&p);
        assert_eq!(c, Config::default());
        assert!(
            err.is_some(),
            "a malformed config must be surfaced, not swallowed"
        );
    }

    #[test]
    fn partial_file_fills_the_rest_from_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "voice = \"bm_george\"\n").expect("write");
        let (c, err) = Config::load_from(&p);
        assert_eq!(err, None);
        assert_eq!(c.voice, "bm_george");
        assert_eq!(c.threads, 8, "unspecified keys must keep their defaults");
    }

    /// The defaults are a promise: narration is off, and turning it on speaks
    /// nothing until an application is named. A default that spoke everything
    /// would make enabling the feature a surprise.
    #[test]
    fn notification_defaults_are_silent_and_opt_in() {
        let c = Config::default();
        assert!(!c.notifications.enabled);
        assert!(c.notifications.allow.is_empty());
        assert_eq!(c.notifications.cooldown_secs, 30);
        assert!(c.notifications.speak_app_name);
        assert!(!c.notifications.speak_body);
    }

    /// A config written before this milestone has no `[notifications]` table
    /// at all, and must keep loading.
    #[test]
    fn a_config_without_the_notifications_table_still_loads() {
        let (c, err) = Config::load_str("voice = \"am_fenrir\"\n");
        assert_eq!(err, None);
        assert_eq!(c.voice, "am_fenrir");
        assert!(!c.notifications.enabled);
    }

    /// A config written before this milestone has no `speed_mode` key at
    /// all, and must keep loading at the pre-existing behaviour rather than
    /// refusing to parse.
    #[test]
    fn a_config_without_speed_mode_still_loads_at_the_model_default() {
        let (c, err) = Config::load_str("voice = \"am_fenrir\"\n");
        assert_eq!(err, None);
        assert_eq!(c.voice, "am_fenrir");
        assert_eq!(c.speed_mode, "model");
    }

    #[test]
    fn speed_mode_round_trips_through_toml() {
        let c = Config {
            speed_mode: "stretch".into(),
            ..Config::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");
        c.save_to(&p).expect("save");
        let (back, err) = Config::load_from(&p);
        assert_eq!(err, None);
        assert_eq!(back, c);
    }

    #[test]
    fn the_notifications_table_round_trips() {
        let mut c = Config::default();
        c.notifications.enabled = true;
        c.notifications.allow = vec!["Signal".into(), "Fractal".into()];
        c.notifications.speak_body = true;
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");
        c.save_to(&p).expect("save");
        let (back, err) = Config::load_from(&p);
        assert_eq!(err, None);
        assert_eq!(back, c);
    }

    #[test]
    fn save_is_atomic_leaving_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");
        Config::default().save_to(&p).expect("save");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");
    }

    /// The defaults are a promise: rewording is off, and the endpoint a user
    /// first sees is a local one. Pointing sayd at a remote endpoint should
    /// be an act, not something they inherit.
    #[test]
    fn reword_defaults_are_off_and_local() {
        let c = Config::default();
        assert!(!c.reword.enabled);
        assert_eq!(c.reword.base_url, "http://localhost:11434/v1");
        assert_eq!(c.reword.model, "llama3.2:3b");
        assert_eq!(c.reword.api_key, "");
        assert_eq!(c.reword.api_key_env, "SAYD_REWORD_API_KEY");
        assert_eq!(
            c.reword.timeout_ms, 1500,
            "a budget, not a measurement -- see the spec's §10"
        );
        assert_eq!(c.reword.max_chars, 400);
    }

    /// A config written before this milestone has no `[reword]` table at
    /// all, and must keep loading.
    #[test]
    fn a_config_without_the_reword_table_still_loads() {
        let (c, err) = Config::load_str("voice = \"am_fenrir\"\n");
        assert_eq!(err, None);
        assert_eq!(c.voice, "am_fenrir");
        assert!(!c.reword.enabled);
        assert_eq!(c.reword.timeout_ms, 1500);
    }

    /// A hand-edited `timeout_ms` is clamped by the *parse*, not only by
    /// the two callers that happen to normalise afterwards.
    ///
    /// `86400000` reaches `Duration::from_millis` otherwise, and with a
    /// real client the practical bound becomes the 10 s HTTP ceiling --
    /// which is past `sayd-cli`'s 3 s D-Bus bound, so `say --reword`
    /// returns a CLI error rather than a sentence. That is exactly what
    /// [`REWORD_TIMEOUT_MAX_MS`] exists to prevent.
    #[test]
    fn a_hand_edited_timeout_is_clamped_by_the_parse_itself() {
        let (c, err) = Config::load_str("[reword]\ntimeout_ms = 86400000\n");
        assert_eq!(err, None);
        assert_eq!(c.reword.timeout_ms, REWORD_TIMEOUT_MAX_MS);

        let (c, _) = Config::load_str("[reword]\ntimeout_ms = 0\n");
        assert_eq!(
            c.reword.timeout_ms, REWORD_TIMEOUT_MIN_MS,
            "and a zero budget is not a way to switch the feature off -- \
             `enabled` is"
        );

        // A value inside the window is not touched, which is the case that
        // matters for every honest config.
        let (c, _) = Config::load_str("[reword]\ntimeout_ms = 1200\n");
        assert_eq!(c.reword.timeout_ms, 1200);
    }

    /// A coalescing window that closes before the notification which opened
    /// it has been submitted inverts the two: the follow-up is never
    /// reworded, so it goes out the instant the window closes, while its
    /// opener is still waiting on a rewrite for up to `timeout_ms`. On an
    /// idle engine the follow-up starts playing at once and
    /// `Policy::Front` does not save the opener -- `Front` jumps ahead of
    /// what is pending, not ahead of what is already playing. Measured with
    /// the shipped 1500 ms budget and `cooldown_secs = 1`.
    #[test]
    fn a_cooldown_shorter_than_the_rewrite_budget_is_raised_to_clear_it() {
        let (c, err) = Config::load_str("[notifications]\ncooldown_secs = 1\n");
        assert_eq!(err, None);
        assert_eq!(c.notifications.cooldown_secs, NOTIFY_COOLDOWN_MIN_SECS);

        assert!(
            std::time::Duration::from_secs(NOTIFY_COOLDOWN_MIN_SECS)
                > std::time::Duration::from_millis(REWORD_TIMEOUT_MAX_MS),
            "the floor has to actually clear the budget it is derived from, \
             or it is decoration"
        );

        let (c, _) = Config::load_str("[notifications]\ncooldown_secs = 0\n");
        assert_eq!(
            c.notifications.cooldown_secs, 0,
            "`0` is the off switch, not a short window: with rate limiting off \
             no window opens and no follow-up is ever composed"
        );

        // Everything an honest config says is left exactly as it said it.
        let (c, _) = Config::load_str("[notifications]\ncooldown_secs = 30\n");
        assert_eq!(c.notifications.cooldown_secs, 30);
    }

    #[test]
    fn the_reword_table_round_trips() {
        let mut c = Config::default();
        c.reword.enabled = true;
        c.reword.base_url = "https://api.ppq.ai/v1".into();
        c.reword.model = "gpt-4o-mini".into();
        // The key fields are the ones this table is most sensitive about --
        // a silently dropped or mangled key would be the "miserable to
        // debug" failure the design worries about, so both must round-trip
        // too, not just be present when checking the file's mode.
        c.reword.api_key = "sk-secret".into();
        c.reword.api_key_env = "MY_CUSTOM_ENV".into();
        c.reword.timeout_ms = 2000;
        c.reword.max_chars = 300;
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");
        c.save_to(&p).expect("save");
        let (back, err) = Config::load_from(&p);
        assert_eq!(err, None);
        assert_eq!(back, c);
    }

    /// A key pasted into the settings window must not land in a
    /// world-readable file. `save_to` writes with the process umask
    /// otherwise.
    #[test]
    fn a_config_carrying_an_api_key_is_written_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.toml");

        let mut c = Config::default();
        c.reword.api_key = "sk-secret".into();
        c.save_to(&p).expect("save");
        let mode = std::fs::metadata(&p)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a config holding an API key must not be readable by anyone else"
        );

        // And a config with no key is left to the umask, unchanged from
        // every release before this one -- proven by parity with a plain
        // `std::fs::write` in the same directory, rather than hard-coding
        // an assumption about what the umask actually is.
        let q = dir.path().join("nokey.toml");
        Config::default().save_to(&q).expect("save");
        let sibling = dir.path().join("sibling.txt");
        std::fs::write(&sibling, "unrelated").expect("write");
        let q_mode = std::fs::metadata(&q)
            .expect("metadata")
            .permissions()
            .mode();
        let sibling_mode = std::fs::metadata(&sibling)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            q_mode & 0o777,
            sibling_mode & 0o777,
            "a config with no api key must be left to the umask, like any other file"
        );
    }

    /// The environment wins over the file, and an unset or empty variable
    /// falls back to it. No key at all is `None`, which is what stops an
    /// `Authorization` header being sent to a local server that does not
    /// want one.
    #[test]
    fn the_environment_key_wins_over_the_file() {
        let mut cfg = RewordConfig {
            api_key: "from-file".into(),
            api_key_env: "SAYD_TEST_KEY".into(),
            ..Default::default()
        };

        let env_set = |name: &str| (name == "SAYD_TEST_KEY").then(|| "from-env".to_string());
        assert_eq!(
            resolve_api_key_with(&cfg, env_set).as_deref(),
            Some("from-env")
        );

        let env_empty = |name: &str| (name == "SAYD_TEST_KEY").then(String::new);
        assert_eq!(
            resolve_api_key_with(&cfg, env_empty).as_deref(),
            Some("from-file"),
            "an empty variable is not a key; the file still counts"
        );

        let env_unset = |_: &str| None;
        assert_eq!(
            resolve_api_key_with(&cfg, env_unset).as_deref(),
            Some("from-file")
        );

        cfg.api_key = String::new();
        assert_eq!(
            resolve_api_key_with(&cfg, env_unset),
            None,
            "no key anywhere means no Authorization header at all"
        );

        // An empty `api_key_env` must not be looked up as a variable name.
        cfg.api_key = "from-file".into();
        cfg.api_key_env = String::new();
        assert_eq!(
            resolve_api_key_with(&cfg, |name| {
                panic!("an empty api_key_env must never be looked up (got {name:?})")
            })
            .as_deref(),
            Some("from-file")
        );
    }
}
