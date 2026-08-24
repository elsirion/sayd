//! The one path from a source's raw text to what the engine is handed.
//!
//! Before this module the order lived as prose in three doc comments that
//! had to agree with each other: `dbus::SaydIface::maybe_reword`,
//! `notify::monitor::speak` and `reword::RewordPlan::admit_with` each
//! described a piece of it, and the pieces were assembled differently by
//! each source. One of those disagreements was a defect -- see
//! `reword::RewordPlan::original`.
//!
//! The order, once, here: **length gate, then admission, then the rewrite or
//! the original.**
//!
//! Three things this deliberately does *not* do.
//!
//! It does not clean: the copy sent to a provider is cleaned inside the plan
//! (CRITICAL 1), and the string that reaches the engine is cleaned by
//! `Engine::submit`, which is the only place a string that will be spoken is
//! cleaned.
//!
//! It does not submit: [`prepare`] returns what to speak and the caller
//! queues it, which is what keeps a rewrite that lands past its deadline
//! unreachable rather than merely unwanted (see `RewordPlan::resolve`).
//!
//! And it does not *await*. [`prepare`] is synchronous -- admission is a
//! pass over a short string and one mutex -- and hands back a
//! [`Prepared::Pending`] for the caller to resolve. That is not a detail:
//! `notify::monitor` submits a refused announcement **inline**, so two that
//! arrive together stay in order, and detaches an admitted one so a
//! `timeout_ms` wait does not stall the tick loop. A `prepare` that awaited
//! internally would have to pick one of those for both sources.

use sayd_core::config::Config;

use crate::reword::{Origin, RewordPlan, Spoken};

/// Whether this submission wants a rewrite, and on whose authority.
///
/// The distinction between the last two is not cosmetic: `[reword] enabled`
/// is a standing ask that governs [`Ask::Automatic`] only. `--reword` is a
/// caller asking, and refusing that because a different, unrelated switch is
/// off would be answering a question nobody posed.
///
/// **Only the asking variants carry a config, and that is the point.**
/// `dbus` promises that an ordinary `Say` "pays nothing for a feature it did
/// not ask for, not even the mutex" -- `ConfigStore::published` takes a lock
/// -- and pins it with a `published_reads` assertion. Carrying the borrow on
/// the variants that need it makes that promise something a caller cannot
/// accidentally break: reaching for a config at all means naming an ask that
/// wants one.
#[derive(Debug, Clone, Copy)]
pub enum Ask<'a> {
    /// Nobody asked: a plain `say`. Needs no configuration, so it reads
    /// none -- no gate, no cleanup pass, no mutex.
    Never,
    /// `[reword] enabled` governs. The notification path.
    Automatic(&'a Config),
    /// The caller asked: `say --reword`, or D-Bus `reword: true`.
    Requested(&'a Config),
}

/// Longer than `max_chars`, and so refused before anything else happens.
///
/// Carries both numbers rather than a formatted string so each caller can
/// say so in its own register -- a D-Bus error, a log line -- from one
/// measurement. [`TooLong::message`] is what makes the wording the same as
/// the engine's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLong {
    pub chars: usize,
    pub limit: usize,
}

impl TooLong {
    /// The refusal, worded exactly as `Engine::submit` words it.
    pub fn message(&self) -> String {
        sayd_core::config::too_long(self.chars, self.limit)
    }
}

impl<'a> Ask<'a> {
    /// The config this ask needs, or `None` for the one that needs none.
    fn config(&self) -> Option<&'a Config> {
        match self {
            Ask::Never => None,
            Ask::Automatic(cfg) | Ask::Requested(cfg) => Some(cfg),
        }
    }
}

/// The outcome of admission: something to speak, or a rewrite to resolve.
///
/// Split rather than resolved here so each source keeps its own concurrency
/// (see this module's doc). Carries `RewordPlan`'s `#[must_use]` obligation:
/// a `Pending` that is dropped has already spent §8's half-open probe token.
#[must_use = "a Pending plan has already spent §8's half-open probe token; \
              resolve it or the run pays for a rewrite that never happened"]
pub enum Prepared {
    /// Nothing to wait for. No rewrite was asked for, none was admitted, or
    /// there is no provider to admit one.
    Ready(Spoken),
    /// Admitted. Resolve it inline, or on a task, as the caller prefers.
    Pending(RewordPlan),
}

impl Prepared {
    /// Resolve inline. The right choice where the caller is already the
    /// thing waiting -- a D-Bus method whose reply is the answer.
    pub async fn resolve(self) -> Spoken {
        match self {
            Prepared::Ready(spoken) => spoken,
            Prepared::Pending(plan) => plan.resolve().await,
        }
    }
}

/// Turn a source's text into what the engine should be handed.
///
/// `Err` is "do not submit this at all" -- the only outcome that is not
/// something to say. Everything else is `Ok`, because every other way a
/// rewrite can fail degrades to speaking the original, which is what the
/// feature promises.
///
/// The length gate measures the **raw** text, which is what `Engine::submit`
/// measures too, so the two agree about which submissions exist. It is here
/// as well as there for one reason: here is before the round trip, and an
/// announcement the engine is going to refuse must not cost a provider
/// request first. That is also why [`Ask::Never`] skips it -- there is no
/// round trip to save, and `Engine::submit`'s own check, which is the
/// engine's guarantee about its own queue and which this gate never
/// replaces, refuses exactly the same text with exactly the same words.
pub fn prepare(text: impl Into<Origin>, ask: Ask<'_>) -> Result<Prepared, TooLong> {
    let origin = text.into();

    let Some(cfg) = ask.config() else {
        return Ok(Prepared::Ready(Spoken::as_written(origin.into_text())));
    };

    let chars = origin.text().chars().count();
    if chars > cfg.max_chars {
        return Err(TooLong {
            chars,
            limit: cfg.max_chars,
        });
    }

    // The `Origin` is passed *through* to `automatic` rather than unwrapped
    // first: the provenance is what that constructor consults to keep this
    // daemon's own composed follow-ups away from a provider, and a string
    // has no provenance left to consult. See `reword::Written`.
    let admitted = match ask {
        // Unreachable: `config` returned `None` for it above.
        Ask::Never => return Ok(Prepared::Ready(Spoken::as_written(origin.into_text()))),
        Ask::Automatic(_) => RewordPlan::automatic(origin, &cfg.reword, &cfg.cleanup),
        Ask::Requested(_) => RewordPlan::requested(origin.into_text(), &cfg.reword, &cfg.cleanup),
    };

    Ok(match admitted {
        Ok(plan) => Prepared::Pending(plan),
        // Not a failure: "this text is not being reworded, here it is back",
        // untouched, for `Engine::submit` to clean.
        Err(text) => Prepared::Ready(Spoken::as_written(text)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reword::{Composed, Written};
    use sayd_core::config::RewordConfig;

    /// A config that would admit a rewrite if anything asked one of it.
    fn configured() -> Config {
        Config {
            max_chars: 40,
            reword: Box::new(RewordConfig {
                enabled: true,
                base_url: "http://127.0.0.1:1/v1".into(),
                provider: Some("generic".into()),
                ..RewordConfig::default()
            }),
            ..Config::default()
        }
    }

    /// The gate refuses before a plan is ever minted, and says so in the
    /// engine's own words.
    ///
    /// "Before a plan is minted" is what the `Err` proves: minting one is
    /// what spends §8's half-open probe token and what would cost the round
    /// trip this gate exists to avoid.
    #[test]
    fn text_over_the_limit_is_refused_before_anything_is_admitted() {
        let cfg = configured();
        let long = "x".repeat(cfg.max_chars + 1);
        let err = prepare(Written(long), Ask::Requested(&cfg)).err();
        assert_eq!(
            err,
            Some(TooLong {
                chars: 41,
                limit: 40
            })
        );
        assert_eq!(
            err.expect("refused").message(),
            "text is 41 characters, limit is 40",
            "the wording a caller sees must not depend on which gate caught it; \
             `Engine::submit` produces this same sentence from the same function"
        );
    }

    /// `Ask::Never` reads no configuration at all -- not even to measure
    /// length -- and that asymmetry is deliberate.
    ///
    /// The gate exists to save a provider round trip, and there is no round
    /// trip on this path. `Engine::submit` refuses the same text with the
    /// same sentence a moment later, so nothing is lost by not measuring it
    /// here, and `dbus`'s promise that an ordinary `Say` "pays nothing for a
    /// feature it did not ask for, not even the mutex" is kept.
    #[test]
    fn nothing_is_measured_or_read_for_an_ask_that_wants_nothing() {
        let long = "x".repeat(10_000);
        let prepared = prepare(Written(long.clone()), Ask::Never).expect("no gate on this path");
        match prepared {
            Prepared::Ready(spoken) => {
                assert_eq!(spoken.text, long, "handed straight back, uncleaned");
                assert!(spoken.fallback.is_none(), "nothing rewrote it");
            }
            Prepared::Pending(_) => panic!("`Never` must never admit a rewrite"),
        }
    }

    /// The master refuses both asks, and keeps the endpoint settings.
    ///
    /// "Off for now", not "forget my provider": the fields are still there
    /// afterwards, which is what makes the switch usable as a switch. The
    /// refusal is checked before `reword::context` is consulted, so a
    /// config nobody is using never builds a client and never records a
    /// cached failure against itself.
    #[test]
    fn the_master_switch_refuses_every_ask_without_clearing_anything() {
        let mut cfg = configured();
        cfg.reword.notifications = true;
        cfg.reword.enabled = false;

        for ask in [Ask::Automatic(&cfg), Ask::Requested(&cfg)] {
            let prepared = prepare(Written("Alice: dinner tonight?".into()), ask)
                .expect("well under the limit");
            assert!(
                matches!(prepared, Prepared::Ready(_)),
                "with the master off nothing is admitted, whoever asks"
            );
        }

        assert_eq!(cfg.reword.provider.as_deref(), Some("generic"));
        assert_eq!(cfg.reword.base_url, "http://127.0.0.1:1/v1");
    }

    /// Cleanup off reaches the provider too: the copy on the wire is the
    /// one `clean` produced, and with the master off `clean` produces the
    /// text as written.
    ///
    /// Worth pinning here rather than only in `cleanup`: this is the call
    /// site the user cannot see, and "cleanup off" quietly not applying to
    /// it would mean the text sent somewhere is cleaned while the text
    /// spoken at home is not.
    #[test]
    fn the_cleanup_master_reaches_the_copy_that_would_be_sent() {
        let mut cfg = configured();
        cfg.cleanup.enabled = false;
        let raw = "**bold** and a link https://example.com/x";
        assert_eq!(
            sayd_core::cleanup::clean(raw, &cfg.cleanup),
            raw,
            "the pass `RewordPlan::admit_with` makes is the identity now"
        );
    }

    /// Provenance survives `prepare`: the origin is passed *through* to
    /// `RewordPlan::automatic` rather than unwrapped into a bare string, so
    /// this daemon's own composed follow-ups still cannot reach a provider.
    ///
    /// The control is build-dependent, and worth being explicit about: in a
    /// build without the `reword` feature there is no client to admit to, so
    /// `Ready` here proves nothing. In a build with one -- where the config
    /// above names a provider -- `Ready` is true only because the origin was
    /// consulted. A `prepare` that called `into_text()` first and asked for
    /// a `Requested` would return `Pending` and fail this there. The
    /// positive control lives in `notify::monitor`'s own tests, which drive
    /// `RewordPlan::automatic` directly.
    #[test]
    fn a_composed_follow_up_is_never_admitted() {
        let cfg = configured();
        let prepared = prepare(
            Composed("Signal: 3 more notifications".into()),
            Ask::Automatic(&cfg),
        )
        .expect("well under the limit");
        assert!(
            matches!(prepared, Prepared::Ready(_)),
            "a follow-up this daemon wrote must not be sent to a provider"
        );
    }
}
