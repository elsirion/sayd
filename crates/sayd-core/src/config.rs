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
    /// The whole of text cleanup, on or off. When false, [`crate::cleanup::clean`]
    /// returns its input untouched and every switch below is inert -- kept,
    /// not cleared, so turning it back on restores the arrangement rather
    /// than a default one.
    ///
    /// Short-circuited inside `clean` rather than at its call sites, so both
    /// of them honour it without either one testing it.
    pub enabled: bool,
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
            enabled: true,
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
    /// any other value is raised to [`notify_cooldown_min_secs`] on load,
    /// which is a floor only when notification rewriting is on.
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

/// The shortest `reword.timeout_ms` [`Config::load_str`] will honour.
///
/// **There is no matching ceiling, deliberately.** There used to be one --
/// 2000 ms, derived from `sayd-cli`'s 3 s bound on a D-Bus call minus the
/// engine round trip that follows a rewrite -- and it was wrong in kind, not
/// in value: a local model on someone else's hardware can legitimately need
/// twenty seconds, and this daemon has no way to know that number. What the
/// old ceiling actually protected was `sayd-cli`, which bounded every call
/// at 3 s and would have reported a daemon that was working fine as not
/// responding. That is now fixed where it lives: `sayd-cli` leaves a `Say`
/// carrying `reword` unbounded and lets the daemon's own configured
/// deadline end it (see `sayd-cli`'s `TIMEOUT`), so nothing downstream needs
/// this value to be small.
///
/// The floor stays, and it is not taste either. Below it a "deadline" cannot
/// be met by any provider on any network, so `enabled = true` with
/// `timeout_ms = 5` would look like a configured feature and behave like a
/// switched-off one: every rewrite abandoned, the original spoken, and
/// nothing in the journal that names the cause. `0` in particular is not a
/// way to switch rewriting off -- `enabled` is -- so it is raised rather
/// than honoured.
///
/// Applied by [`Config::load_str`] rather than only by
/// `settings::model::normalize`, because there are four `load_from` callers
/// and only two of them normalise.
pub const REWORD_TIMEOUT_MIN_MS: u64 = 200;

/// The shortest non-zero `notifications.cooldown_secs` [`Config::load_str`]
/// will honour for `reword`, and the one range in this table that is not
/// about taste.
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
/// A function rather than the constant it was, because the budget it has to
/// clear is now the user's own: `reword.timeout_ms` has no ceiling, so there
/// is no largest value to derive a single number from. One second past the
/// configured deadline, rounded up, so the opener -- submitted at the latest
/// `timeout_ms` after it arrived -- still has the rest of that second for
/// its submission round trip.
///
/// This is required, not incidental, and it is worth being explicit about
/// the size it can reach: `timeout_ms = 300_000` (five minutes, for a local
/// model on slow hardware) with `cooldown_secs = 4` in the same file raises
/// the cooldown to **301** on load -- `300_000.div_ceil(1000) + 1` -- and
/// `Config::save_to` writes that raised value back to `config.toml`, not the
/// 4 the user typed. That is not a bug this function could avoid by being
/// gentler: a five-second window is still shorter than the five-minute
/// budget the opener may need, so honouring the user's `4` would let the
/// ordering bug this floor exists to prevent happen anyway, every time,
/// silently, for as long as the deadline stays long. Surprising as "setting
/// a long rewrite deadline acquires minutes-long notification coalescing"
/// reads, the alternative is a race the user asked for without knowing it.
///
/// Two exemptions, and neither is a softening of the rule:
///
/// * **Rewording off.** Nothing delays the opener when `enabled` is false --
///   the notification path is the only one this ordering concerns, and it
///   rewrites only when `enabled` is on (`sayd_core::reword::eligible`). An
///   explicit `say --reword` is not a notification and opens no window. So
///   the floor is 1: the smallest non-zero cooldown there is, which is to
///   say no floor at all. Inflating it would take a setting nobody asked
///   about (a 2-second cooldown on a daemon that does not reword) and raise
///   it for a reason that does not apply.
/// * **`cooldown_secs == 0`.** That means something else entirely --
///   `Limiter::decide`'s zero arm switches rate limiting off, so no window
///   ever opens and no follow-up is ever composed. The ordering this floor
///   protects does not exist there. The exemption is applied by the callers
///   (`Config::load_str` and `settings::model::clamp_ranges`), which is
///   where the `!= 0` test can be read next to the `max` it guards.
pub fn notify_cooldown_min_secs(reword: &RewordConfig) -> u64 {
    // `notifications`, not the `enabled` master: the ordering this floor
    // protects is the notification coalescing window's, and nothing opens
    // one unless notifications are being rewritten automatically. An
    // explicit `say --reword` is not a notification and opens no window.
    if !(reword.enabled && reword.notifications) {
        return 1;
    }
    reword.timeout_ms.div_ceil(1000) + 1
}

/// What a submission over `max_chars` is refused with.
///
/// One function rather than one `format!` because two gates produce it:
/// `Engine::submit`, which is the engine's guarantee about its own queue,
/// and `sayd::pipeline::prepare`, which catches the same text earlier so an
/// over-long submission never costs a provider round trip first. A caller
/// must not be able to tell which one caught it -- the text is over the
/// limit either way, and two wordings for one refusal is a difference
/// that means nothing.
pub fn too_long(chars: usize, limit: usize) -> String {
    format!("text is {chars} characters, limit is {limit}")
}

/// Which dialect a provider is told to stop reasoning in.
///
/// The one thing `base_url` cannot say. Every endpoint in §6's table speaks
/// the same `/chat/completions`, which is why there was no `provider`
/// setting for so long -- but they do not agree on how to switch a thinking
/// model's thinking off, and a model that thinks cannot answer inside
/// `timeout_ms`. Measured against the local llama.cpp router:
/// `chat_template_kwargs` suppressed reasoning on 6 requests of 6, while
/// `reasoning_budget` was ignored on 6 of 6 and the unmodified request
/// reasoned on 9 of 10 -- 13 to 33 s each, an order of magnitude past the
/// 1500 ms default deadline this whole table exists to avoid missing.
///
/// Two values, because only two are measured. vLLM documents the same
/// `chat_template_kwargs` upstream and Ollama and LM Studio have their own
/// spellings, but none of the three has been tested here, and a dialect
/// guessed wrong is a 400 on a path whose whole design is to fail quietly.
/// Adding one later is a match arm and a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Sends `chat_template_kwargs: {"enable_thinking": false}`.
    LlamaCpp,
    /// Sends nothing beyond the common request. Correct for a provider that
    /// does not reason, and for any remote one: OpenAI-compatible services
    /// reject unknown top-level fields rather than ignoring them.
    Generic,
}

impl Provider {
    /// Every accepted spelling, for the messages that have to list them.
    pub const NAMES: [&'static str; 2] = ["llama-cpp", "generic"];

    /// `None` for anything not in [`Provider::NAMES`].
    ///
    /// Surrounding whitespace is forgiven because a hand-edited TOML value
    /// is where this comes from; case is not, because the value is a token
    /// rather than prose and accepting `"Generic"` invites `"Llama.CPP"`.
    pub fn parse(name: &str) -> Option<Provider> {
        match name.trim() {
            "llama-cpp" => Some(Provider::LlamaCpp),
            "generic" => Some(Provider::Generic),
            _ => None,
        }
    }
}

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
    /// Rewording at all, on or off. The master.
    ///
    /// When false nothing is reworded -- not notifications, not an explicit
    /// `say --reword` -- and the endpoint settings below are kept rather
    /// than cleared, so this is "off for now" and not "forget my provider".
    ///
    /// **Defaults to `true`, and that is not the feature being on by
    /// default.** [`RewordConfig::provider`] defaults to `None`, so
    /// `HttpRewriter::new` refuses, `build_rewriter` returns `NotConfigured`
    /// and `reword::context` yields no client: a config that has never
    /// mentioned rewording rewrites nothing, exactly as before. What the
    /// default buys is that switching the master off is a deliberate act
    /// recorded in the file, rather than the state everyone starts in --
    /// which in turn is what keeps the migration below from having to guess.
    ///
    /// This field held [`RewordConfig::notifications`]'s meaning until
    /// 2026-08-24; see [`Config::load_str`] for the migration that moves an
    /// old file's value across.
    pub enabled: bool,
    /// Rewrite notification announcements without being asked.
    ///
    /// `--reword` on a submission does not require this: `notifications`
    /// means "rewrite my notifications automatically", `--reword` is being
    /// asked. Both require [`RewordConfig::enabled`].
    pub notifications: bool,
    /// Any OpenAI-compatible endpoint. PPQ, Ollama, llama.cpp's `server`,
    /// LM Studio and vLLM all speak the same request, which is why `base_url`
    /// alone once said everything. What they do not agree on is how a thinking
    /// model is told to stop reasoning; that is what [`RewordConfig::provider`]
    /// carries. A trailing `/` is stripped before `/chat/completions` is
    /// appended, so both spellings work.
    pub base_url: String,
    pub model: String,
    /// Which provider is at `base_url`, and so how it is told not to reason.
    ///
    /// A `String` rather than a [`Provider`], and resolved by
    /// [`RewordConfig::resolved_provider`] at use. As an enum, one typo
    /// fails the parse of the whole document and `load_str` returns
    /// `Config::default()` -- every other setting in the file discarded over
    /// a misspelling. Parse leniently, refuse at use: the same shape
    /// `timeout_ms`'s clamp and `settings::model::normalize` already have.
    ///
    /// Required when `enabled` is true; see
    /// [`reword_startup_refusal`]. `skip_serializing_if` because the
    /// settings window rewrites the whole file and an unset provider must
    /// come back as an absent key rather than as something that will not
    /// parse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Local servers ignore this. Prefer `api_key_env`: a key in a shell
    /// profile or a systemd `EnvironmentFile` can be rotated without
    /// touching a file the settings window rewrites wholesale, and it keeps
    /// the key out of that file entirely.
    pub api_key: String,
    /// If this names a variable that is set and non-empty, that value is
    /// used and `api_key` is ignored.
    pub api_key_env: String,
    /// How long a rewrite may take before the original is spoken instead.
    /// Raised to [`REWORD_TIMEOUT_MIN_MS`] on load if it is shorter, and
    /// bounded above by nothing: a local model may legitimately need far
    /// longer than any number this daemon could pick. Everything downstream
    /// derives from the value set here -- the transport's own ceiling
    /// (`sayd::reword::http_ceiling`), the notification cooldown floor
    /// ([`notify_cooldown_min_secs`]), and `sayd-cli`, which leaves a `Say`
    /// carrying `reword` unbounded precisely because it cannot know this
    /// number.
    pub timeout_ms: u64,
    /// The same deadline for every explicit `--reword`, and much longer.
    /// Raised to [`REWORD_TIMEOUT_MIN_MS`] on load; no upper bound, for the
    /// reason `timeout_ms` has none.
    ///
    /// The deadline has to be split for the same reason the character
    /// ceiling does, and the consequence of *not* splitting it is worse than
    /// a bad number. A notification wants a short budget: it arrives
    /// uninvited, it is one sentence, and a rewrite that has not landed in a
    /// second and a half should get out of the way. An explicit `--reword`
    /// over a document wants tens of seconds, because that is simply how
    /// long a local model takes to read two thousand characters and write a
    /// paragraph.
    ///
    /// One shared number could not serve both, and raising it to suit the
    /// document would have dragged something unrelated with it:
    /// [`notify_cooldown_min_secs`] derives the notification coalescing
    /// floor from `timeout_ms`, so a 25-second deadline set for clipboard
    /// reads would have silently turned every notification window into 26
    /// seconds. That floor still reads `timeout_ms`, and now `timeout_ms`
    /// means only what the notification path asked for.
    pub request_timeout_ms: u64,
    /// Speak an explicit `--reword` sentence by sentence, as the model
    /// writes it, instead of waiting for the whole answer.
    ///
    /// **Off by default, and this is the one switch that trades the
    /// feature's safety property away.** Everything else here degrades to
    /// "the original is spoken". Streaming cannot: once a sentence is
    /// audio it cannot be unsaid, so from the moment the first one is
    /// committed there is no fallback left. `sayd_core::reword::check` --
    /// which rejects a rewrite that grew past `length_ceiling`, that came
    /// back as more than one line, or that carries a code fence -- judges a
    /// whole answer and cannot judge a prefix. A model that follows the
    /// prompt for three sentences and then explains itself in the fourth is
    /// caught today and is spoken when this is on.
    ///
    /// What it buys is the wait. A local model reading a document takes
    /// tens of seconds, and all of it is silence today; streamed, the first
    /// sentence starts at roughly prefill plus one sentence of decode.
    ///
    /// Only ever applies to an explicit `--reword`. A notification is one
    /// sentence, so there is nothing to stream and fragmenting it into
    /// several utterances would be worse than the wait it saves.
    pub stream: bool,
    /// The instruction sent with a notification rewrite, or `None` for
    /// [`crate::reword::NOTIFICATION_PROMPT`].
    ///
    /// `Option` and `skip_serializing_if` rather than a `String` carrying
    /// the default, for the reason [`RewordConfig::provider`] is one: the
    /// settings window rewrites the whole file on every save, so a `String`
    /// here would stamp today's wording into every config that has never
    /// touched it and freeze it there. Absent means "whatever this build
    /// ships", which is what lets an improved default reach a user who
    /// never asked to own this text.
    ///
    /// Blank -- empty or whitespace -- reads as absent rather than as an
    /// empty instruction. A user who clears the box in the settings window
    /// means "give me the default back", and a request carrying an empty
    /// system message is not a thing anyone wants sent on their behalf.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The same for an explicit `--reword`, defaulting to
    /// [`crate::reword::REQUEST_PROMPT`].
    ///
    /// Separate from [`RewordConfig::prompt`] for the reason
    /// [`RewordConfig::request_max_chars`] is separate from `max_chars`, and
    /// the evidence is sharper here: the notification prompt asks for one or
    /// two sentences and forbids dropping names, which turns a document into
    /// a headline with a file path read aloud in it. One box for both would
    /// force a choice between announcing a Signal message well and
    /// summarising a page of output well.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_prompt: Option<String>,
    /// Longer text is spoken as written; `0` means no ceiling and is the
    /// default. A limit here is a tightening a user reaches for after
    /// watching their own provider choke, not a guess this file makes for
    /// them first.
    ///
    /// **The notification ceiling only.** Every explicit `--reword` is
    /// measured against [`RewordConfig::request_max_chars`] instead; see
    /// that field for why one number could not serve both.
    pub max_chars: usize,
    /// The same ceiling for every explicit `--reword`; `0` again means no
    /// ceiling and is the default.
    ///
    /// `max_chars` cannot serve both, because the two asks are not the same
    /// shape. A notification arrives uninvited and is already short, so its
    /// ceiling stops at 2000: past that it is a document rather than a
    /// notification, and rewriting it was never what the feature was for. An
    /// explicit `--reword` is a user pointing at something and asking for
    /// it, and what they point at is routinely a document -- a page of
    /// prose, a chat log, a long tool output. Sharing one number would mean
    /// choosing which of the two to break.
    ///
    /// Applies to `say --reword "..."`, `say --reword selection` and `say
    /// --reword clipboard` alike: they are one ask made three ways, and a
    /// limit that varied between them would be a rule nobody could hold in
    /// their head.
    ///
    /// **The response bound does not scale with this.** `max_tokens` is
    /// still derived from `max_chars`, and deliberately: this number bounds
    /// how much text may be *sent*, while `max_tokens` bounds how much comes
    /// *back*, and what comes back is a spoken summary. A summary of eight
    /// thousand characters is not eight thousand characters long -- it is a
    /// paragraph, the same size as a summary of four hundred. Scaling the
    /// response with the input would buy nothing and would ask a local model
    /// for a generation far longer than anything worth listening to.
    pub request_max_chars: usize,
}

impl Default for RewordConfig {
    fn default() -> Self {
        RewordConfig {
            enabled: true,
            notifications: false,
            base_url: "http://localhost:11434/v1".into(),
            model: "llama3.2:3b".into(),
            provider: None,
            api_key: String::new(),
            api_key_env: "SAYD_REWORD_API_KEY".into(),
            // A budget, not an observation: above the first-token latency a
            // small local model is generally capable of, and short enough
            // that a missed rewrite -- the original spoken instead -- reads
            // as a brief pause rather than a stall. It does not answer to
            // any ceiling above it; `timeout_ms` has none
            // (REWORD_TIMEOUT_MIN_MS explains why the floor stays and the
            // ceiling doesn't). End-to-end provider latency has not been
            // measured -- the settings window's Test row is how a user gets
            // their own number on their own setup and sets this against it.
            timeout_ms: 1500,
            // Measured, unlike the one above: against a local llama.cpp
            // server on a CPU-only box, summarising ~2000-character
            // assistant answers, the fastest usable models answered in
            // 8.6-16.4 s. 25 s clears the slowest of those with room for a
            // cold model load, and losing the race still only costs the
            // original being spoken.
            request_timeout_ms: 25_000,
            stream: false,
            prompt: None,
            request_prompt: None,
            max_chars: 0,
            request_max_chars: 0,
        }
    }
}

impl RewordConfig {
    /// The instruction for a notification rewrite: the configured one, or
    /// the built-in default when it is unset or blank.
    ///
    /// Blank is folded in here rather than refused at the edit, so a config
    /// hand-edited to `prompt = ""` behaves the same as one the settings
    /// window cleared -- both mean the default.
    pub fn notification_prompt(&self) -> &str {
        Self::or_default(&self.prompt, crate::reword::NOTIFICATION_PROMPT)
    }

    /// The same for an explicit `--reword`.
    pub fn request_prompt(&self) -> &str {
        Self::or_default(&self.request_prompt, crate::reword::REQUEST_PROMPT)
    }

    fn or_default<'a>(set: &'a Option<String>, fallback: &'a str) -> &'a str {
        match set {
            Some(p) if !p.trim().is_empty() => p,
            _ => fallback,
        }
    }

    /// The configured provider, or `None` if it is unset or unrecognised.
    ///
    /// Callers may not distinguish the two by the return value on purpose:
    /// both mean "there is no dialect to speak", and the two messages that
    /// *do* tell them apart are built where they are shown.
    pub fn resolved_provider(&self) -> Option<Provider> {
        Provider::parse(self.provider.as_deref()?)
    }

    /// The token cap for one rewrite: three times the longest text the
    /// feature accepts.
    ///
    /// Generous against a strict character ceiling in the guard, and
    /// generous on purpose -- a tight cap truncates mid-sentence, and a
    /// truncated sentence passes a length check and gets *spoken*, while a
    /// generous one means an over-long answer arrives complete and is
    /// rejected whole.
    ///
    /// Here, in the config crate, rather than beside the request it is
    /// serialised into: the client sends it, the breaker's journal line
    /// quotes it and the settings window's Test row explains it, and three
    /// copies of one multiplier is three places to miss when it changes.
    ///
    /// This is **not** a bound on latency and is not meant to be one: at the
    /// 8-19 tok/s a CPU-only box sustains, 1200 tokens is a minute. The
    /// client's own ceiling is what bounds the request, as it already did
    /// when this was a fixed 256.
    ///
    /// With `max_chars` at `0` -- no input ceiling -- there is nothing to
    /// multiply, and the answer is a paragraph however long the input was,
    /// so the bound falls back to a constant: 1200, three times the 400
    /// that was the ceiling's old default.
    ///
    /// Saturating rather than wrapping: a hand-edited file reaches some
    /// callers before anything normalises it, and a wrapped cap of 3
    /// tokens would truncate every answer.
    pub fn max_tokens(&self) -> u32 {
        if self.max_chars == 0 {
            return 1200;
        }
        u32::try_from(self.max_chars.saturating_mul(3)).unwrap_or(u32::MAX)
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

/// Why the daemon must not start, or `None` to carry on.
///
/// `enabled = true` with no usable `provider` is the one configuration that
/// is a contradiction rather than a degradation: the user asked for
/// notifications to be rewritten automatically, and there is no dialect to
/// ask a provider in. Everything else degrades, because a hard exit
/// elsewhere costs more than it buys -- the settings window is reached
/// through the running daemon's tray, so a daemon that refuses to boot has
/// taken away the GUI this field would be set with. `--reword` on a
/// submission, a live config reload and the Test row all reach
/// `HttpRewriter::new` instead, which reports the same problem as
/// `NotConfigured` and speaks the text as written.
///
/// A free function over `RewordConfig` rather than a method, and returning
/// the sentence rather than printing it, so the rule is testable without
/// `main()` and without a process that exits.
pub fn reword_startup_refusal(cfg: &RewordConfig) -> Option<String> {
    // Keyed on `notifications` rather than on the `enabled` master, and the
    // difference is a boot the daemon must not refuse: `enabled` defaults to
    // true and the migration below turns it on for every existing config, so
    // a refusal keyed there would stop an unconfigured daemon from starting.
    // The contradiction this exists for is narrower than that -- the user
    // asked for notifications to be rewritten automatically and there is no
    // dialect to ask a provider in. Everything else degrades.
    if !(cfg.enabled && cfg.notifications) || cfg.resolved_provider().is_some() {
        return None;
    }
    let names = Provider::NAMES.join(", ");
    Some(match cfg.provider.as_deref() {
        None => format!(
            "reword.notifications = true but reword.provider is unset. \
             Set reword.provider to one of: {names}"
        ),
        Some(bad) => format!(
            "reword.notifications = true but reword.provider = {bad:?} is not \
             a provider this build knows. Set reword.provider to one of: {names}"
        ),
    })
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
    /// Submissions longer than this are refused; `0` -- the default --
    /// refuses nothing.
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
            max_chars: 0,
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
        // Which `[reword]` keys the document actually spells, read before
        // deserialising because `serde(default)` erases the difference
        // between "absent" and "set to the default".
        //
        // MIGRATION (2026-08-24). `[reword] enabled` used to mean what
        // `notifications` means now: "rewrite my notifications without being
        // asked". A document that spells `enabled` but not `notifications`
        // is therefore an old one, and its value belongs to the new field.
        // The master is set true, which is what makes the migration
        // behaviour-preserving in both directions: a user who had automatic
        // rewriting on keeps it, and a user who had it off but a provider
        // configured keeps `say --reword` working.
        //
        // One-shot in practice rather than an inference that runs forever:
        // `Config::save_to` writes the whole struct, so the first write from
        // the settings window -- or from anything else -- leaves both keys
        // present and this arm is never taken again.
        let spelled = |key: &str| {
            toml::from_str::<toml::Value>(txt)
                .ok()
                .and_then(|v| v.get("reword").and_then(|r| r.get(key)).map(|_| ()))
                .is_some()
        };
        let migrate = spelled("enabled") && !spelled("notifications");

        match toml::from_str::<Config>(txt) {
            Ok(mut c) => {
                if migrate {
                    c.reword.notifications = c.reword.enabled;
                    c.reword.enabled = true;
                }
                // The one range this layer enforces itself, and only its
                // floor. Everything else out of range is a degradation the
                // daemon can report and carry on with
                // (`settings::model::normalize`), and both daemon entry
                // points do exactly that. `reword.timeout_ms` is different
                // in kind at the bottom of its range: it is handed to
                // `Duration::from_millis`, and a budget no provider can meet
                // is a feature that looks configured and behaves as if it
                // were off. There is deliberately no upper bound -- see
                // `REWORD_TIMEOUT_MIN_MS`. Enforced at the parse rather than
                // at the use because there are four `load_from` callers and
                // only two of them normalise.
                c.reword.timeout_ms = c.reword.timeout_ms.max(REWORD_TIMEOUT_MIN_MS);
                c.reword.request_timeout_ms =
                    c.reword.request_timeout_ms.max(REWORD_TIMEOUT_MIN_MS);
                // The same kind of range for the same kind of reason, and
                // the other half of the same interaction: a window that
                // closes before the notification that opened it has been
                // submitted inverts the two. Derived from the deadline just
                // clamped above, in that order, so the floor clears the
                // budget this config will actually run with. `0` is left
                // alone -- it is the off switch, not a short window. See
                // `notify_cooldown_min_secs`.
                if c.notifications.cooldown_secs != 0 {
                    c.notifications.cooldown_secs = c
                        .notifications
                        .cooldown_secs
                        .max(notify_cooldown_min_secs(&c.reword));
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

    /// The defaults are a promise: nothing is rewritten, and the endpoint a
    /// user first sees is a local one. Pointing sayd at a remote endpoint
    /// should be an act, not something they inherit.
    ///
    /// The master is `true` and that is not the promise being broken: with
    /// `provider` unset nothing can be rewritten anyway, and `notifications`
    /// -- the switch that actually asks for a rewrite -- is off. See
    /// `RewordConfig::enabled`.
    #[test]
    fn reword_defaults_are_off_and_local() {
        let c = Config::default();
        assert!(!c.reword.notifications, "nothing is rewritten by default");
        assert!(c.reword.provider.is_none(), "and there is nothing to rewrite with");
        assert!(c.reword.enabled, "the master is on; the two above are why that is safe");
        assert_eq!(c.reword.base_url, "http://localhost:11434/v1");
        assert_eq!(c.reword.model, "llama3.2:3b");
        assert_eq!(c.reword.api_key, "");
        assert_eq!(c.reword.api_key_env, "SAYD_REWORD_API_KEY");
        assert_eq!(
            c.reword.timeout_ms, 1500,
            "a budget, not a measurement -- see the spec's §10"
        );
        assert_eq!(c.reword.max_chars, 0);
    }

    /// MIGRATION (2026-08-24): `[reword] enabled` used to mean what
    /// `notifications` means now, so an old document's value moves across
    /// and the master goes on.
    ///
    /// Behaviour-preserving in both directions is the whole requirement,
    /// and the two directions fail differently. A user who had automatic
    /// rewriting on must keep it -- mapping to `notifications = false`
    /// would silently switch off a feature they configured. A user who had
    /// it *off* with a provider set must keep `say --reword` working --
    /// mapping to `enabled = false` would silently break a keybind that
    /// worked yesterday, since `--reword` never consulted the old switch.
    #[test]
    fn an_old_file_moves_reword_enabled_to_notifications() {
        let (c, err) =
            Config::load_str("[reword]\nenabled = true\nprovider = \"llama-cpp\"\n");
        assert_eq!(err, None);
        assert!(
            c.reword.notifications,
            "the old key carried the automatic-rewrite meaning"
        );
        assert!(c.reword.enabled, "and the master goes on, so nothing changes");
    }

    #[test]
    fn an_old_file_with_automatic_rewriting_off_keeps_reword_available() {
        // This user configured a provider and turned automatic rewriting
        // off. `say --reword` worked for them and must keep working.
        let (c, err) =
            Config::load_str("[reword]\nenabled = false\nprovider = \"llama-cpp\"\n");
        assert_eq!(err, None);
        assert!(!c.reword.notifications);
        assert!(
            c.reword.enabled,
            "--reword never consulted the old switch, so the master must not \
             inherit its `false`"
        );
    }

    /// A document that spells both keys is a new one, and says what it
    /// means. Nothing is inferred from it -- which is what stops the
    /// migration from being an inference that runs forever: the settings
    /// window writes the whole struct, so the first save makes every later
    /// load take this path.
    #[test]
    fn a_new_file_carrying_both_keys_is_left_exactly_as_written() {
        for (toml, enabled, notifications) in [
            ("[reword]\nenabled = false\nnotifications = true\n", false, true),
            ("[reword]\nenabled = true\nnotifications = false\n", true, false),
            ("[reword]\nenabled = false\nnotifications = false\n", false, false),
        ] {
            let (c, err) = Config::load_str(toml);
            assert_eq!(err, None, "{toml:?}");
            assert_eq!(c.reword.enabled, enabled, "{toml:?}");
            assert_eq!(c.reword.notifications, notifications, "{toml:?}");
        }
    }

    /// `notifications` alone is also a new document: there is no old value
    /// to move, so the master keeps its default rather than being derived
    /// from a key that is not there.
    #[test]
    fn a_file_spelling_only_notifications_is_not_migrated() {
        let (c, err) = Config::load_str("[reword]\nnotifications = true\n");
        assert_eq!(err, None);
        assert!(c.reword.notifications);
        assert!(c.reword.enabled);
    }

    /// The migration runs before the cooldown floor is derived, because the
    /// floor reads the flag the migration moves. Out of order, an old file
    /// asking for automatic rewriting would load with a 1-second floor --
    /// the "rewording off" exemption -- and the ordering that floor exists
    /// to protect would be lost on exactly the configs that had it.
    #[test]
    fn the_migration_runs_before_the_cooldown_floor_is_derived() {
        let (c, err) = Config::load_str(
            "[notifications]\ncooldown_secs = 1\n\
             [reword]\nenabled = true\ntimeout_ms = 20000\n",
        );
        assert_eq!(err, None);
        assert!(c.reword.notifications, "the premise: the migration fired");
        assert_eq!(
            c.notifications.cooldown_secs,
            notify_cooldown_min_secs(&c.reword),
            "the floor must be the one the migrated flag implies, not the \
             1-second exemption an unmigrated read would have given"
        );
        assert_eq!(c.notifications.cooldown_secs, 21);
    }

    /// A config written before this milestone has no `[reword]` table at
    /// all, and must keep loading.
    #[test]
    fn a_config_without_the_reword_table_still_loads() {
        let (c, err) = Config::load_str("voice = \"am_fenrir\"\n");
        assert_eq!(err, None);
        assert_eq!(c.voice, "am_fenrir");
        assert!(!c.reword.notifications);
        assert_eq!(c.reword.timeout_ms, 1500);
    }

    /// A `timeout_ms` under the floor is raised by the *parse*, not only by
    /// the two callers that happen to normalise afterwards.
    ///
    /// The other direction is the point of this milestone and is asserted
    /// here too: a long deadline is the user's to set. Whoever runs a local
    /// model that needs thirty seconds gets thirty seconds, and every bound
    /// that used to be derived from a ceiling is now derived from this
    /// value.
    #[test]
    fn a_short_timeout_is_raised_by_the_parse_itself_and_a_long_one_is_kept() {
        let (c, err) = Config::load_str("[reword]\ntimeout_ms = 0\n");
        assert_eq!(err, None);
        assert_eq!(
            c.reword.timeout_ms, REWORD_TIMEOUT_MIN_MS,
            "a zero budget is not a way to switch the feature off -- \
             `enabled` is"
        );

        let (c, err) = Config::load_str("[reword]\ntimeout_ms = 30000\n");
        assert_eq!(err, None);
        assert_eq!(
            c.reword.timeout_ms, 30_000,
            "there is no ceiling: a local model that needs half a minute is \
             a real configuration, and no number this daemon could pick would \
             know that"
        );

        // A value in the ordinary range is not touched, which is the case
        // that matters for every honest config.
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
        let (c, err) = Config::load_str(
            "[notifications]\ncooldown_secs = 1\n\
             [reword]\nenabled = true\ntimeout_ms = 1500\n",
        );
        assert_eq!(err, None);
        assert_eq!(
            c.notifications.cooldown_secs,
            notify_cooldown_min_secs(&c.reword)
        );

        // The floor has to actually clear the budget it is derived from --
        // otherwise it is decoration -- and it has to keep doing that at a
        // deadline no ceiling constrains any more. Ten seconds of local
        // model is the case this milestone exists for.
        for timeout_ms in [200, 1500, 10_000, 45_000] {
            let (c, err) = Config::load_str(&format!(
                "[notifications]\ncooldown_secs = 1\n\
                 [reword]\nenabled = true\ntimeout_ms = {timeout_ms}\n",
            ));
            assert_eq!(err, None);
            assert!(
                std::time::Duration::from_secs(c.notifications.cooldown_secs)
                    > std::time::Duration::from_millis(timeout_ms),
                "a cooldown of {} does not clear a {timeout_ms} ms rewrite",
                c.notifications.cooldown_secs
            );
        }

        // With rewording off the floor is not a floor: nothing delays the
        // opener, so a 1-second window is honoured as written rather than
        // inflated over an interaction this config cannot have.
        let (c, err) = Config::load_str("[notifications]\ncooldown_secs = 1\n");
        assert_eq!(err, None);
        assert!(!c.reword.notifications);
        assert_eq!(c.notifications.cooldown_secs, 1);

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

    /// The field is absent from every config written before it existed, and
    /// absent is not the same as wrong: it parses, and the daemon decides what
    /// to do about it. See `reword_startup_refusal`.
    #[test]
    fn an_absent_provider_parses_as_none() {
        let (c, err) = Config::load_str("[reword]\nmodel = \"gemma\"\n");
        assert_eq!(err, None);
        assert_eq!(c.reword.provider, None);
        assert_eq!(c.reword.resolved_provider(), None);
        assert_eq!(c.reword.model, "gemma", "the rest of the table still parses");
    }

    /// The reason `provider` is an `Option<String>` and not an enum. As an enum
    /// this typo fails `toml::from_str` for the *whole document*, `load_str`
    /// hands back `Config::default()`, and every other setting in the user's
    /// file is silently discarded over one misspelt word.
    #[test]
    fn an_unrecognised_provider_does_not_discard_the_rest_of_the_config() {
        let (c, err) = Config::load_str(
            "[reword]\nprovider = \"llama.cpp\"\nmodel = \"gemma\"\nmax_chars = 512\n",
        );
        assert_eq!(err, None, "a bad provider is not a parse failure");
        assert_eq!(c.reword.provider.as_deref(), Some("llama.cpp"));
        assert_eq!(
            c.reword.resolved_provider(),
            None,
            "it is preserved verbatim and refused at use, not at parse"
        );
        assert_eq!(c.reword.model, "gemma");
        assert_eq!(c.reword.max_chars, 512);
    }

    /// The cap follows the longest text the feature accepts rather than sitting
    /// at a constant a long notification outgrows.
    #[test]
    fn the_token_cap_is_three_times_the_longest_text_accepted() {
        let mut c = RewordConfig::default();

        assert_eq!(c.max_chars, 0, "no ceiling by default");
        assert_eq!(c.max_tokens(), 1200, "unlimited input still bounds the answer");

        c.max_chars = 32;
        assert_eq!(c.max_tokens(), 96);

        c.max_chars = 2000;
        assert_eq!(c.max_tokens(), 6000);

        // A hand-edited file reaches some callers before anything
        // normalises it. Saturating rather than wrapping: a cap of 3
        // tokens would truncate every answer.
        c.max_chars = usize::MAX;
        assert_eq!(c.max_tokens(), u32::MAX);
    }

    #[test]
    fn the_two_provider_names_parse_and_nothing_else_does() {
        assert_eq!(Provider::parse("llama-cpp"), Some(Provider::LlamaCpp));
        assert_eq!(Provider::parse("generic"), Some(Provider::Generic));
        assert_eq!(Provider::parse(" generic "), Some(Provider::Generic));
        assert_eq!(Provider::parse("Generic"), None, "the value is a token, not prose");
        assert_eq!(Provider::parse("vllm"), None, "unverified dialects are not offered");
        assert_eq!(Provider::parse(""), None);
        assert_eq!(Provider::NAMES.len(), 2);
    }

    /// The only combination that refuses. `notifications = true` is the user
    /// asking for automatic rewording; without a provider it cannot be
    /// delivered, and a daemon that starts anyway is one that silently does
    /// nothing.
    ///
    /// The `enabled` master is held *on* throughout, so what is under test
    /// is the notification switch and not rewording being off wholesale.
    #[test]
    fn only_enabled_without_a_usable_provider_refuses_to_start() {
        let mut c = RewordConfig {
            enabled: true,
            notifications: false,
            provider: None,
            ..RewordConfig::default()
        };

        assert_eq!(
            reword_startup_refusal(&c),
            None,
            "the table is inert when disabled, and must not block a boot"
        );

        c.provider = Some("nonsense".into());
        assert_eq!(reword_startup_refusal(&c), None, "still disabled");

        c.notifications = true;
        c.provider = Some("llama-cpp".into());
        assert_eq!(reword_startup_refusal(&c), None);

        c.provider = Some("generic".into());
        assert_eq!(reword_startup_refusal(&c), None);

        c.provider = None;
        let unset = reword_startup_refusal(&c).expect("unset must refuse");

        c.provider = Some("llama.cpp".into());
        let wrong = reword_startup_refusal(&c).expect("unrecognised must refuse");
        assert_ne!(
            unset, wrong,
            "a user who typed something wrong and one who typed nothing need \
             different sentences"
        );
    }


    /// An unset, blank, or whitespace-only prompt is the built-in one, and
    /// the two asks resolve to *different* built-ins.
    ///
    /// Blank folding is what makes "select all, delete" in the settings
    /// window mean the same as Reset. Without it a cleared box would send an
    /// empty system message, which is the one configuration where the model
    /// has been told nothing at all about what it is for.
    #[test]
    fn a_blank_prompt_resolves_to_the_built_in_one_for_that_ask() {
        use crate::reword::{NOTIFICATION_PROMPT, REQUEST_PROMPT};
        let mut c = RewordConfig::default();
        assert_eq!(c.notification_prompt(), NOTIFICATION_PROMPT);
        assert_eq!(c.request_prompt(), REQUEST_PROMPT);
        assert_ne!(
            NOTIFICATION_PROMPT, REQUEST_PROMPT,
            "the two asks must not share one instruction; that is the point \
             of having two"
        );

        for blank in ["", "   ", "\n\t "] {
            c.prompt = Some(blank.to_string());
            c.request_prompt = Some(blank.to_string());
            assert_eq!(c.notification_prompt(), NOTIFICATION_PROMPT, "{blank:?}");
            assert_eq!(c.request_prompt(), REQUEST_PROMPT, "{blank:?}");
        }

        c.prompt = Some("say it shorter".into());
        assert_eq!(c.notification_prompt(), "say it shorter");
        assert_eq!(
            c.request_prompt(),
            REQUEST_PROMPT,
            "setting one must not move the other"
        );
    }

    /// An untouched prompt is absent from the file, so a build that improves
    /// the shipped wording reaches a user who never asked to own it.
    #[test]
    fn an_unset_prompt_is_not_written_to_the_file() {
        let c = Config::default();
        let out = toml::to_string_pretty(&c).expect("serialises");
        assert!(
            !out.contains("prompt"),
            "a default config must carry no prompt key: {out}"
        );

        let mut edited = Config::default();
        edited.reword.request_prompt = Some("summarise for the ear".into());
        let out = toml::to_string_pretty(&edited).expect("serialises");
        assert!(out.contains("request_prompt = \"summarise for the ear\""), "{out}");
        assert!(
            !out.contains("\nprompt"),
            "and the one still on its default stays absent: {out}"
        );

        let (back, err) = Config::load_str(&out);
        assert_eq!(err, None, "a config carrying a prompt must round-trip");
        assert_eq!(back.reword.request_prompt.as_deref(), Some("summarise for the ear"));
        assert_eq!(back.reword.prompt, None);
    }

    /// A refusal that does not say what to type is a refusal the user has to
    /// go and read source code about.
    #[test]
    fn the_refusal_names_the_field_and_every_value_it_accepts() {
        let c = RewordConfig {
            enabled: true,
            notifications: true,
            provider: None,
            ..RewordConfig::default()
        };
        let msg = reword_startup_refusal(&c).expect("must refuse");
        assert!(msg.contains("reword.provider"), "{msg}");
        for name in Provider::NAMES {
            assert!(msg.contains(name), "{name} must be offered: {msg}");
        }

        let c = RewordConfig {
            provider: Some("llama.cpp".into()),
            ..c
        };
        let msg = reword_startup_refusal(&c).expect("must refuse");
        assert!(
            msg.contains("llama.cpp"),
            "the rejected value must be quoted back so the typo is visible: {msg}"
        );
    }

    /// The settings window serialises the whole `Config` on every save. A `None`
    /// that serialises as a TOML value rather than as an absent key would either
    /// fail the write or write something that does not parse back.
    ///
    /// `to_string_pretty` because that is the call `Config::save_to` makes; a
    /// test that round-trips through a different serialiser is not testing the
    /// write that happens.
    #[test]
    fn an_absent_provider_round_trips_through_a_whole_config_write() {
        let mut c = Config::default();
        c.reword.provider = None;
        let text = toml::to_string_pretty(&c).expect("a default config must serialise");
        assert!(
            !text.contains("provider"),
            "an unset provider is an absent key, not an empty one: {text}"
        );
        let (back, err) = Config::load_str(&text);
        assert_eq!(err, None);
        assert_eq!(back.reword.provider, None);

        c.reword.provider = Some("llama-cpp".into());
        let text = toml::to_string_pretty(&c).expect("must serialise");
        let (back, err) = Config::load_str(&text);
        assert_eq!(err, None);
        assert_eq!(back.reword.resolved_provider(), Some(Provider::LlamaCpp));
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
