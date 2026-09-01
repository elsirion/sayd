//! The settings surface, declared as data.
//!
//! Every row in the window used to be sixty hand-written lines repeating one
//! shape: build the widget, show the config's value, register a redraw
//! closure with `Ui::row`, connect a handler that calls `Ui::apply`, add it
//! to the group. `window.rs` carried about thirty copies of that, which is
//! thirty places for one of the five steps to be missing.
//!
//! This is the generalisation of a pattern the file already had:
//! `CLEANUP_SWITCHES` and `NOTIFICATION_SWITCHES` were tables of
//! `(title, subtitle, get, set)` covering two groups. [`ROOT`] covers all of
//! them, and `window::render` is the one implementation of the five steps.
//!
//! **What is deliberately not described.** Six things in this window are not
//! a value bound to a config field, and pretending otherwise would cost more
//! than it saves: the Voice group's Test row and its download row, the Reword
//! group (its endpoint presets, its key-visibility rule and its asynchronous
//! Test result), the allowlist's entry-plus-list, and the two suggestion
//! groups with their icons. They stay hand-written behind [`Row::Custom`] and
//! [`Section::Custom`].
//!
//! The download row is the newest of them and the clearest case for the
//! escape hatch: it is an *action*, with a progress bar and a cancel, and
//! there is no config field anywhere it could be a view of. That is the bar.
//! A row that does read and write a setting and merely looks unusual is not
//! -- if one of those arrives, extend the descriptors, because `Custom`
//! growing for that reason is this module failing.

use gtk4 as gtk;
use sayd_core::config::Config;

use super::model::{
    COOLDOWN_MAX, COOLDOWN_MIN, COOLDOWN_STEP, IDLE_UNLOAD_MAX, IDLE_UNLOAD_MIN, IDLE_UNLOAD_STEP,
    MAX_CHARS_MAX, MAX_CHARS_MIN, MAX_CHARS_STEP, MODELS, SPEED_MAX, SPEED_MIN, SPEED_MODES,
    SPEED_STEP, THREADS_MAX, THREADS_MIN, THREADS_STEP,
};
use super::window::{Build, Ui};

/// Where a [`Row::Choice`]'s entries come from.
pub enum Options {
    /// A fixed table of `(value written to the config, label shown)`.
    Static(&'static [(&'static str, &'static str)]),
    /// A fixed table of `(value, note)` shown as `"value — note"`.
    ///
    /// The Model and Speed mode rows: their measured trade-off goes in the
    /// item text itself rather than in the row's subtitle, because it is
    /// what the user needs while the dropdown is *open* and they are
    /// choosing between the entries, not afterwards.
    Annotated(&'static [(&'static str, &'static str)]),
    /// Discovered at build time. Voices are one file each in the models
    /// directory, so the list is whatever is installed and cannot be a
    /// constant. Values and labels are the same string.
    Discovered(fn(&Ui) -> Vec<String>),
}

impl Options {
    /// `(value, label)` pairs, in the order the dropdown will show them.
    pub fn resolve(&self, ui: &Ui) -> Vec<(String, String)> {
        match self {
            Options::Static(table) => table
                .iter()
                .map(|(v, l)| ((*v).to_string(), (*l).to_string()))
                .collect(),
            Options::Annotated(table) => table
                .iter()
                .map(|(v, note)| ((*v).to_string(), format!("{v} — {note}")))
                .collect(),
            Options::Discovered(f) => f(ui).into_iter().map(|v| (v.clone(), v)).collect(),
        }
    }
}

/// One row: what it shows, and the one config field it is a view of.
///
/// `get`/`set` are function pointers rather than a trait or a macro for the
/// reason `CleanupGet`/`CleanupSet` were: the fields have no common
/// accessor, and a pair of one-line closures per field is the least
/// machinery that still lets one renderer serve every row.
pub enum Row {
    Bool {
        title: &'static str,
        subtitle: &'static str,
        get: fn(&Config) -> bool,
        set: fn(&mut Config, bool),
    },
    /// A number with a range. `f64` throughout because that is what
    /// `AdwSpinRow` speaks; the accessors do the narrowing, which is also
    /// where the one place per field that knows the real type lives.
    Int {
        title: &'static str,
        subtitle: &'static str,
        min: f64,
        max: f64,
        step: f64,
        /// How far one PageUp moves. Equal to `step` for every range small
        /// enough to cross by arrow clicks; larger where it is not.
        page: f64,
        digits: u32,
        get: fn(&Config) -> f64,
        set: fn(&mut Config, f64),
    },
    Choice {
        title: &'static str,
        options: Options,
        /// How to word the synthetic entry for a configured value this row
        /// does not offer. See `window::Combo` for why that entry exists.
        unknown: fn(&str) -> String,
        get: fn(&Config) -> String,
        set: fn(&mut Config, &str),
    },
    /// A widget no descriptor describes. See this module's doc.
    Custom(fn(&Build) -> gtk::Widget),
}

impl Row {
    /// The row's title, for the tree-integrity tests. `None` for a `Custom`,
    /// whose widget is built rather than declared.
    ///
    /// Test-only: the renderer reads the title out of the variant it is
    /// already matching on, so nothing in a shipped build needs to ask.
    #[cfg(test)]
    pub fn title(&self) -> Option<&'static str> {
        match self {
            Row::Bool { title, .. } | Row::Int { title, .. } | Row::Choice { title, .. } => {
                Some(title)
            }
            Row::Custom(_) => None,
        }
    }
}

pub struct Group {
    /// `None` for a group that carries a page's master switch: libadwaita's
    /// own pattern for one is an untitled group above the titled ones, and a
    /// heading over a single switch that repeats the page title reads as a
    /// second setting.
    pub title: Option<&'static str>,
    pub description: Option<&'static str>,
    pub rows: &'static [Row],
}

impl Group {
    /// A name for assertion messages. `Custom` groups never reach here.
    #[cfg(test)]
    pub fn name(&self) -> &'static str {
        self.title.unwrap_or("(master)")
    }
}

/// A sub-page, reached from a navigation row on the root page.
pub struct Page {
    pub title: &'static str,
    pub sections: &'static [Section],
    /// What the navigation row says underneath the title.
    ///
    /// Load-bearing rather than decorative: it is what keeps the root page a
    /// status view rather than a menu. Without it this trades one long page
    /// for four short ones plus the clicks to reach them.
    pub summary: fn(&Config) -> String,
}

/// One `AdwPreferencesGroup`, described or hand-built.
pub enum Section {
    Described(Group),
    /// A whole group no descriptor describes. See this module's doc.
    Custom(fn(&Build) -> adw::PreferencesGroup),
    /// A group of navigation rows, each opening a sub-page.
    Links {
        title: &'static str,
        pages: &'static [&'static Page],
    },
}

/// The Cleanup sub-page: the master, then the transforms it governs.
pub static CLEANUP: Page = Page {
    title: "Cleanup",
    sections: &[
        Section::Described(Group {
            title: None,
            description: None,
            rows: &[Row::Bool {
                title: "Clean up text",
                subtitle: "Tidy every submission before it is spoken",
                get: |c| c.cleanup.enabled,
                set: |c, v| c.cleanup.enabled = v,
            }],
        }),
        Section::Described(Group {
        title: Some("Text cleanup"),
        description: Some("Applied to every submission before it is spoken"),
        // In the order spec §8 lists the transforms.
        rows: &[
            Row::Bool {
                title: "Collapse whitespace",
                subtitle: "Runs of spaces and blank lines become a single space",
                get: |c| c.cleanup.collapse_whitespace,
                set: |c, v| c.cleanup.collapse_whitespace = v,
            },
            Row::Bool {
                title: "Rejoin hyphenation",
                subtitle: "Reunite words a line break split with a hyphen",
                get: |c| c.cleanup.rejoin_hyphenation,
                set: |c, v| c.cleanup.rejoin_hyphenation = v,
            },
            Row::Bool {
                title: "Strip Markdown",
                subtitle: "Drop emphasis, heading and link syntax instead of reading it out",
                get: |c| c.cleanup.strip_markdown,
                set: |c, v| c.cleanup.strip_markdown = v,
            },
            Row::Bool {
                title: "Drop code blocks",
                subtitle: "Skip fenced and indented code rather than speaking it",
                get: |c| c.cleanup.drop_code_blocks,
                set: |c, v| c.cleanup.drop_code_blocks = v,
            },
            Row::Bool {
                title: "Spell out acronyms",
                subtitle: "Read TTS as T-T-S rather than as a word",
                get: |c| c.cleanup.spell_acronyms,
                set: |c, v| c.cleanup.spell_acronyms = v,
            },
            Row::Choice {
                title: "URLs",
                options: Options::Static(&[
                    ("link", "Say “link”"),
                    ("domain", "Say the domain"),
                    ("keep", "Read the whole URL"),
                ]),
                // Unreachable in practice -- `UrlPolicy` is an enum, so the
                // table above covers every value it can take -- but a
                // wording that reads as nonsense would be worse than one
                // that never appears.
                unknown: |p| format!("‘{p}’ — not a URL setting this build knows"),
                get: |c| format!("{:?}", c.cleanup.urls).to_lowercase(),
                set: |c, v| {
                    if let Some(p) = super::window::url_policy_named(v) {
                        c.cleanup.urls = p;
                    }
                },
            },
        ],
    }),
    ],
    summary: |c| {
        if !c.cleanup.enabled {
            return "Off".into();
        }
        let on = [
            c.cleanup.collapse_whitespace,
            c.cleanup.rejoin_hyphenation,
            c.cleanup.strip_markdown,
            c.cleanup.drop_code_blocks,
            c.cleanup.spell_acronyms,
        ]
        .iter()
        .filter(|b| **b)
        .count();
        let urls = match c.cleanup.urls {
            sayd_core::config::UrlPolicy::Link => "say \u{201c}link\u{201d}",
            sayd_core::config::UrlPolicy::Domain => "say the domain",
            sayd_core::config::UrlPolicy::Keep => "read the whole URL",
        };
        format!("{on} of 5 transforms on \u{b7} URLs: {urls}")
    },
};

/// The Rewording sub-page.
///
/// The master is described; everything under it is the hand-built Reword
/// group, whose endpoint presets, key-visibility rule and asynchronous Test
/// result are not values bound to config fields.
pub static REWORDING: Page = Page {
    title: "Rewording",
    sections: &[
        Section::Described(Group {
            title: None,
            description: None,
            rows: &[Row::Bool {
                title: "Reword text",
                subtitle: "Off keeps the endpoint below; nothing is rewritten, \
                           not even an explicit --reword",
                get: |c| c.reword.enabled,
                set: |c, v| c.reword.enabled = v,
            }],
        }),
        Section::Custom(super::window::reword_group),
    ],
    summary: |c| {
        if !c.reword.enabled {
            return "Off".into();
        }
        // The provider as the file spells it: `Provider` has no display
        // name of its own, and the string the user typed is the one they
        // will recognise.
        match (c.reword.resolved_provider(), c.reword.provider.as_deref()) {
            (Some(_), Some(name)) => format!("{} via {name}", c.reword.model),
            _ => "No provider set".into(),
        }
    },
};

/// The `say` sub-page: the submission limit that belongs to it.
///
/// **No master switch.** It was in the first sketch of this hierarchy and it
/// does not earn its keep: notifications arrive uninvited, so a switch for
/// them is worth having, but `say` only runs when the user runs it.
/// Switching it off would mean `Say` returning a D-Bus error naming a
/// setting -- a new failure mode in exchange for nothing.
pub static SAY: Page = Page {
    title: "say command",
    sections: &[Section::Described(Group {
        title: None,
        description: Some("Text submitted with the `say` command, a hotkey, or over D-Bus"),
        rows: &[
            Row::Int {
                title: "Long-text guard",
                subtitle: "Refuse submissions longer than this many characters",
                min: MAX_CHARS_MIN,
                max: MAX_CHARS_MAX,
                step: MAX_CHARS_STEP,
                page: MAX_CHARS_STEP,
                digits: 0,
                get: |c| c.max_chars as f64,
                set: |c, v| c.max_chars = v as usize,
            },
        ],
    })],
    summary: |c| format!("Up to {} characters", c.max_chars),
};

/// The Notifications sub-page: the switches, then the allowlist and the two
/// pickers that feed it.
pub static NOTIFICATIONS: Page = Page {
    title: "Notifications",
    sections: &[
        Section::Described(Group {
        title: Some("Notifications"),
        description: Some(
            "Takes effect at once: turning this on starts watching the session bus",
        ),
        // In the order spec §6 lists them.
        //
        // The rows below "Speak notifications", and the allowlist further
        // down, stay *sensitive* when it is off. Deliberately: those values
        // are not meaningless while announcements are off, they are merely
        // not in effect, and a dimmed row says "unset", which would be a lie
        // about what the config holds. Curating the list with announcements
        // off is also a real way to use this.
        rows: &[
            Row::Bool {
                title: "Speak notifications",
                subtitle: "Announce desktop notifications from the applications listed below; \
                           they are still shown as usual",
                get: |c| c.notifications.enabled,
                set: |c, v| c.notifications.enabled = v,
            },
            Row::Bool {
                title: "Say the application name",
                subtitle: "Announce “Signal: Ada: dinner?” rather than the summary on its own",
                get: |c| c.notifications.speak_app_name,
                set: |c, v| c.notifications.speak_app_name = v,
            },
            Row::Bool {
                title: "Say the body",
                subtitle: "Read the body after the summary; many applications only restate \
                           the summary there",
                get: |c| c.notifications.speak_body,
                set: |c, v| c.notifications.speak_body = v,
            },
            Row::Int {
                title: "Cooldown",
                // The subtitle has to spend its words on `0`, which is the
                // one value here that does not mean what a "seconds between
                // X" row usually means: it is not "no wait", it turns rate
                // limiting off entirely. Left unsaid, `0` reads like the
                // *least* chatty setting rather than the most.
                subtitle: "Seconds before the same application is announced again; \
                           0 speaks every notification",
                min: COOLDOWN_MIN,
                max: COOLDOWN_MAX,
                step: COOLDOWN_STEP,
                page: COOLDOWN_STEP,
                digits: 0,
                get: |c| c.notifications.cooldown_secs as f64,
                set: |c, v| c.notifications.cooldown_secs = v as u64,
            },
        ],
    }),
        Section::Custom(super::window::allowlist_group),
        Section::Custom(super::window::seen_suggestions_group),
        Section::Custom(super::window::curated_suggestions_group),
    ],
    summary: |c| {
        if !c.notifications.enabled {
            return "Off".into();
        }
        let apps = c.notifications.allow.len();
        let apps = match apps {
            0 => "no applications yet".to_string(),
            1 => "1 application".to_string(),
            n => format!("{n} applications"),
        };
        match c.notifications.cooldown_secs {
            0 => format!("On \u{b7} {apps} \u{b7} every notification"),
            n => format!("On \u{b7} {apps} \u{b7} {n} s cooldown"),
        }
    },
};

/// The root page: what is worth seeing without navigating, and the way in to
/// everything else.
///
/// The navigation rows sit directly here under a group heading rather than
/// behind an intermediate page each: a page whose entire content is two more
/// navigation rows is a click that shows nothing.
pub static ROOT: &[Section] = &[
    Section::Described(Group {
        title: Some("Voice and speed"),
        description: None,
        rows: &[
            Row::Choice {
                title: "Voice",
                options: Options::Discovered(|ui| ui.voices()),
                // `sayd` already warns about a configured voice with no pack
                // at startup; saying it again here is what keeps the row from
                // silently showing some other voice as if it were the
                // configured one.
                unknown: |v| format!("‘{v}’ — no voice pack installed"),
                get: |c| c.voice.clone(),
                set: |c, v| c.voice = v.to_string(),
            },
            // Directly under the dropdown it fills, and hidden the moment
            // that dropdown has anything in it. A fresh install opens this
            // window to an empty Voice row and a subtitle saying the
            // configured voice has no pack; the answer to that belongs on
            // the next line, not on a page the user has to go looking for.
            Row::Custom(super::window::voice_download_row),
            Row::Int {
                title: "Speed",
                subtitle: "Playback rate for every utterance",
                min: SPEED_MIN as f64,
                max: SPEED_MAX as f64,
                step: SPEED_STEP,
                page: SPEED_STEP,
                digits: 2,
                get: |c| c.speed as f64,
                set: |c, v| c.speed = v as f32,
            },
            Row::Choice {
                title: "Speed mode",
                options: Options::Annotated(&SPEED_MODES),
                unknown: |m| format!("‘{m}’ — not a speed mode this build knows"),
                get: |c| c.speed_mode.clone(),
                set: |c, v| c.speed_mode = v.to_string(),
            },
            Row::Custom(super::window::voice_test_row),
        ],
    }),
    Section::Described(Group {
        title: Some("Engine"),
        description: Some(
            "Changes take effect on the next utterance; switching model reloads the session",
        ),
        rows: &[
            Row::Choice {
                title: "Model",
                options: Options::Annotated(&MODELS),
                unknown: |m| format!("‘{m}’ — not a model this build knows"),
                get: |c| c.model.clone(),
                set: |c, v| c.model = v.to_string(),
            },
            Row::Int {
                title: "Threads",
                subtitle: "ONNX Runtime intra-op threads; measured peak at 8",
                min: THREADS_MIN,
                max: THREADS_MAX,
                step: THREADS_STEP,
                page: THREADS_STEP,
                digits: 0,
                get: |c| c.threads as f64,
                set: |c, v| c.threads = v as usize,
            },
            Row::Int {
                title: "Idle unload",
                subtitle: "Seconds of silence before the ~1.27 GB session is dropped; 0 never unloads",
                min: IDLE_UNLOAD_MIN,
                max: IDLE_UNLOAD_MAX,
                step: IDLE_UNLOAD_STEP,
                page: IDLE_UNLOAD_STEP,
                digits: 0,
                get: |c| c.idle_unload_secs as f64,
                set: |c, v| c.idle_unload_secs = v as u64,
            },
        ],
    }),
    Section::Links {
        title: "Text",
        pages: &[&CLEANUP, &REWORDING],
    },
    Section::Links {
        title: "Sources",
        pages: &[&SAY, &NOTIFICATIONS],
    },
];

/// Every sub-page, for the tests that check the tree is whole.
/// Every described group in the whole tree, root page and sub-pages alike.
///
/// Shared by this module's tests and by `window`'s, so a group that moves to
/// a sub-page does not quietly fall out of either one's coverage.
#[cfg(test)]
pub fn described_groups() -> impl Iterator<Item = &'static Group> {
    fn from(sections: &'static [Section]) -> impl Iterator<Item = &'static Group> {
        sections.iter().filter_map(|s| match s {
            Section::Described(g) => Some(g),
            Section::Custom(_) | Section::Links { .. } => None,
        })
    }
    from(ROOT).chain(PAGES.iter().flat_map(|p| from(p.sections)))
}

#[cfg(test)]
pub static PAGES: &[&Page] = &[&CLEANUP, &REWORDING, &SAY, &NOTIFICATIONS];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sub-page is reachable from the root, and every navigation row
    /// names a page that exists.
    ///
    /// A `Page` declared and never linked is a page nobody can open, which
    /// nothing else in this file would notice: it compiles, it has rows, and
    /// its rows even redraw.
    #[test]
    fn every_page_is_linked_from_the_root_exactly_once() {
        let mut linked: Vec<&'static str> = Vec::new();
        for section in ROOT {
            if let Section::Links { pages, .. } = section {
                for page in *pages {
                    assert!(
                        !linked.contains(&page.title),
                        "{} is linked twice",
                        page.title
                    );
                    linked.push(page.title);
                }
            }
        }
        let declared: Vec<&str> = PAGES.iter().map(|p| p.title).collect();
        for title in &declared {
            assert!(linked.contains(title), "{title} is declared but not linked");
        }
        for title in &linked {
            assert!(
                declared.contains(title),
                "{title} is linked but not in PAGES, so the tests below skip it"
            );
        }
    }

    /// Each summary renders from a known config to a known string.
    ///
    /// These are what make the root page a status view rather than a menu,
    /// so they are pinned as text rather than merely as "not empty".
    #[test]
    fn each_summary_says_what_its_page_holds() {
        let mut cfg = Config::default();

        assert_eq!(
            (CLEANUP.summary)(&cfg),
            "5 of 5 transforms on \u{b7} URLs: say \u{201c}link\u{201d}"
        );
        cfg.cleanup.strip_markdown = false;
        assert_eq!(
            (CLEANUP.summary)(&cfg),
            "4 of 5 transforms on \u{b7} URLs: say \u{201c}link\u{201d}"
        );
        cfg.cleanup.enabled = false;
        assert_eq!((CLEANUP.summary)(&cfg), "Off");

        // The shipped default: the master is on, and there is no provider.
        assert_eq!((REWORDING.summary)(&cfg), "No provider set");
        cfg.reword.provider = Some("llama-cpp".into());
        cfg.reword.model = "qwen3:32b".into();
        assert_eq!((REWORDING.summary)(&cfg), "qwen3:32b via llama-cpp");
        // A provider the file names but this build does not know is not one:
        // `resolved_provider` is what the daemon will actually use.
        cfg.reword.provider = Some("llama.cpp".into());
        assert_eq!((REWORDING.summary)(&cfg), "No provider set");
        cfg.reword.enabled = false;
        assert_eq!((REWORDING.summary)(&cfg), "Off");

        assert_eq!(
            (SAY.summary)(&cfg),
            format!("Up to {} characters", Config::default().max_chars)
        );

        assert_eq!((NOTIFICATIONS.summary)(&cfg), "Off");
        cfg.notifications.enabled = true;
        // `0` is the off switch for rate limiting, not a short window, and
        // the summary has to say so in words for the same reason the
        // Cooldown row's subtitle does.
        cfg.notifications.cooldown_secs = 0;
        assert_eq!(
            (NOTIFICATIONS.summary)(&cfg),
            "On \u{b7} no applications yet \u{b7} every notification"
        );
        cfg.notifications.allow = vec!["Signal".into()];
        cfg.notifications.cooldown_secs = 5;
        assert_eq!(
            (NOTIFICATIONS.summary)(&cfg),
            "On \u{b7} 1 application \u{b7} 5 s cooldown"
        );
        cfg.notifications.allow.push("Element".into());
        assert_eq!(
            (NOTIFICATIONS.summary)(&cfg),
            "On \u{b7} 2 applications \u{b7} 5 s cooldown"
        );
    }

    /// No two rows in a group share a title.
    ///
    /// The window's own test helpers find rows by title, and so does anyone
    /// reading the window: two "Model" rows in one group is a row that
    /// cannot be referred to.
    #[test]
    fn every_row_title_in_a_group_is_distinct() {
        for group in described_groups() {
            let mut seen: Vec<&str> = Vec::new();
            for title in group.rows.iter().filter_map(Row::title) {
                assert!(
                    !seen.contains(&title),
                    "{}: two rows titled {title:?}",
                    group.name()
                );
                seen.push(title);
            }
        }
    }

    /// Every `Choice` row's configured default is one of the entries it
    /// offers.
    ///
    /// A default the row cannot show is not a crash -- `Combo` grows a
    /// synthetic entry saying so -- but on a *shipped default* it means
    /// every new user opens the window to a row explaining that their
    /// configuration is unrecognised.
    #[test]
    fn a_static_choice_offers_the_shipped_default() {
        let cfg = Config::default();
        for group in described_groups() {
            for row in group.rows {
                let Row::Choice {
                    title,
                    options: Options::Static(table) | Options::Annotated(table),
                    get,
                    ..
                } = row
                else {
                    continue;
                };
                let value = get(&cfg);
                assert!(
                    table.iter().any(|(v, _)| *v == value),
                    "{}/{title}: the default {value:?} is not one of {:?}",
                    group.name(),
                    table.iter().map(|(v, _)| *v).collect::<Vec<_>>()
                );
            }
        }
    }

    /// Every described row round-trips: setting a value through `set` is
    /// what `get` then reports.
    ///
    /// This is the pin against a copy-paste pair that reads one field and
    /// writes another -- the failure the old hand-written rows made easy and
    /// which no compiler catches, since both fields have the same type.
    #[test]
    fn every_row_reads_back_what_it_writes() {
        for group in described_groups() {
            for row in group.rows {
                let mut cfg = Config::default();
                match row {
                    Row::Bool {
                        title, get, set, ..
                    } => {
                        let flipped = !get(&cfg);
                        set(&mut cfg, flipped);
                        assert_eq!(get(&cfg), flipped, "{}/{title}", group.name());
                    }
                    Row::Int {
                        title,
                        min,
                        max,
                        digits,
                        get,
                        set,
                        ..
                    } => {
                        // A value inside the row's own range and not the
                        // default, rounded to what the row can express.
                        let mut want = (min + max) / 2.0;
                        if *digits == 0 {
                            want = want.round();
                        }
                        set(&mut cfg, want);
                        let got = get(&cfg);
                        assert!(
                            (got - want).abs() < 0.01,
                            "{}/{title}: wrote {want}, read {got}",
                            group.name()
                        );
                    }
                    Row::Choice {
                        title,
                        options: Options::Static(table) | Options::Annotated(table),
                        get,
                        set,
                        ..
                    } => {
                        for (value, _) in table.iter() {
                            set(&mut cfg, value);
                            assert_eq!(&get(&cfg), value, "{}/{title}", group.name());
                        }
                    }
                    // Voices need a models directory; `Custom` has no
                    // accessors at all.
                    Row::Choice { .. } | Row::Custom(_) => {}
                }
            }
        }
    }
}
