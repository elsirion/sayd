//! Fetching the Kokoro weights and voice packs, for a machine that has
//! none.
//!
//! A fresh install has an empty models directory: the Voice dropdown offers
//! nothing, and `sayd` prints a warning telling the user to go and find
//! `scripts/fetch-models.sh` in a source tree they may not have. This module
//! is the other half of that script -- the same base URL, the same 29 voice
//! names -- reachable from the settings window instead.
//!
//! Nothing here draws anything, for the reason `model.rs` gives: the window
//! cannot run without a display, so every string, every number and every
//! decision that a test could pin lives on this side of the line. What the
//! window is left with is a subtitle, a fraction and a button label.
//!
//! **Only `model.onnx`.** The script fetches all three variants (fp32, fp16
//! and quantized) because it exists to populate a development tree; this
//! fetches the one the config's `model` setting defaults to. The other two
//! are 255 MB nobody asked for, and a user who later switches `model` to
//! `fp16` lands on the missing-model path that already exists for it.
//!
//! # What it promises
//!
//! - **Nothing half-written survives.** Every file is fetched to
//!   `<name>.part`, flushed to disk and only then renamed over its
//!   destination, so an interrupt, a failure or a power loss leaves either
//!   the complete file or no file. That matters most for `model.onnx`: a
//!   truncated one is not reported as a failed download, it surfaces days
//!   later as an ONNX protobuf parse error inside the engine thread.
//! - **A cancel stops the transfer**, rather than hiding the UI in front of
//!   one. [`download`] asks `cancel` before every chunk it reads, so the
//!   worst case is one chunk's worth of network wait.
//! - **A failure is a sentence.** No network, a 404, a full disk and a
//!   response far larger than it should be all come back as
//!   [`Outcome::Failed`] carrying a message, and leave the `.part` behind
//!   them deleted.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Where the packs come from, and the same URL `scripts/fetch-models.sh`
/// uses. Kept as one string with the paths appended, so the host the README
/// names is the host in the code.
pub const BASE_URL: &str =
    "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main";

/// The host, for the sentence shown before the user commits to 341 MB.
/// Derived from [`BASE_URL`] rather than written twice.
pub fn host() -> &'static str {
    BASE_URL
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or(BASE_URL)
}

/// Every voice pack the upstream repository publishes, and exactly the list
/// `scripts/fetch-models.sh` carries. Changing one without the other is how
/// the two silently diverge.
pub const VOICES: [&str; 29] = [
    "af",
    "af_alloy",
    "af_aoede",
    "af_bella",
    "af_heart",
    "af_jessica",
    "af_kore",
    "af_nicole",
    "af_nova",
    "af_river",
    "af_sarah",
    "af_sky",
    "am_adam",
    "am_echo",
    "am_eric",
    "am_fenrir",
    "am_liam",
    "am_michael",
    "am_onyx",
    "am_puck",
    "am_santa",
    "bf_alice",
    "bf_emma",
    "bf_isabella",
    "bf_lily",
    "bm_daniel",
    "bm_fable",
    "bm_george",
    "bm_lewis",
];

/// The sizes below are **measured, not estimated**: `Content-Length` from a
/// `HEAD` against every one of the 33 URLs, cross-checked against `stat` over
/// a completed installation. They exist so the window can say how large the
/// download is *before* it starts, which is the one number a user needs in
/// order to decide, and which asking the server at UI time would only produce
/// after 33 round trips the user would be waiting through.
///
/// They are not load-bearing for correctness. Each file's real
/// `Content-Length` replaces its constant as soon as the response arrives
/// (see [`download`]), so an upstream that reuploads a slightly different
/// `model.onnx` moves the progress bar, not the outcome.
pub const CONFIG_BYTES: u64 = 44;
pub const TOKENIZER_BYTES: u64 = 3_497;
pub const MODEL_BYTES: u64 = 325_532_232;

/// What 28 of the 29 packs weigh: `STYLE_ROWS * STYLE_DIM` f32s, which is
/// `510 * 256 * 4`. `sayd_kokoro::Kokoro::load_voice` validates exactly that
/// length, so this is the size a well-formed pack *has* rather than a
/// measurement that happens to agree.
const VOICE_BYTES: u64 = 510 * 256 * 4;

/// `af.bin` alone is 2,048 bytes larger -- 524,288, which is 512 style rows
/// rather than 510. Measured against upstream, not guessed.
///
/// **`sayd` cannot load it.** `sayd_kokoro::decode_pack` requires exactly
/// `STYLE_ROWS * STYLE_DIM` floats and rejects anything else as
/// `BadVoicePack`, so selecting `af` fails at synthesis time whether the pack
/// arrived through this button or through `scripts/fetch-models.sh`, which
/// has always fetched it too. It is downloaded anyway rather than quietly
/// skipped: that is a pre-existing disagreement between upstream's pack and
/// this decoder, it is not this module's to decide, and a download that
/// silently installs 28 of the 29 packs upstream publishes would be a second
/// surprise on top of the first. Written out as its own constant rather than
/// folded into an average because the per-file figures have to sum to the
/// advertised total, or the progress bar finishes somewhere other than at the
/// end.
const AF_BYTES: u64 = 524_288;

/// All 29 packs together: `28 * VOICE_BYTES + AF_BYTES`.
pub const VOICES_BYTES: u64 = 28 * VOICE_BYTES + AF_BYTES;

/// What one pack weighs, by name. See [`AF_BYTES`].
fn voice_bytes(name: &str) -> u64 {
    if name == "af" {
        AF_BYTES
    } else {
        VOICE_BYTES
    }
}

/// What the window shows before the first byte moves: 341 MB.
pub const TOTAL_BYTES: u64 = CONFIG_BYTES + TOKENIZER_BYTES + MODEL_BYTES + VOICES_BYTES;

/// How much larger than its measured size a file may arrive before the
/// transfer refuses it.
///
/// The body is untrusted for the reason `reword/http.rs` spells out at
/// length -- not because the host is hostile, but because a captive portal,
/// a caching proxy and a misconfigured CDN all serve bodies nobody
/// documented. Unbounded, "download the voices" is a way to fill the user's
/// home directory from the network. Doubling plus a megabyte is slack for a
/// genuine reupload (the largest file here would have to grow past 651 MB to
/// trip it) while still bounding the damage at roughly what was promised.
fn ceiling_for(expected: u64) -> u64 {
    expected.saturating_mul(2).saturating_add(1 << 20)
}

/// How much is read from the socket at a time, and therefore how long a
/// cancel can take to be noticed: one chunk's worth of network wait.
const CHUNK: usize = 64 * 1024;

/// How often the progress closure is called, in bytes transferred.
///
/// Per chunk would be 5,200 calls for `model.onnx` alone, each one a widget
/// update and a channel send on the other side of this seam. One per
/// mebibyte is 325 for the same file -- a visibly smooth bar at any speed a
/// 341 MB download is worth watching, and nothing the main loop notices.
/// The start of each file always reports, whatever this says, so the
/// filename in the subtitle is never stale.
const PROGRESS_STEP: u64 = 1 << 20;

/// One file to fetch: where it comes from, where it goes, and how large it
/// was when measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub url: String,
    /// Below the models directory. Also what the progress subtitle names,
    /// which is why it keeps the `voices/` prefix.
    pub relative: PathBuf,
    /// The measured size; see the constants above for what that is and is
    /// not good for.
    pub expected: u64,
}

impl Asset {
    /// What the subtitle calls this file: `voices/af_heart.bin`.
    pub fn name(&self) -> String {
        self.relative.to_string_lossy().into_owned()
    }
}

/// Everything a fresh install needs, in the order it is fetched.
///
/// Config and tokenizer first because they are three and a half kilobytes
/// between them: a wrong base URL, a proxy that intercepts, a machine with
/// no route -- every failure that is going to happen at all happens in the
/// first second, rather than 300 MB into `model.onnx`. The weights then, and
/// the packs last, because the packs are what the Voice dropdown reads and
/// finishing them is what makes the window's list change.
pub fn assets() -> Vec<Asset> {
    let file = |name: &str, remote: &str, expected: u64| Asset {
        url: format!("{BASE_URL}/{remote}"),
        relative: PathBuf::from(name),
        expected,
    };
    let mut out = vec![
        file("config.json", "config.json", CONFIG_BYTES),
        file("tokenizer.json", "tokenizer.json", TOKENIZER_BYTES),
        // `onnx/model.onnx` upstream, `model.onnx` here: `model_file_for` in
        // `kokoro_synth.rs` looks for the bare name beside `config.json`,
        // exactly as `scripts/fetch-models.sh` lays it out.
        file("model.onnx", "onnx/model.onnx", MODEL_BYTES),
    ];
    for voice in VOICES {
        out.push(Asset {
            url: format!("{BASE_URL}/voices/{voice}.bin"),
            relative: PathBuf::from("voices").join(format!("{voice}.bin")),
            expected: voice_bytes(voice),
        });
    }
    out
}

/// A number of bytes as the window says it: `341 MB`.
///
/// Decimal rather than binary units, which is what makes 340,682,781 read as
/// the 341 MB the README promises rather than as 325 MiB. Rounded to whole
/// units on purpose: this is a size to decide against, not a measurement,
/// and a bar that ticks through `128.4 MB` is a bar that is being read
/// instead of glanced at.
pub fn human_bytes(bytes: u64) -> String {
    if bytes < 1_000 {
        format!("{bytes} B")
    } else if bytes < 1_000_000 {
        format!("{} kB", (bytes + 500) / 1_000)
    } else {
        format!("{} MB", (bytes + 500_000) / 1_000_000)
    }
}

/// What the row says while nothing is running: the size, the host, and what
/// is actually being fetched.
///
/// The size is the whole point of the sentence. A button that starts a
/// 341 MB transfer without saying so is one a user on a metered connection
/// presses once and regrets.
pub fn offer_subtitle() -> String {
    format!(
        "{} from {}: the Kokoro-82M weights and {} voice packs",
        human_bytes(TOTAL_BYTES),
        host(),
        VOICES.len()
    )
}

/// What the row says between the click and the first byte.
///
/// A sentence of its own rather than leaving the offer up, because
/// connecting and resolving can take a second or two on a cold DNS cache,
/// and an unchanged row is a button that looks like it did not take the
/// press.
pub fn starting_subtitle() -> String {
    format!("Connecting to {}…", host())
}

/// How far along a transfer is. Produced by [`download`], rendered by the
/// window and by nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// The file being fetched right now, as [`Asset::name`] spells it.
    pub file: String,
    /// Bytes on disk across the whole set, including files that were
    /// already there.
    pub done: u64,
    /// Bytes expected across the whole set: [`TOTAL_BYTES`] at the start,
    /// corrected file by file as each real `Content-Length` arrives.
    pub total: u64,
}

impl Progress {
    /// What the row says while it runs.
    pub fn subtitle(&self) -> String {
        format!(
            "{} — {} of {}",
            self.file,
            human_bytes(self.done),
            human_bytes(self.total)
        )
    }

    /// Where the bar sits, clamped to the range a `GtkProgressBar` accepts.
    ///
    /// Clamped rather than trusted: `total` is corrected from the network,
    /// so a server that declares less than it sends could otherwise push
    /// this past 1.0.
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every file is in place.
    Complete,
    /// The user asked to stop, or the window that asked for this is gone.
    Cancelled,
    /// One file could not be fetched, and the sentence saying why.
    Failed(String),
}

impl Outcome {
    /// What the row says afterwards.
    ///
    /// `Complete` still returns a sentence even though the row that shows it
    /// is hidden on success: an outcome whose wording depends on who is
    /// asking is one that gets it wrong somewhere.
    pub fn subtitle(&self) -> String {
        match self {
            Outcome::Complete => format!("{} voice packs installed", VOICES.len()),
            // Says what was *not* left behind, because that is the question
            // a user who has just cancelled a 341 MB download actually has.
            Outcome::Cancelled => {
                format!("Cancelled. {}", offer_subtitle())
            }
            Outcome::Failed(e) => format!("Download failed: {e}"),
        }
    }
}

/// What a run reports, in the order it happens: any number of [`Progress`],
/// then exactly one [`Outcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Progress(Progress),
    Finished(Outcome),
}

/// One HTTP GET.
///
/// A trait and not a function so the whole of [`download`] -- the atomic
/// rename, the cancel check, the size ceiling, the progress arithmetic --
/// can be driven from a table of canned bodies with no network at all. The
/// same seam, and the same reason, as `crate::reword::Rewriter`.
pub trait Fetch {
    /// The body, and the `Content-Length` the server declared if it declared
    /// one. `limit` is a hard ceiling on what the returned reader will
    /// yield; see [`ceiling_for`].
    fn get(&self, url: &str, limit: u64) -> Result<Transfer, String>;
}

/// An open response body and what the server said about its size.
pub struct Transfer {
    pub declared: Option<u64>,
    pub body: Box<dyn Read>,
}

/// Fetch everything in [`assets`] into `models_dir`.
///
/// `cancel` is asked before every chunk, so stopping costs at most one
/// chunk's network wait. `report` is called at the start of each file and
/// then once per [`PROGRESS_STEP`]; it is deliberately *not* how
/// cancellation is signalled, because the caller needs to be able to stop a
/// transfer that is stalled between reports.
///
/// A file that is already present and non-empty is left alone and counted as
/// done -- the same rule `scripts/fetch-models.sh` follows, and what makes
/// pressing Download again after a failure resume rather than restart. It is
/// safe here in a way it is not for the script: nothing this function writes
/// is visible under its final name until it is complete.
pub fn download(
    models_dir: &Path,
    fetch: &dyn Fetch,
    cancel: &dyn Fn() -> bool,
    report: &mut dyn FnMut(Progress),
) -> Outcome {
    let assets = assets();
    // Corrected as each `Content-Length` arrives: `total` starts as the sum
    // of the measured constants and each file swaps its constant for what
    // the server (or, for a file already on disk, the filesystem) says.
    let mut total = TOTAL_BYTES;
    let mut done: u64 = 0;

    if let Err(e) = std::fs::create_dir_all(models_dir.join("voices")) {
        // `voices/` implies the parent, so one call covers both.
        return Outcome::Failed(format!(
            "could not create {}: {e}",
            models_dir.join("voices").display()
        ));
    }

    for asset in &assets {
        if cancel() {
            return Outcome::Cancelled;
        }
        let dest = models_dir.join(&asset.relative);
        // An existing file of length zero is not a download this function
        // made -- it never renames an empty `.part` into place -- so it is
        // treated as absent rather than as done.
        if let Ok(meta) = std::fs::metadata(&dest) {
            if meta.is_file() && meta.len() > 0 {
                total = total
                    .saturating_sub(asset.expected)
                    .saturating_add(meta.len());
                done += meta.len();
                report(Progress {
                    file: asset.name(),
                    done,
                    total,
                });
                continue;
            }
        }
        report(Progress {
            file: asset.name(),
            done,
            total,
        });
        match fetch_one(&dest, asset, fetch, cancel, &mut |written, declared| {
            // Handed to this closure exactly once, at the head of the file,
            // so it cannot correct the same estimate twice.
            if let Some(declared) = declared {
                total = total
                    .saturating_sub(asset.expected)
                    .saturating_add(declared);
            }
            report(Progress {
                file: asset.name(),
                done: done + written,
                total,
            });
        }) {
            Ok(Some(written)) => done += written,
            Ok(None) => return Outcome::Cancelled,
            Err(e) => return Outcome::Failed(format!("{}: {e}", asset.name())),
        }
    }
    Outcome::Complete
}

/// Fetch one file to `<dest>.part` and rename it over `dest`.
///
/// `Ok(Some(n))` is `n` bytes written and renamed, `Ok(None)` is a cancel,
/// and either way no `.part` is left behind: every exit below deletes it,
/// which is what makes "an interrupted download leaves no truncated file"
/// true for the *temporary* name as well as the final one.
///
/// `on_progress` is handed the bytes written so far and, on its first call
/// only, the length the server declared -- which is how the caller's running
/// total stops being an estimate for this file. Every later call passes
/// `None`, so a caller that corrects its total cannot correct it twice.
fn fetch_one(
    dest: &Path,
    asset: &Asset,
    fetch: &dyn Fetch,
    cancel: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Option<u64>, String> {
    let ceiling = ceiling_for(asset.expected);
    let mut transfer = fetch.get(&asset.url, ceiling)?;
    // Refused on the header, before a byte is read, which is the case that
    // matters: a `Content-Length` of 300 GB should cost one round trip, not
    // a full disk. A server that declares nothing, or lies, is caught by the
    // running check in `stream` instead.
    if let Some(declared) = transfer.declared {
        if declared > ceiling {
            return Err(format!(
                "the server offered {declared} bytes, far past the {ceiling} this file \
                 should need"
            ));
        }
    }

    // `config.json` -> `config.json.part`, `af_heart.bin` -> `af_heart.bin.part`.
    // Appended to the whole name rather than replacing the extension, so the
    // temporary file cannot collide with a real asset's name.
    let part = dest.with_extension(match dest.extension() {
        Some(ext) => format!("{}.part", ext.to_string_lossy()),
        None => "part".to_string(),
    });
    let mut file =
        std::fs::File::create(&part).map_err(|e| format!("creating {}: {e}", part.display()))?;

    let streamed = stream(
        &mut file,
        &mut *transfer.body,
        ceiling,
        transfer.declared,
        cancel,
        on_progress,
    );
    let written = match streamed {
        Ok(Some(written)) => written,
        // A cancel and a failure leave the same thing behind -- nothing.
        other => {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return other;
        }
    };

    // Flushed before the rename, not after: a rename is atomic with respect
    // to *this process* for free, but the promise is that a machine which
    // loses power mid-download comes back to either the whole file or no
    // file, and that needs the bytes on the platter before the directory
    // entry points at them. A few seconds on `model.onnx`, once, weighed
    // against a truncated model that reports itself weeks later as an ONNX
    // parse error from inside the engine thread.
    let flushed = file.sync_all();
    drop(file);
    if let Err(e) = flushed {
        let _ = std::fs::remove_file(&part);
        return Err(format!("flushing {}: {e}", part.display()));
    }
    if let Err(e) = std::fs::rename(&part, dest) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("renaming {} into place: {e}", part.display()));
    }
    on_progress(written, None);
    Ok(Some(written))
}

/// Copy `body` into `file`, stopping for a cancel and refusing a body that
/// runs past `ceiling`. `Ok(None)` is a cancel.
///
/// Split out from [`fetch_one`] so that every way out of the copy loop --
/// and there are five -- goes through one `.part` deletion rather than five.
fn stream(
    file: &mut std::fs::File,
    body: &mut dyn Read,
    ceiling: u64,
    declared: Option<u64>,
    cancel: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Option<u64>, String> {
    // The correction goes out before the first byte is read, so a long
    // transfer is measured against the size the server actually named
    // rather than against the constant, for its whole length.
    on_progress(0, declared);

    let mut buf = vec![0u8; CHUNK];
    let mut written: u64 = 0;
    let mut reported = 0u64;
    loop {
        // Before the read rather than after it, so a cancel pressed while
        // the socket is idle is honoured on the next chunk boundary rather
        // than after one more chunk has been paid for.
        if cancel() {
            return Ok(None);
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(format!("reading from the network: {e}")),
        };
        // `UreqFetch` already caps the reader at `ceiling`, so in the daemon
        // this is unreachable. It is here because [`Fetch`] is a trait: the
        // ceiling is this function's promise, not its client's.
        if written + n as u64 > ceiling {
            return Err(format!(
                "the response ran past the {ceiling} bytes this file should need"
            ));
        }
        if let Err(e) = file.write_all(&buf[..n]) {
            return Err(format!("writing to disk: {e}"));
        }
        written += n as u64;
        if written - reported >= PROGRESS_STEP {
            reported = written;
            on_progress(written, None);
        }
    }
    Ok(Some(written))
}

/// [`Fetch`] over `ureq`, which is the only implementation the daemon uses.
///
/// Its configuration differs from `reword/http.rs`'s agent in two places
/// that look like inconsistencies and are not:
///
/// - **Redirects are followed.** `http.rs` sets `max_redirects(0)` because a
///   redirect there sends the user's text and their API key to a host the
///   *provider* chose. Nothing of the sort rides along here: the request
///   carries no key, no body and nothing private, and Hugging Face serves
///   every large object in this set by redirecting to its CDN, so refusing
///   redirects would not harden this download, it would make it impossible.
/// - **The proxy environment is respected**, where `http.rs` sets
///   `proxy(None)`. Same reason from the other side: `http.rs` turns it off
///   so that "your text goes to `base_url` and nowhere else" stays literally
///   true. Here the connection is TLS to `huggingface.co` either way -- a
///   proxy sees a `CONNECT` and ciphertext -- so honouring it costs no
///   promise and is the difference between working and not working on a
///   corporate network.
///
/// `https_only` is set because both of those decisions widen where a
/// response may come from, and a redirect down to cleartext is the one place
/// that would matter.
pub struct UreqFetch {
    agent: ureq::Agent,
}

impl UreqFetch {
    pub fn new() -> UreqFetch {
        let config = ureq::Agent::config_builder()
            // See the struct doc: the large objects are CDN redirects.
            // `ureq`'s own default, named here so it reads as a decision
            // rather than as an oversight next to `http.rs`.
            .max_redirects(10)
            .https_only(true)
            // Connecting is the part that fails fast when there is no route
            // at all; the transfer itself is deliberately not on a clock,
            // because 341 MB over a slow link is not a timeout. What bounds
            // a *stalled* transfer is the cancel button, which is checked
            // between chunks.
            .timeout_connect(Some(std::time::Duration::from_secs(30)))
            .build();
        UreqFetch {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Default for UreqFetch {
    fn default() -> Self {
        UreqFetch::new()
    }
}

impl Fetch for UreqFetch {
    fn get(&self, url: &str, limit: u64) -> Result<Transfer, String> {
        let response = match self.agent.get(url).call() {
            Ok(r) => r,
            // Worth splitting out: a 404 here means the upstream repository
            // moved a file, which is a different thing to fix from "this
            // machine has no route to huggingface.co", and the user is the
            // one who has to tell them apart.
            Err(ureq::Error::StatusCode(code)) => {
                return Err(format!("the server answered {code}"))
            }
            Err(e) => return Err(e.to_string()),
        };
        let body = response.into_body();
        let declared = body.content_length();
        Ok(Transfer {
            declared,
            // The same ceiling the caller enforces, applied a second time by
            // the client itself: this one bounds what can be *read into this
            // process* at all, rather than what is allowed to reach the
            // disk.
            body: Box::new(body.into_with_config().limit(limit).reader()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A `Fetch` over a table of canned bodies, with a `declared` length
    /// that can disagree with what it actually yields -- which is how the
    /// ceiling and the progress correction are exercised without a server.
    struct Canned {
        bodies: HashMap<String, Vec<u8>>,
        /// Overrides the `Content-Length` for a URL. Absent means "declare
        /// the body's real length".
        declared: HashMap<String, Option<u64>>,
        /// URLs that fail instead of answering, and the message.
        errors: HashMap<String, String>,
        asked: RefCell<Vec<String>>,
    }

    impl Canned {
        /// Every asset present, each filled with its measured size in `b`s
        /// scaled down: the real figures would make this test allocate
        /// 341 MB.
        fn everything(size: usize) -> Canned {
            let mut bodies = HashMap::new();
            for asset in assets() {
                bodies.insert(asset.url.clone(), vec![b'k'; size]);
            }
            Canned {
                bodies,
                declared: HashMap::new(),
                errors: HashMap::new(),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Fetch for Canned {
        fn get(&self, url: &str, limit: u64) -> Result<Transfer, String> {
            self.asked.borrow_mut().push(url.to_string());
            if let Some(e) = self.errors.get(url) {
                return Err(e.clone());
            }
            let body = self
                .bodies
                .get(url)
                .cloned()
                .ok_or_else(|| "the server answered 404".to_string())?;
            let declared = match self.declared.get(url) {
                Some(d) => *d,
                None => Some(body.len() as u64),
            };
            Ok(Transfer {
                declared,
                body: Box::new(Limited {
                    inner: std::io::Cursor::new(body),
                    left: limit,
                }),
            })
        }
    }

    /// A reader that behaves the way `ureq`'s does at its limit: it yields
    /// `limit` bytes and then *errors*, rather than reporting end of file.
    ///
    /// The difference is the whole point of the stub. Truncating silently
    /// would let an oversized body look like a short one, and the test for
    /// the ceiling would pass against a `download` that had no ceiling at
    /// all.
    struct Limited {
        inner: std::io::Cursor<Vec<u8>>,
        left: u64,
    }

    impl Read for Limited {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.left == 0 {
                return Err(std::io::Error::other("body exceeds limit"));
            }
            let max = (self.left as usize).min(buf.len());
            let n = self.inner.read(&mut buf[..max])?;
            self.left -= n as u64;
            Ok(n)
        }
    }

    fn never() -> impl Fn() -> bool {
        || false
    }

    /// The one number the window shows before the user commits, and the one
    /// most likely to be quietly wrong: it is a sum of four measured
    /// constants, and nothing but this catches a typo in any of them.
    #[test]
    fn the_advertised_size_is_what_the_files_actually_add_up_to() {
        assert_eq!(TOTAL_BYTES, 340_682_781);
        assert_eq!(human_bytes(TOTAL_BYTES), "341 MB");
        let summed: u64 = assets().iter().map(|a| a.expected).sum();
        assert_eq!(
            summed, TOTAL_BYTES,
            "the per-file figures must sum to the advertised total, or the bar \
             finishes somewhere other than at the end"
        );
        // The measurement itself, pinned as the individual figures rather
        // than only as their sum: a pair of errors that cancel would leave
        // the assertion above perfectly happy.
        let by_name = |name: &str| {
            assets()
                .into_iter()
                .find(|a| a.relative.as_path() == Path::new(name))
                .unwrap_or_else(|| panic!("no asset for {name}"))
                .expected
        };
        assert_eq!(by_name("config.json"), 44);
        assert_eq!(by_name("tokenizer.json"), 3_497);
        assert_eq!(by_name("model.onnx"), 325_532_232);
        assert_eq!(by_name("voices/af_heart.bin"), 522_240);
        assert_eq!(
            by_name("voices/af.bin"),
            524_288,
            "af.bin is the one pack that is not 510*256 f32s"
        );
        assert!(
            offer_subtitle().contains("341 MB"),
            "the offer must say the size before the download starts: {}",
            offer_subtitle()
        );
        assert!(
            offer_subtitle().contains("huggingface.co"),
            "the offer must name the host it fetches from: {}",
            offer_subtitle()
        );
    }

    /// Decimal units, and rounded the way a size is read rather than the way
    /// a byte count is.
    #[test]
    fn sizes_are_worded_in_decimal_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(44), "44 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(3_497), "3 kB");
        assert_eq!(human_bytes(999_999), "1000 kB");
        assert_eq!(human_bytes(1_000_000), "1 MB");
        assert_eq!(human_bytes(325_532_232), "326 MB");
    }

    /// Every URL is built from the one base, and every destination lands
    /// where `kokoro_synth` and `list_voices` look for it.
    ///
    /// This is the pin against the two mistakes that produce a download
    /// which "succeeds" and leaves the dropdown as empty as it was: a voice
    /// written beside `model.onnx` rather than under `voices/`, and
    /// `onnx/model.onnx` reproduced verbatim as a *local* path.
    #[test]
    fn every_url_and_destination_is_where_the_daemon_looks() {
        let assets = assets();
        assert_eq!(assets.len(), 3 + VOICES.len());
        for asset in &assets {
            assert!(
                asset.url.starts_with(BASE_URL),
                "{} is not under the base URL",
                asset.url
            );
            assert!(
                asset.relative.is_relative(),
                "{} must be relative to the models directory",
                asset.relative.display()
            );
        }
        let by_name = |name: &str| {
            assets
                .iter()
                .find(|a| a.relative.as_path() == Path::new(name))
                .unwrap_or_else(|| panic!("no asset for {name}"))
                .clone()
        };
        assert_eq!(
            by_name("config.json").url,
            format!("{BASE_URL}/config.json")
        );
        assert_eq!(
            by_name("model.onnx").url,
            format!("{BASE_URL}/onnx/model.onnx"),
            "the weights live under onnx/ upstream and beside config.json locally"
        );
        let heart = by_name("voices/af_heart.bin");
        assert_eq!(heart.url, format!("{BASE_URL}/voices/af_heart.bin"));

        // Only fp32. The other two variants are 255 MB nobody asked for.
        for asset in &assets {
            let name = asset.name();
            assert!(
                !name.contains("fp16") && !name.contains("quantized"),
                "{name} is a model variant this download does not fetch"
            );
        }
    }

    /// The files land under their final names, complete, and no `.part`
    /// survives.
    #[test]
    fn a_completed_download_leaves_the_files_and_no_part_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fetch = Canned::everything(8);
        let mut seen = Vec::new();
        let outcome = download(dir.path(), &fetch, &never(), &mut |p| seen.push(p));
        assert_eq!(outcome, Outcome::Complete);

        for asset in assets() {
            let path = dir.path().join(&asset.relative);
            assert_eq!(
                std::fs::read(&path).expect("the file"),
                b"kkkkkkkk",
                "{} is not what was served",
                path.display()
            );
        }
        assert!(
            parts_under(dir.path()).is_empty(),
            "a completed download left a .part behind: {:?}",
            parts_under(dir.path())
        );
        assert!(!seen.is_empty(), "a download reports progress");
        let last = seen.last().expect("a last report");
        assert_eq!(
            last.done, last.total,
            "the bar must reach the end when the download does"
        );
    }

    /// Every `.part` under `dir`, at any depth.
    fn parts_under(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                out.extend(parts_under(&path));
            } else if path.extension().is_some_and(|e| e == "part") {
                out.push(path);
            }
        }
        out
    }

    /// A failure part-way through leaves nothing truncated behind, under
    /// either name.
    ///
    /// The whole point of the `.part`-then-rename dance: a truncated
    /// `model.onnx` is not reported as a failed download, it surfaces later
    /// as an ONNX parse error from inside the engine thread, and the user
    /// has no way to connect the two.
    #[test]
    fn a_failure_leaves_no_truncated_file_under_the_real_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut fetch = Canned::everything(8);
        let model = assets()
            .into_iter()
            .find(|a| a.relative.as_path() == Path::new("model.onnx"))
            .expect("the model asset");
        fetch
            .errors
            .insert(model.url.clone(), "the server answered 503".into());

        let outcome = download(dir.path(), &fetch, &never(), &mut |_| {});
        let Outcome::Failed(message) = outcome else {
            panic!("a 503 must fail the run, not finish it: {outcome:?}");
        };
        assert!(
            message.contains("model.onnx") && message.contains("503"),
            "the message must name the file and the reason: {message}"
        );
        assert!(
            !dir.path().join("model.onnx").exists(),
            "a failed file must not exist under its real name at all"
        );
        assert!(
            parts_under(dir.path()).is_empty(),
            "a failed download left a .part behind: {:?}",
            parts_under(dir.path())
        );
        // The files fetched before the failure are complete and stay.
        assert!(dir.path().join("config.json").exists());
    }

    /// A body larger than the file could plausibly be is refused, and
    /// nothing of it is kept.
    #[test]
    fn a_response_far_larger_than_the_file_should_be_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut fetch = Canned::everything(8);
        let config = assets().into_iter().next().expect("config.json first");
        // Past `ceiling_for(44)`, which is 2*44 + 1 MiB.
        fetch
            .bodies
            .insert(config.url.clone(), vec![b'k'; (1 << 21) + 1]);
        // Declared honestly: the ceiling must refuse it before a byte is
        // read, which is the case that matters for a 300 GB "model.onnx".
        let outcome = download(dir.path(), &fetch, &never(), &mut |_| {});
        let Outcome::Failed(message) = outcome else {
            panic!("an oversized body must fail: {outcome:?}");
        };
        assert!(
            message.contains("config.json"),
            "the message must name the file: {message}"
        );
        assert!(!dir.path().join("config.json").exists());
        assert!(parts_under(dir.path()).is_empty());

        // And again with the server lying about the length, so the ceiling
        // has to hold on the bytes rather than on the header.
        let dir = tempfile::tempdir().expect("tempdir");
        fetch.declared.insert(config.url.clone(), Some(44));
        let outcome = download(dir.path(), &fetch, &never(), &mut |_| {});
        assert!(
            matches!(outcome, Outcome::Failed(_)),
            "a body that runs past the ceiling must fail even when the header \
             said otherwise: {outcome:?}"
        );
        assert!(parts_under(dir.path()).is_empty());
    }

    /// Cancelling stops the run and leaves nothing half-written.
    #[test]
    fn a_cancel_stops_the_run_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fetch = Canned::everything(8);
        // Cancel after the first file, so the run is genuinely under way
        // rather than declining to start.
        let seen = std::cell::Cell::new(0usize);
        let cancel = || seen.get() > 1;
        let outcome = download(dir.path(), &fetch, &cancel, &mut |_| {
            seen.set(seen.get() + 1)
        });
        assert_eq!(outcome, Outcome::Cancelled);
        assert!(
            parts_under(dir.path()).is_empty(),
            "a cancelled download left a .part behind"
        );
        assert!(
            fetch.asked.borrow().len() < assets().len(),
            "a cancel must stop asking for files, not merely stop reporting"
        );
    }

    /// A file already on disk is left alone rather than fetched again, which
    /// is what makes a second press after a failure resume.
    #[test]
    fn an_existing_file_is_not_fetched_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("voices")).expect("voices dir");
        std::fs::write(dir.path().join("config.json"), b"already here").expect("write");
        // Zero-length is not "already here": this function never renames an
        // empty `.part` into place, so a zero-length file is somebody else's
        // interrupted download.
        std::fs::write(dir.path().join("tokenizer.json"), b"").expect("write");

        let fetch = Canned::everything(8);
        let outcome = download(dir.path(), &fetch, &never(), &mut |_| {});
        assert_eq!(outcome, Outcome::Complete);
        assert_eq!(
            std::fs::read(dir.path().join("config.json")).expect("read"),
            b"already here",
            "an existing file must not be overwritten"
        );
        assert_eq!(
            std::fs::read(dir.path().join("tokenizer.json")).expect("read"),
            b"kkkkkkkk",
            "a zero-length file is not a download that finished"
        );
        let asked = fetch.asked.borrow().clone();
        assert!(
            !asked.iter().any(|u| u.ends_with("/config.json")),
            "config.json was already there and must not have been requested"
        );
    }

    /// The progress numbers are the real ones: the total starts at the
    /// measured constant and moves to what the server actually declares.
    #[test]
    fn the_total_is_corrected_from_the_declared_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut fetch = Canned::everything(8);
        // `model.onnx` is the one asset whose ceiling is large enough to
        // carry a body past `PROGRESS_STEP`, so it is where the mid-file
        // reports have to come from.
        let model = assets()
            .into_iter()
            .find(|a| a.relative.as_path() == Path::new("model.onnx"))
            .expect("the model asset");
        let big = 3 << 20;
        fetch.bodies.insert(model.url.clone(), vec![b'k'; big]);

        let mut seen = Vec::new();
        let outcome = download(dir.path(), &fetch, &never(), &mut |p| seen.push(p));
        assert_eq!(outcome, Outcome::Complete);

        assert_eq!(
            seen[0].total, TOTAL_BYTES,
            "before anything has been fetched the total is the advertised one"
        );
        assert_eq!(
            seen.iter().filter(|p| p.file == "model.onnx").count(),
            // The file's start, the correction, three `PROGRESS_STEP`
            // crossings and the report after the rename.
            6,
            "a long file reports as it goes, not only when it finishes"
        );
        let last = seen.last().expect("a last report");
        assert_eq!(
            last.total,
            8 * (assets().len() as u64 - 1) + big as u64,
            "by the end the total is what the server actually served"
        );
        assert_eq!(last.done, last.total);
        assert!((last.fraction() - 1.0).abs() < f64::EPSILON);
        assert!(
            seen.iter().all(|p| p.fraction() <= 1.0),
            "the fraction never leaves the range a progress bar accepts"
        );
        assert!(
            seen.iter().any(|p| p.file == "voices/af_heart.bin"),
            "progress names the file being fetched"
        );
        assert!(
            seen[0].subtitle().contains("of "),
            "the subtitle reads as a fraction of the whole: {}",
            seen[0].subtitle()
        );
    }

    /// A fraction over a total the server understated is still a fraction.
    #[test]
    fn a_total_smaller_than_what_arrived_does_not_overrun_the_bar() {
        let p = Progress {
            file: "model.onnx".into(),
            done: 200,
            total: 100,
        };
        assert!((p.fraction() - 1.0).abs() < f64::EPSILON);
        let p = Progress {
            file: "model.onnx".into(),
            done: 0,
            total: 0,
        };
        assert_eq!(p.fraction(), 0.0);
    }

    /// The outcomes read as sentences a user can act on.
    #[test]
    fn every_outcome_says_what_happened() {
        assert!(Outcome::Complete.subtitle().contains("29"));
        assert!(Outcome::Cancelled.subtitle().starts_with("Cancelled"));
        assert!(
            Outcome::Cancelled.subtitle().contains("341 MB"),
            "a cancelled row is an offer again, so it says the size again: {}",
            Outcome::Cancelled.subtitle()
        );
        let failed = Outcome::Failed("no route to host".into());
        assert!(failed.subtitle().contains("no route to host"));
    }

    /// The voice list here and the one in `scripts/fetch-models.sh` are the
    /// same set, in the same order.
    ///
    /// Two copies of a 29-name list is exactly the kind of duplication that
    /// diverges silently -- the script grows a voice, the window does not,
    /// and a user who downloads from the window is missing one for no reason
    /// anybody can see. The script is shell and cannot import this, so the
    /// test reads it.
    #[test]
    fn the_voice_list_matches_the_shell_script() {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/fetch-models.sh");
        let Ok(text) = std::fs::read_to_string(script) else {
            // A packaged crate has no `scripts/`; there is nothing to
            // disagree with in that case.
            return;
        };
        let list = text
            .split_once("VOICES=(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(names, _)| names.to_string())
            .expect("the script declares a VOICES array");
        let from_script: Vec<&str> = list.split_whitespace().collect();
        assert_eq!(
            from_script,
            VOICES.to_vec(),
            "scripts/fetch-models.sh and settings::download disagree about the voices"
        );
    }
}
