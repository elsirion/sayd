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
pub struct Config {
    pub voice: String,
    pub speed: f32,
    /// `fp32` | `fp16` | `q8`. Measured: fp32 RTF 4.78, fp16 4.66, q8 1.40.
    pub model: String,
    /// Measured peak at 8; 16 and 24 both regress.
    pub threads: usize,
    /// Seconds of an empty queue before the ~1.27 GB ORT session is dropped.
    pub idle_unload_secs: u64,
    pub muted: bool,
    /// Submissions longer than this are refused.
    pub max_chars: usize,
    pub cleanup: CleanupConfig,
    pub chunking: ChunkConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            voice: "af_heart".into(),
            speed: 1.0,
            model: "fp32".into(),
            threads: 8,
            idle_unload_secs: 600,
            muted: false,
            max_chars: 20_000,
            cleanup: CleanupConfig::default(),
            chunking: ChunkConfig::default(),
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
        match toml::from_str(&txt) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
        }
    }

    /// Write atomically: a temp file in the same directory, then rename. The
    /// caller is responsible for ignoring the resulting inotify event.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let txt = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, txt)?;
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
}
