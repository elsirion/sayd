//! The OpenAI-compatible client: the one implementation of [`super::Rewriter`]
//! that reaches the network.
//!
//! Small on purpose: two `Serialize` structs for the request, four
//! all-optional `Deserialize` structs for the response and its error object,
//! an agent built once, and a function from `(&RewordConfig, &str)` to a
//! `Result<String, RewordError>`. There is no streaming, no tool calling, no
//! schema, no conversation state and no model listing, because nothing asks
//! for any of them.
//!
//! **There are no providers, plural.** PPQ, Ollama, llama.cpp's `server`, LM
//! Studio and vLLM all speak the same request -- `POST
//! {base}/chat/completions`, an `Authorization: Bearer` header, a body of
//! `{"model", "messages", "stream": false}`, and an answer read at
//! `choices[0].message.content`. That is one client with a different
//! `base_url`, not five integrations.
//!
//! Split into two pure halves and a socket -- [`build_request`],
//! [`parse_response`] and [`HttpRewriter::reword`] -- so the classification
//! that §12 names as the most likely thing this design got wrong can be
//! exercised over recorded bodies without a server.
//!
//! # Everything off the socket is untrusted
//!
//! Not because the operator is: because a half-configured local server, a
//! reverse proxy and a load balancer all produce bodies no provider's
//! documentation describes. Every response field here is an `Option` that
//! tolerates the wrong type as well as `null`, nothing is indexed, and there
//! is no `unwrap`, `expect` or `panic!` on any value that arrived over the
//! wire. The read is length-limited for the same reason, and every string
//! that reaches a log line -- `error.message`, and the transport reason
//! carried by [`RewordError::Unreachable`] -- goes through
//! [`sanitise_message`], which cuts it to [`MESSAGE_CHARS`] and replaces
//! control characters. Unbounded, a provider could write a 60 KB warning
//! line, forge further `warning: reword:` lines inside it and run ANSI
//! escapes at whoever reads `journalctl`. Measured on the second of those
//! before it was bounded: a 60,000-character `Location` header produced a
//! 60,094-byte `warning:` line and a 60 KB subtitle in the settings window,
//! and a crafted one put a forged `warning: reword: your API key was
//! revoked` inside sayd's own warning line.
//!
//! "No panic on anything off the socket" is a claim about this file, and it
//! was not a claim about the parser underneath it: a response header name of
//! 65,536 bytes or more panics inside `ureq-proto`. That is why the agent
//! sets [`RESPONSE_HEADER_LIMIT`] below where the crash lives.
//!
//! # The request goes to `base_url` and nowhere else
//!
//! §7 tells the user their text goes to `base_url`, and the `info: reword:
//! sending text to …` line names `base_url`. Two of `ureq`'s defaults would
//! make both statements false, and [`build_agent`] turns both off.
//!
//! `ureq` picks a proxy out of `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` by
//! default, so in a shell that happens to have one set the request would be
//! tunnelled through a host the user was never told about. The agent
//! therefore sets `proxy(None)` explicitly. A user who must egress through a
//! proxy puts it in `base_url`, where the line that announces it can name
//! it.
//!
//! `ureq` also follows up to 10 redirects by default, over plain HTTP as
//! readily as HTTPS. The notification text and the API key do not survive
//! one -- `ureq` downgrades to GET on 301/302/303 and refuses 307/308 with a
//! body -- but a GET still goes to whatever host and port the *provider*
//! named: loopback, `169.254.169.254`, anything on the LAN. And the answer
//! that came back from there, not from `base_url`, would be the candidate
//! that reached `check()` and the speaker. The agent therefore sets
//! `max_redirects(0)`.

use std::sync::OnceLock;
use std::time::Duration;

use sayd_core::config::{resolve_api_key, Provider, RewordConfig};
use sayd_core::reword::{chat_completions_url, parse_base_url};
use serde::{Deserialize, Serialize};

use super::{http_ceiling, RewordError, Rewriter};


/// `f64` rather than `f32`, because this is serialised into JSON and JSON
/// numbers are `f64`. As an `f32` the literal `0.2` widens to
/// `0.20000000298023224`, and that is what goes out on the wire -- harmless
/// to a model and needless noise in a request a user may well be reading in
/// a proxy log.
const TEMPERATURE: f64 = 0.2;

/// How much of a response body is read before the read fails.
///
/// The body is untrusted, so an unbounded read is a memory bug: a server
/// that streams gigabytes must not be able to grow this process. At
/// [`RewordConfig::max_chars`]'s ceiling of 2000, [`RewordConfig::max_tokens`]
/// is 6000 -- around 24 KB of UTF-8 -- so 64 KiB still clears it plus the
/// envelope, but by a factor of about two and a half now that the cap
/// moves with configuration, not by the order of magnitude it was when
/// this was a fixed 256-token cap. Still far more than any `error.message`
/// worth reading. A body that does exceed the limit is not a crash or a
/// leak: the read fails and the attempt classifies as `Malformed`, the
/// same as any other response this client cannot make sense of.
const BODY_LIMIT: u64 = 64 * 1024;

/// How much of a provider's `error.message` may reach a log line.
///
/// §8 asks for "the reason and the first 80 characters", and this is the
/// second half of that sentence. [`BODY_LIMIT`] bounds the *process*; this
/// bounds the *journal*, which is a different budget with a different
/// attacker: a message is quoted verbatim into a `warning: reword:` line, so
/// unbounded it is a 64 KB log entry, and one containing a newline is a
/// forged second warning line.
const MESSAGE_CHARS: usize = 80;

/// How much status line and how many header bytes the client will read
/// before it gives up on a response.
///
/// IMPORTANT 4, and the number is chosen against a crash rather than
/// against a budget. `ureq`'s default is exactly 64 KiB, and `run.rs` calls
/// `try_response` *before* it compares what it has read against that limit,
/// so a header name of 65,536 bytes or more panics inside `ureq-proto`'s
/// parser rather than being refused. Bisected exactly on this dependency:
/// 65,535 clean, 65,536 panics. The panic is contained -- [`super::attempt`]
/// turns the `JoinError` into [`RewordError::Malformed`], the permit is
/// released by the unwind and the runtime survives -- but it falsifies this
/// module's panic-freedom claim and puts a backtrace on the daemon's
/// stderr, and anyone who can answer the socket can fire it, including a
/// host a redirect named.
///
/// 16 KiB, and [`INPUT_BUFFER_SIZE`] below it, because one number is not
/// enough: `ureq` compares this against `input.len()` *after* the parse, so
/// what actually has to stay under 65,536 is how many bytes can be in the
/// read buffer at once. With the shipped 128 KiB buffer a 70,000-byte name
/// arrives whole and is parsed -- measured, and flaky in exactly the way
/// that implies: the same test passed and then panicked depending on how
/// the reads landed.
///
/// No real provider's response headers come near 16 KiB: the
/// OpenAI-compatible answer this client reads carries a status line, a
/// content type, a length and a handful of rate-limit counters.
const RESPONSE_HEADER_LIMIT: usize = 16 * 1024;

/// How many bytes of a response may sit in `ureq`'s read buffer at once.
///
/// The other half of [`RESPONSE_HEADER_LIMIT`], and the half that actually
/// keeps `ureq-proto`'s parser away from the header name that panics it:
/// the parser only ever sees what is in this buffer, so at 32 KiB a
/// 65,536-byte name cannot be assembled in it at all. Strictly larger than
/// the header limit, so the limit is what reports the failure -- a buffer
/// that filled to exactly the limit would leave `check_size > limit` false
/// and the loop with nothing to say.
///
/// `ureq`'s default is 128 KiB. Smaller costs a few more `read` calls on a
/// body that is already capped at [`BODY_LIMIT`].
const INPUT_BUFFER_SIZE: usize = 32 * 1024;

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [Message<'a>; 2],
    stream: bool,
    temperature: f64,
    max_tokens: u32,
    /// Absent for every provider but llama.cpp, and absent rather than
    /// `null`: a remote OpenAI-compatible service rejects an unknown
    /// top-level field, so this must not reach one at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<ChatTemplateKwargs>,
}

/// llama.cpp's spelling of "do not reason".
///
/// A nested object rather than a flat field because that is the shape the
/// server's chat-template layer reads; the kwargs are handed to the
/// template, not to the sampler. `reasoning_budget: 0` is the obvious
/// alternative and does not work -- measured, 6 requests of 6 still
/// reasoned.
#[derive(Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

/// A field that is missing, `null`, **or the wrong type entirely** reads as
/// `None`, and the rest of the body survives it.
///
/// `Option` alone is not enough, and the gap is not hypothetical:
/// `{"choices":[{"message":{"content":"a good rewrite"}}],"error":"oops"}` --
/// an `error` that is a string where this client expects an object -- fails
/// the whole parse with plain `Option<ApiError>`, and a perfectly good
/// rewrite is thrown away with it. `Option` covers `null`;
/// `#[serde(default)]` covers absent; only buffering the field and letting
/// its own parse fail in isolation covers the wrong type.
///
/// The cost is one `serde_json::Value` per field, on a body already capped
/// at [`BODY_LIMIT`], on a path that runs at most twice a minute.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).ok())
}

/// Every field an `Option`: this is untrusted input off a socket, whoever is
/// running the server.
///
/// `Option` rather than the bare type with `#[serde(default)]`, which is the
/// obvious spelling and the wrong one -- `default` covers a *missing* field
/// but not an explicit `null`, and `"content": null` is exactly what a local
/// server returns when generation produced nothing. With the bare type one
/// `null` anywhere fails the whole parse, and the `error.message` that would
/// have said *why* goes with it. [`lenient`] extends the same argument to a
/// field of the wrong type.
#[derive(Deserialize, Default)]
struct ChatResponse {
    #[serde(default, deserialize_with = "lenient")]
    choices: Option<Vec<Choice>>,
    #[serde(default, deserialize_with = "lenient")]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default, deserialize_with = "lenient")]
    message: Option<ChoiceMessage>,
    #[serde(default, deserialize_with = "lenient")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    #[serde(default, deserialize_with = "lenient")]
    content: Option<String>,
    /// Where llama.cpp puts a thinking block. Read only to say *why* a
    /// generation ran out of room; it is never a candidate, and never spoken.
    #[serde(default, deserialize_with = "lenient")]
    reasoning_content: Option<String>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default, deserialize_with = "lenient")]
    message: Option<String>,
}

/// A request as values, so it can be asserted on without a socket.
pub struct Request {
    pub url: String,
    /// The whole header value, `Bearer …`, or `None` for no header at all.
    pub authorization: Option<String>,
    pub body: serde_json::Value,
}

/// Pure: takes the already-resolved key rather than reading the
/// environment, so a test can drive both the with-key and without-key
/// cases without touching process-global state.
pub fn build_request(cfg: &RewordConfig, key: Option<&str>, prompt: &str, text: &str) -> Request {
    build_request_in(cfg, key, prompt, text, false)
}

/// [`build_request`] with the `stream` flag chosen by the caller.
///
/// Split rather than parameterised at the one call site, so the
/// non-streaming body -- which every existing test asserts on, byte for
/// byte -- keeps a constructor that cannot accidentally start streaming.
pub fn build_request_in(
    cfg: &RewordConfig,
    key: Option<&str>,
    prompt: &str,
    text: &str,
    stream: bool,
) -> Request {
    let request = ChatRequest {
        model: &cfg.model,
        messages: [
            Message {
                role: "system",
                content: prompt,
            },
            Message {
                role: "user",
                content: text,
            },
        ],
        stream,
        temperature: TEMPERATURE,
        max_tokens: cfg.max_tokens(),
        chat_template_kwargs: match cfg.resolved_provider() {
            Some(Provider::LlamaCpp) => Some(ChatTemplateKwargs {
                enable_thinking: false,
            }),
            Some(Provider::Generic) | None => None,
        },
    };
    Request {
        url: chat_completions_url(&cfg.base_url),
        authorization: key.map(|k| format!("Bearer {k}")),
        // `to_value` on a struct that has just been built cannot fail, but
        // this is a daemon: an `unwrap_or` costs nothing and removes the
        // question.
        body: serde_json::to_value(&request).unwrap_or(serde_json::Value::Null),
    }
}

/// A provider's own words, cut to length and stripped of anything that can
/// act on a terminal. `None` for a message that is absent or all whitespace.
///
/// **The byte came off a socket, so this is the boundary.** Sanitising here
/// rather than at the `eprintln!` means there is one place to get it right
/// and no way for a later caller to forget: past this function no
/// `RewordError` carries a string a provider chose the length or the bytes
/// of.
///
/// Two separate hazards, one function:
///
/// * **Length.** Measured on a hostile loopback server: a 60 000-character
///   `error.message` survived whole into a single `warning:` line.
/// * **Control characters.** JSON `\n` and `\u001b` survive `serde_json`
///   into the `String`, because they are perfectly legal JSON. A newline
///   forges an additional `warning: reword:` line in the journal -- a
///   provider writing what looks like this daemon's own diagnostics -- and
///   an ESC runs ANSI sequences at whoever is reading it. Both become
///   U+FFFD, which is visible rather than silent: a mangled message tells
///   the reader something was in there.
///
/// The ellipsis is not decoration. Without it a truncated message reads as
/// the provider's complete sentence, and the user goes looking for a reason
/// that was cut off.
fn sanitise_message(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let head = sayd_core::reword::truncate_for_debug(raw, MESSAGE_CHARS);
    let mut out: String = head
        .chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect();
    if head.len() < raw.len() {
        out.push('…');
    }
    Some(out)
}

/// Does `message` name `model`?
///
/// A plain `contains` is the obvious spelling and is wrong: with a short
/// model name it fires on any message that happens to contain those letters
/// -- `"Rate limit reached"` contains `"m"` -- and a rate limit misreported
/// as a missing model sends the user to fix a setting that was never broken.
/// The match must therefore fall on an identifier boundary: the characters
/// either side of it may not be alphanumeric. Every real name clears this
/// (`model 'llama3.2:3b' not found`, `failed to load model "gpt-4o-mini"`),
/// because a server that names a model quotes or spaces it.
///
/// **This is not a sound test and is not asked to be one.** It still fires
/// on `model 'gpt-4o-mini' not found` for a configured `gpt-4o`, on
/// `claude-sonnet-5-thinking` for a configured `claude-sonnet-5`, and on
/// `try again in 20 m` for a configured `m`. Requiring a delimiter around
/// the name (`'x'`, `"x"`, `` `x` ``) would kill all three -- and was
/// rejected, because it trades the cheap error for the expensive one. This
/// rule now runs *only* on statuses that mean nothing on their own (see
/// [`parse_response`]), where a false positive costs a misleading line on a
/// request that failed anyway, and a false negative costs the entire §12
/// mitigation: a server answering 200 or 500 for a missing model would go
/// back to being undiagnosable. A server that names a model without
/// quoting it -- `Model gpt-4o-mini does not exist` -- is one delimiter
/// style away from real, and there is no list of them worth betting the
/// diagnosis on.
fn mentions_model(message: &str, model: &str) -> bool {
    if model.is_empty() {
        return false;
    }
    // `match_indices` yields byte offsets that are always on a character
    // boundary, so both slices below are sound for any UTF-8 message.
    message.match_indices(model).any(|(start, matched)| {
        let before = message[..start].chars().next_back();
        let after = message[start + matched.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Classify one response. **A status that means one thing decides; a status
/// that means nothing lets the body decide.**
///
/// OpenAI-compatible servers are not consistent about which of the two
/// carries the reason, which is why `http_status_as_error` is off and every
/// response is read whatever its status. But they are not equally
/// inconsistent, and the split is what this function is:
///
/// * **401, 403 and 429 mean one thing on every server that speaks this
///   protocol.** No body can improve on them, so they are classified first
///   and the body is used only for its wording. Getting this backwards was
///   measured and is not theoretical: OpenAI's real 429 body is `Rate limit
///   reached for gpt-4o-mini in organization org-… on requests per min`,
///   which *names the configured model*, space-delimited -- the exact
///   boundary [`mentions_model`] accepts. Ordered the other way that 429
///   became [`RewordError::NoSuchModel`], so `RewordState::record` never
///   set the rate-limit backoff (only [`RewordError::RateLimited`] does),
///   §8's "honour `Retry-After`, otherwise back off 60 s" never happened,
///   and the daemon kept hammering a provider that was asking it to stop --
///   while telling the user to go change a model name that was correct. The
///   same ordering defeated the auth latch: `Your API key does not have
///   access to model gpt-5.6-sol` is a 403 that names a model, so a key
///   with no access to that model was retried for the life of the daemon
///   instead of latching. Per-model key scoping is ordinary on gateways.
/// * **200, 400, 404, 500 and everything else mean nothing on their own.**
///   A missing model arrives as any of them depending on the server (§12),
///   so here the body gets the vote: a message naming the configured model
///   is a missing model whatever the status says. This is what keeps §12's
///   mitigation -- 500-for-a-missing-model and 200-with-an-error-body --
///   working.
///
/// A message is used for its wording wherever one is present, after
/// [`sanitise_message`] has bounded it.
pub fn parse_response(
    status: u16,
    retry_after: Option<&str>,
    body: &[u8],
    model: &str,
    host: &str,
) -> Result<String, RewordError> {
    // A body that is not JSON at all -- an HTML error page from a proxy, an
    // empty body, a truncated one -- parses to the default, which carries
    // neither a message nor a choice, and falls out the bottom as
    // `Malformed`. A trusted operator does not make a truncated body parse.
    let parsed: ChatResponse = serde_json::from_slice(body).unwrap_or_default();
    let message = parsed
        .error
        .and_then(|e| e.message)
        .and_then(|m| sanitise_message(&m));

    // Unambiguous statuses first: the body may say what happened, never
    // which row it was.
    match status {
        401 | 403 => {
            return Err(RewordError::Auth {
                status,
                host: host.to_string(),
                message,
            })
        }
        429 => {
            return Err(RewordError::RateLimited {
                // Only the delta-seconds form. An HTTP-date would need a
                // date parser for a header that is already advisory, and
                // the fixed backoff is the honest fallback.
                retry_after: retry_after
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs),
                message,
            });
        }
        _ => {}
    }
    // Everything below this line is a status that does not say what went
    // wrong, so the body is the only evidence there is.
    if message.as_deref().is_some_and(|m| mentions_model(m, model)) || status == 404 {
        return Err(RewordError::NoSuchModel {
            status,
            model: model.to_string(),
            message,
        });
    }
    // A 200 carrying *both* a usable `content` and a non-empty
    // `error.message` is read as the error, so a provider that attaches a
    // deprecation notice to a good answer switches the feature off silently.
    // Deliberate, and this is the cheaper direction: an `error` object is a
    // server saying something went wrong, and speaking an answer it
    // disowned -- a truncated generation, a content-filter stub -- puts
    // words in the user's ear that no line in the journal can take back.
    // The other direction merely costs a rewrite and leaves the reason in
    // the debug log.
    if let Some(message) = message {
        return Err(RewordError::Malformed(message));
    }
    if status >= 400 {
        return Err(RewordError::Malformed(format!("HTTP {status}")));
    }
    // No indexing anywhere: an empty `choices`, a `choices` that is not an
    // array, a choice with no `message` and a `content` that is `null` all
    // arrive here as `None`.
    let choice = parsed.choices.unwrap_or_default().into_iter().next();
    // Before the content is looked at, and regardless of whether there is
    // any. A truncated answer that *has* text is the dangerous case, not the
    // safe one: it is a sentence cut off mid-clause, it passes the guard's
    // length check, and it is what gets spoken.
    //
    // Matched case-insensitively against both "length" and "max_tokens":
    // OpenAI and llama.cpp send `"length"`, but Gemini- and
    // Anthropic-compatibility shims send `MAX_TOKENS` / `max_tokens`. Missing
    // either spelling does not fail safe -- it is the mid-sentence
    // truncation above that reaches the speaker uncaught.
    if choice
        .as_ref()
        .and_then(|c| c.finish_reason.as_deref())
        .is_some_and(|r| r.eq_ignore_ascii_case("length") || r.eq_ignore_ascii_case("max_tokens"))
    {
        return Err(RewordError::Truncated {
            reasoning: choice
                .as_ref()
                .and_then(|c| c.message.as_ref())
                .and_then(|m| m.reasoning_content.as_deref())
                .is_some_and(|r| !r.trim().is_empty()),
        });
    }
    let content = choice
        .and_then(|c| c.message)
        .and_then(|m| m.content)
        .filter(|c| !c.trim().is_empty());
    content.ok_or_else(|| {
        RewordError::Malformed("the response carried no choices[0].message.content".to_string())
    })
}

/// One agent, carrying everything about this client that is not per
/// request.
///
/// **The global timeout is deliberately not among them.** It used to be:
/// the agent was built with a flat 10 s ceiling, which was
/// sound only while `reword.timeout_ms` was clamped to 2 s. It no longer is,
/// and an agent-level ceiling cannot follow a config that changes at
/// runtime -- [`agent`] is a `OnceLock` built once per process, so a
/// deadline raised to 30 s in the settings window would have gone on being
/// cut off at 10 by a client built before the change. That is the one thing
/// this path must never do: the transport ending a rewrite before the
/// configured deadline is indistinguishable, to the user, from a provider
/// that did not answer.
///
/// So the ceiling is set on the *request* instead, by [`send`], from the
/// config that request is for. It is a required argument there, not an
/// option with a default, which is what keeps "no ceiling at all" from
/// being reachable by forgetting something.
fn build_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        // Load-bearing. Left on (the default), a 4xx becomes an `Err`
        // and the body is discarded -- and the body is where
        // `error.message` lives.
        .http_status_as_error(false)
        // Load-bearing for §7, not for correctness: `ureq`'s default reads
        // `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` and would tunnel the
        // request through a host neither §7's privacy statement nor the
        // `sending text to …` line names. See the module doc.
        .proxy(None)
        // IMPORTANT 1, and the other half of the same sentence. `ureq`'s
        // default is 10, and a followed redirect is a request to a host
        // and port the *provider* chose: loopback, `169.254.169.254`, or
        // anything on the LAN. `https_only` is false by default too, so an
        // `https://` base could be bounced to plain `http://` -- and the
        // cleartext warning is computed from `base_url` alone, so it would
        // never fire. Zero, not one: the redirect target also chooses the
        // candidate that reaches `check()` and the speaker, and this client
        // has exactly one endpoint to talk to. With no redirects allowed
        // the 3xx is returned as-is and classifies as `Malformed`, which
        // is the truth -- it carried no `choices[0].message.content`.
        //
        // `max_redirects_will_error` is deliberately left alone: it "has no
        // meaning if `max_redirects` is 0" (ureq's own doc), so setting it
        // would suggest a behaviour it does not have.
        .max_redirects(0)
        // IMPORTANT 4, both lines: `ureq`'s defaults are 64 KiB and
        // 128 KiB, and a header name of 65,536 bytes panics the parser
        // underneath them. See the two constants -- the buffer is what
        // bounds what the parser can be handed, and the header limit is
        // what reports it.
        .max_response_header_size(RESPONSE_HEADER_LIMIT)
        .input_buffer_size(INPUT_BUFFER_SIZE)
        .build();
    ureq::Agent::new_with_config(config)
}

/// The agent, built once and cached.
///
/// An `Agent` owns a connection pool, and against a 1.5 s budget a fresh DNS
/// lookup plus a TLS handshake is most of the budget. It outlives config
/// changes, and that is now true without qualification: `base_url`, `model`,
/// the key *and the ceiling* are per-request inputs rather than client
/// state, so nothing in a config change requires rebuilding it. Which is why
/// a `OnceLock` with no way to replace its value is the right shape -- and
/// why the ceiling had to leave it, since a `OnceLock` is exactly the shape
/// a value that must track the config cannot have.
///
/// Nothing is pre-warmed at startup, because that would be a network call
/// the user did not ask for. The **first** rewrite of a run is therefore
/// expected to miss the deadline and speak the original. That is the
/// fallback working, not a bug, and the settings window's Test row says so
/// on screen.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(build_agent)
}

pub struct HttpRewriter {
    cfg: RewordConfig,
    key: Option<String>,
    host: String,
}

/// Hand-written because `key` is not a value that already sits in a file --
/// for an `api_key_env` configuration it is the live secret read out of the
/// process environment -- and `cfg` carries `api_key` inline, so deriving
/// `Debug` on either field puts a credential one `dbg!()`, one debug log
/// line, or one panic-message interpolation away from plaintext. This file
/// already refuses a key containing a character an HTTP header cannot carry
/// (see `is_header_safe`); the same care applies here, just pointed the
/// other way -- at what this type is willing to print rather than what a
/// provider is willing to accept.
///
/// This impl exists only so `{other:?}` keeps compiling on the
/// `Result<HttpRewriter, RewordError>` that
/// `a_client_cannot_be_built_without_a_usable_provider` matches on below --
/// no production code formats a rewriter today. It prints the fields worth
/// having when a rewrite goes wrong (`host`, and `cfg`'s endpoint and model)
/// and, for the key, only whether one resolved at all: that distinguishes
/// "no key configured" from "key rejected" without ever showing what the
/// key is.
impl std::fmt::Debug for HttpRewriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRewriter")
            .field("host", &self.host)
            .field("base_url", &self.cfg.base_url)
            .field("model", &self.cfg.model)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl HttpRewriter {
    /// Refuses an unusable `base_url` -- and an unusable key -- here rather
    /// than at request time, so the daemon logs it once at first use instead
    /// of once per utterance.
    pub fn new(cfg: &RewordConfig) -> Result<HttpRewriter, RewordError> {
        // Before `base_url`, because a config with neither wants the field it
        // has never heard of named first. Reported here rather than at
        // startup because `--reword` does not require `enabled`, so this is
        // reachable on a daemon that was right to start.
        if cfg.resolved_provider().is_none() {
            let names = Provider::NAMES.join(", ");
            return Err(RewordError::NotConfigured(match cfg.provider.as_deref() {
                None => format!("reword.provider is unset; set it to one of: {names}"),
                Some(bad) => format!(
                    "reword.provider = {bad:?} is not a provider this build \
                     knows; set it to one of: {names}"
                ),
            }));
        }

        let endpoint = parse_base_url(&cfg.base_url)
            .map_err(|e| RewordError::NotConfigured(format!("reword.base_url: {e}")))?;
        let key = resolve_api_key(cfg);
        if let Some(key) = &key {
            if let Some(bad) = key.chars().find(|c| !is_header_safe(*c)) {
                // MINOR 4. Left to `send`, this arrives as
                // `ureq::Error::Protocol("authorization header is not a
                // string")`, which falls through that function's catch-all
                // into `RewordError::Unreachable` -- measured -- and three
                // notifications then open the transport breaker over a
                // configuration nothing about the network will fix. The
                // arm beside it says in as many words that a key which
                // cannot go into a header belongs on the row that is said
                // once per run; this is what puts it there.
                return Err(RewordError::NotConfigured(format!(
                    "{source} holds a character an HTTP header cannot carry ({bad:?}); \
                     an en-dash or a smart quote from a copied web page is the usual \
                     cause",
                    source = key_source(cfg),
                )));
            }
        }
        Ok(HttpRewriter {
            cfg: cfg.clone(),
            key,
            host: endpoint.host,
        })
    }
}

/// May `c` appear in an HTTP header value?
///
/// The `http` crate's `HeaderValue` accepts visible ASCII plus space and
/// tab, and nothing else; a `char` outside that range is what
/// `ureq_proto` refuses with "authorization header is not a string". This
/// is the same rule stated where the *configuration* is checked, so the
/// failure is reported as one.
///
/// Deliberately narrower than the RFC, which also permits obs-text (0x80 to
/// 0xFF): those bytes are not `char`s, no provider issues a key containing
/// them, and `HeaderValue::from_str` would refuse them anyway.
fn is_header_safe(c: char) -> bool {
    c == '\t' || (' '..='~').contains(&c)
}

/// Which setting the key came out of, for the line that says it is unusable.
///
/// Naming it matters here more than most places: a key supplied through
/// `api_key_env` is not in the file the settings window writes, so "check
/// `reword.api_key`" would send the user to look at an empty field.
fn key_source(cfg: &RewordConfig) -> String {
    let from_env =
        !cfg.api_key_env.is_empty() && std::env::var(&cfg.api_key_env).is_ok_and(|v| !v.is_empty());
    if from_env {
        format!("the API key in ${}", cfg.api_key_env)
    } else {
        "reword.api_key".to_string()
    }
}

/// A transport failure, with both halves of what it will say bounded.
///
/// `ureq`'s transport errors name what went wrong (`io: Connection refused`)
/// and never *where*: the address lives in the separate once-per-run
/// `sending text to …` line, which is not the line a user pastes into an
/// issue. Appending the URL is what makes the one warning they do paste say
/// which endpoint could not be reached.
///
/// IMPORTANT 2, and the reason this is a function with a doc comment rather
/// than the closure it was: `reason` is not ours. It is `ureq`'s `Display`
/// over an error whose text can carry bytes the *provider* chose, and it
/// goes verbatim into a `warning: reword:` line and into the settings
/// window's Test subtitle. Measured unbounded: a 60,000-character `Location`
/// header produced a 60,094-byte `warning:` line and a 60 KB subtitle, a
/// crafted one put a forged `warning: reword: your API key was revoked`
/// inside sayd's own warning line, and a TAB survived into the journal.
/// That is exactly the hazard [`sanitise_message`] closes for
/// `error.message`, reached through a different variant, so it is closed the
/// same way and to the same [`MESSAGE_CHARS`].
///
/// The URL goes through it too. It is the user's own configuration rather
/// than a provider's, so it is not hostile -- but a bound on half of a line
/// is not a bound on the line.
fn unreachable_from(reason: &str, url: &str) -> RewordError {
    fn bounded(s: &str, absent: &str) -> String {
        sanitise_message(s).unwrap_or_else(|| absent.to_string())
    }
    RewordError::Unreachable(format!(
        "{} ({})",
        bounded(reason, "no reason given"),
        bounded(url, "no endpoint")
    ))
}

/// Send one request on `agent`, bounded by `ceiling`, and classify what
/// comes back.
///
/// The agent is a parameter rather than a call to [`agent`] so a test can
/// drive this whole path -- socket, status, headers, body limit,
/// classification -- against a real socket. `ceiling` is a parameter for two
/// reasons: production needs it per request, because it is derived from a
/// `reword.timeout_ms` that changes at runtime (see
/// [`crate::reword::http_ceiling`]), and it is the only way the ceiling can
/// be *tested* -- the real one is at least ten seconds, longer than any test
/// should take, so `a_provider_that_never_answers_hits_the_ceiling` hands
/// this 400 ms and points it at a server that never answers. Note the type:
/// `Duration`, not `Option<Duration>`. "No ceiling at all" is not
/// expressible here.
///
/// `timeout_global` is ureq's end-to-end bound -- "from DNS lookup to
/// finishing reading the response body", its own words -- so the body read
/// further down is inside it too, and not only the headers. Set at request
/// scope it replaces whatever the agent carries for this call alone
/// (`ureq::config`'s `RequestScope`, which clones the agent's config and
/// overrides it), which is what lets one cached agent serve deadlines that
/// differ per request.
fn send(
    agent: &ureq::Agent,
    request: &Request,
    ceiling: Duration,
    model: &str,
    host: &str,
) -> Result<String, RewordError> {
    let mut call = agent
        .post(&request.url)
        .config()
        .timeout_global(Some(ceiling))
        .build()
        .header("content-type", "application/json");
    if let Some(auth) = &request.authorization {
        call = call.header("authorization", auth);
    }
    let unreachable = |e: ureq::Error| unreachable_from(&e.to_string(), &request.url);
    let mut response = match call.send_json(&request.body) {
        Ok(r) => r,
        Err(ureq::Error::Timeout(_)) => return Err(RewordError::Ceiling),
        // Headers past [`RESPONSE_HEADER_LIMIT`]. The same reasoning as
        // `BodyExceedsLimit` below: this is a *response* this client will
        // not read, not a provider that is down, so it must not count
        // toward the transport breaker -- otherwise three fat answers, or
        // three hostile ones, switch the feature off for a minute.
        Err(ureq::Error::LargeResponseHeader(read, limit)) => {
            return Err(RewordError::Malformed(format!(
                "the response headers reached {read} bytes, past the {limit}-byte limit"
            )))
        }
        // Nothing was sent: the URL or a header value could not go into
        // a request at all. `parse_base_url` checks the scheme and
        // picks out the host, which is not the same as being a URI, and
        // a key pasted with a stray control character is an invalid
        // header value. Both are configuration failures no amount of
        // time fixes, so neither may count toward the transport
        // breaker -- they belong on the row that says so once per run.
        Err(e @ (ureq::Error::BadUri(_) | ureq::Error::Http(_))) => {
            return Err(RewordError::NotConfigured(format!(
                "reword.base_url or the API key cannot be put into a request ({e})"
            )))
        }
        Err(e) => return Err(unreachable(e)),
    };
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = match response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()
    {
        Ok(b) => b,
        Err(ureq::Error::Timeout(_)) => return Err(RewordError::Ceiling),
        // A body over the limit is a *response* this client will not
        // read, not a provider that is down: classifying it as
        // `Unreachable` would count it toward the transport breaker and
        // switch the feature off for a minute over one fat answer.
        Err(ureq::Error::BodyExceedsLimit(limit)) => {
            return Err(RewordError::Malformed(format!(
                "the response body exceeded {limit} bytes"
            )))
        }
        Err(e) => return Err(unreachable(e)),
    };
    parse_response(status, retry_after.as_deref(), &body, model, host)
}

/// One `data:` line of an OpenAI-style stream. Every field optional for
/// the reason [`ChatResponse`]'s are: this is an untrusted body, and a
/// frame missing what this client wants must be skipped rather than fail
/// the stream.
#[derive(Deserialize, Default)]
struct StreamFrame {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize, Default)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
}

/// Read an SSE body, handing each `delta.content` to `on_delta`.
///
/// Stops -- returning `Ok(())` -- as soon as `on_delta` answers `false`,
/// on `data: [DONE]`, or at [`BODY_LIMIT`] total bytes. The cap is the same
/// one `send` relies on and matters more here, not less: a server that
/// never stops streaming would otherwise grow this process without bound
/// *and* speak without bound.
fn read_stream(
    mut reader: impl std::io::Read,
    on_delta: &mut dyn FnMut(&str) -> bool,
) -> Result<(), RewordError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut consumed: u64 = 0;
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) => return Err(RewordError::Malformed(format!("reading the stream: {e}"))),
        };
        consumed += n as u64;
        if consumed > BODY_LIMIT {
            return Err(RewordError::Malformed(format!(
                "the streamed body exceeded {BODY_LIMIT} bytes"
            )));
        }
        buf.extend_from_slice(&chunk[..n]);
        // SSE frames are newline-delimited; a partial trailing line stays in
        // the buffer until the rest of it arrives.
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                return Ok(());
            }
            let frame: StreamFrame = serde_json::from_str(data).unwrap_or_default();
            for choice in &frame.choices {
                if let Some(text) = &choice.delta.content {
                    if !text.is_empty() && !on_delta(text) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// The shape of `GET /v1/models`. Every field optional for the reason
/// [`ChatResponse`]'s are: this is an untrusted body, and a server that
/// answers something unexpected must produce an empty list rather than fail
/// the parse.
#[derive(Deserialize, Default)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize, Default)]
struct ModelEntry {
    #[serde(default)]
    id: Option<String>,
}

/// Ask the configured endpoint which models it has.
///
/// `GET {base_url}/models`, the OpenAI-compatible listing that llama.cpp's
/// server, llama-swap, Ollama, LM Studio and vLLM all answer. **Optional by
/// design**: a server that does not implement it, or answers something
/// unrecognisable, yields an empty list rather than an error the user has to
/// dismiss, because the Model row is a free-text entry and a listing that
/// fails costs nothing but the suggestion.
///
/// A short deadline of its own rather than [`http_ceiling`]: this runs
/// because a user opened a menu and is waiting on it, which is a different
/// budget from a rewrite that may legitimately take half a minute. Ten
/// seconds is long enough for a loaded server on a slow link and short
/// enough that a dead endpoint does not leave the menu spinning.
///
/// Sends the same `Authorization` header a rewrite would: a remote endpoint
/// that needs a key to answer `/chat/completions` needs one to answer this.
pub fn list_models(cfg: &RewordConfig, key: Option<&str>) -> Result<Vec<String>, RewordError> {
    let url = sayd_core::reword::models_url(&cfg.base_url);
    let mut call = agent()
        .get(&url)
        .config()
        .timeout_global(Some(MODEL_LIST_CEILING))
        .build();
    if let Some(key) = key {
        call = call.header("authorization", format!("Bearer {key}"));
    }
    let response = match call.call() {
        Ok(r) => r,
        Err(ureq::Error::Timeout(_)) => return Err(RewordError::Ceiling),
        Err(e) => return Err(unreachable_from(&e.to_string(), &url)),
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(RewordError::NotConfigured(format!(
            "the endpoint answered {status} when asked for its model list"
        )));
    }
    let body = response
        .into_body()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()
        .map_err(|e| RewordError::Malformed(format!("reading the model list: {e}")))?;
    let parsed: ModelList = serde_json::from_slice(&body).unwrap_or_default();
    let mut ids: Vec<String> = parsed
        .data
        .into_iter()
        .filter_map(|m| m.id)
        .filter(|id| !id.trim().is_empty())
        .collect();
    // Sorted and deduplicated because this is a menu: the wire order is
    // whatever the server felt like, and llama-swap in particular lists in
    // config-file order, which is not an order a user is looking for a name
    // in.
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// What [`list_models`] waits. See its doc for why this is not
/// [`http_ceiling`].
const MODEL_LIST_CEILING: Duration = Duration::from_secs(10);

impl Rewriter for HttpRewriter {
    fn reword(&self, prompt: &str, text: &str) -> Result<String, RewordError> {
        let request = build_request(&self.cfg, self.key.as_deref(), prompt, text);
        // From `self.cfg`, which is the config this rewriter was built for:
        // `reword::context` rebuilds the rewriter whenever the config
        // changes, so this is the deadline the caller is timing this very
        // request against -- never a stale one, and never a constant.
        send(
            agent(),
            &request,
            http_ceiling(&self.cfg),
            &self.cfg.model,
            &self.host,
        )
    }

    fn reword_stream(
        &self,
        prompt: &str,
        text: &str,
        emit: &mut dyn FnMut(&str) -> bool,
    ) -> Result<(), RewordError> {
        let request = build_request_in(&self.cfg, self.key.as_deref(), prompt, text, true);
        let ceiling = http_ceiling(&self.cfg);
        let mut call = agent()
            .post(&request.url)
            .config()
            .timeout_global(Some(ceiling))
            .build()
            .header("content-type", "application/json");
        if let Some(auth) = &request.authorization {
            call = call.header("authorization", auth);
        }
        let response = match call.send_json(&request.body) {
            Ok(r) => r,
            Err(ureq::Error::Timeout(_)) => return Err(RewordError::Ceiling),
            Err(e) => return Err(unreachable_from(&e.to_string(), &request.url)),
        };
        let status = response.status().as_u16();
        // A non-2xx never streams: it is one ordinary JSON body saying what
        // went wrong, and `parse_response` is what turns it into the right
        // variant -- the auth row, the model row, the rate-limit row. Read
        // it whole and hand it there rather than looking for `data:` lines
        // that will never come.
        if !(200..300).contains(&status) {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = response
                .into_body()
                .with_config()
                .limit(BODY_LIMIT)
                .read_to_vec()
                .unwrap_or_default();
            return parse_response(
                status,
                retry_after.as_deref(),
                &body,
                &self.cfg.model,
                &self.host,
            )
            .map(|_| ());
        }
        read_stream(response.into_body().into_reader(), emit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The prompt moved to `sayd-core` when it became a config setting;
    // aliased rather than re-spelled so these assert against the one copy
    // that is actually sent.
    use sayd_core::reword::NOTIFICATION_PROMPT as SYSTEM_PROMPT;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn cfg() -> RewordConfig {
        RewordConfig {
            enabled: true,
            base_url: "https://api.ppq.ai/v1".into(),
            model: "gpt-4o-mini".into(),
            // Every test that is not *about* the provider wants the one that
            // sends nothing extra, so the body under assertion is the common
            // request.
            provider: Some("generic".into()),
            ..RewordConfig::default()
        }
    }

    /// A model list is sorted, deduplicated and stripped of blanks.
    ///
    /// The wire order is whatever the server felt like -- llama-swap lists
    /// in config-file order -- and this is a menu someone is looking a name
    /// up in. Verified against a real llama-swap on this machine before the
    /// shape was fixed here; that check is not committed because it needs a
    /// server.
    #[test]
    fn a_model_list_is_sorted_and_deduplicated() {
        let body = br#"{"data":[{"id":"zeta"},{"id":"alpha"},{"id":"zeta"},
                       {"id":""},{"id":"  "},{"id":"beta"}]}"#;
        let (cfg, server) = serve(move |_req| {
            Some(http(200, "", std::str::from_utf8(body).unwrap()))
        });
        let got = list_models(&cfg, None).expect("the server answers");
        assert_eq!(got, ["alpha", "beta", "zeta"]);
        server.join().expect("server thread ends");
    }

    /// A server with no listing endpoint, or one that answers something
    /// unrecognisable, must not produce an error the user has to dismiss --
    /// the Model row is free text and a failed suggestion costs nothing.
    #[test]
    fn an_unusable_model_list_is_empty_rather_than_fatal() {
        let (cfg, server) = serve(|_req| Some(http(200, "", "not json at all")));
        assert_eq!(
            list_models(&cfg, None).expect("a body that is not JSON still parses to empty"),
            Vec::<String>::new()
        );
        server.join().expect("server thread ends");

        let (cfg, server) = serve(|_req| Some(http(200, "", r#"{"object":"list"}"#)));
        assert_eq!(
            list_models(&cfg, None).expect("a listing with no data key is empty"),
            Vec::<String>::new()
        );
        server.join().expect("server thread ends");
    }

    /// A refusal is reported, because it is the one case where the user can
    /// act: a 404 means this endpoint has no listing, a 401 means the key is
    /// wrong, and both are worth a line in the menu.
    #[test]
    fn a_refused_model_list_says_what_the_server_answered() {
        let (cfg, server) = serve(|_req| Some(http(404, "", "nope")));
        let Err(RewordError::NotConfigured(why)) = list_models(&cfg, None) else {
            panic!("a 404 must be reported, not silently empty");
        };
        assert!(why.contains("404"), "the status is the actionable part: {why}");
        server.join().expect("server thread ends");
    }

    /// The listing goes to `/models` beside `/chat/completions`, with the
    /// same trailing-slash handling.
    #[test]
    fn the_listing_url_sits_beside_the_completions_url() {
        for base in ["http://h/v1", "http://h/v1/"] {
            assert_eq!(sayd_core::reword::models_url(base), "http://h/v1/models");
        }
    }

    /// The URL for every row of §6's endpoint table, the trailing-slash
    /// case included, and the header that must *not* be sent to a local
    /// server that does not want one.
    #[test]
    fn a_request_names_one_url_and_omits_an_absent_key() {
        let mut c = cfg();
        c.base_url = "http://localhost:11434/v1/".into();
        let r = build_request(&c, None, SYSTEM_PROMPT, "Alice: dinner?");
        assert_eq!(r.url, "http://localhost:11434/v1/chat/completions");
        assert_eq!(
            r.authorization, None,
            "no key means no Authorization header at all -- which is exactly \
             right for a local server"
        );

        let r = build_request(&cfg(), Some("sk-abc"), SYSTEM_PROMPT, "Alice: dinner?");
        assert_eq!(r.url, "https://api.ppq.ai/v1/chat/completions");
        assert_eq!(r.authorization.as_deref(), Some("Bearer sk-abc"));
    }

    /// The body: both messages in order, `stream: false`, and the two
    /// generation parameters §3 fixes.
    #[test]
    fn the_body_carries_both_messages_in_order_and_never_streams() {
        let r = build_request(&cfg(), None, SYSTEM_PROMPT, "Alice: where do you want to go for dinner");
        assert_eq!(r.body["model"], "gpt-4o-mini");
        assert_eq!(r.body["stream"], false);
        assert_eq!(r.body["max_tokens"], 1200);
        assert_eq!(r.body["temperature"], 0.2);
        let messages = r.body["messages"].as_array().expect("an array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .expect("a string")
                .contains("read them aloud"),
            "the system prompt is the one in §3"
        );
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[1]["content"],
            "Alice: where do you want to go for dinner"
        );
    }

    /// Recorded bodies, one per row of §8's failure table, plus the two
    /// §12 warns about: a 200 carrying an `error` object, and a 500 for a
    /// model the server does not have.
    #[test]
    fn every_recorded_body_classifies_to_the_row_it_belongs_to() {
        let ok = br#"{"choices":[{"message":{"role":"assistant","content":"Alice is asking where you want to go for dinner"}}]}"#;
        assert_eq!(
            parse_response(200, None, ok, "gpt-4o-mini", "api.ppq.ai").as_deref(),
            Ok("Alice is asking where you want to go for dinner")
        );

        let unauthorized = br#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        assert_eq!(
            parse_response(401, None, unauthorized, "gpt-4o-mini", "api.ppq.ai"),
            Err(RewordError::Auth {
                status: 401,
                host: "api.ppq.ai".into(),
                message: Some("Incorrect API key provided".into()),
            })
        );
        assert!(matches!(
            parse_response(403, None, b"", "gpt-4o-mini", "api.ppq.ai"),
            Err(RewordError::Auth { status: 403, .. })
        ));

        let no_model =
            br#"{"error":{"message":"model 'llama3.2:3b' not found","type":"not_found"}}"#;
        assert_eq!(
            parse_response(404, None, no_model, "llama3.2:3b", "localhost"),
            Err(RewordError::NoSuchModel {
                status: 404,
                model: "llama3.2:3b".into(),
                message: Some("model 'llama3.2:3b' not found".into()),
            })
        );

        // §12: llama.cpp and friends are known to answer 500 for a missing
        // model. The body names it, so the body wins.
        assert_eq!(
            parse_response(500, None, no_model, "llama3.2:3b", "localhost"),
            Err(RewordError::NoSuchModel {
                status: 500,
                model: "llama3.2:3b".into(),
                message: Some("model 'llama3.2:3b' not found".into()),
            })
        );
        // ...and to answer 200 with an error object, which must never be
        // read as a rewrite.
        assert_eq!(
            parse_response(200, None, no_model, "llama3.2:3b", "localhost"),
            Err(RewordError::NoSuchModel {
                status: 200,
                model: "llama3.2:3b".into(),
                message: Some("model 'llama3.2:3b' not found".into()),
            })
        );
        let other_200_error = br#"{"error":{"message":"context length exceeded"}}"#;
        assert_eq!(
            parse_response(200, None, other_200_error, "llama3.2:3b", "localhost"),
            Err(RewordError::Malformed("context length exceeded".into())),
            "an error object that names nothing more specific is still not an answer"
        );

        let limited = br#"{"error":{"message":"Rate limit reached"}}"#;
        assert_eq!(
            parse_response(429, Some("12"), limited, "gpt-4o-mini", "api.ppq.ai"),
            Err(RewordError::RateLimited {
                retry_after: Some(Duration::from_secs(12)),
                message: Some("Rate limit reached".into()),
            })
        );
        assert_eq!(
            parse_response(429, None, limited, "gpt-4o-mini", "api.ppq.ai"),
            Err(RewordError::RateLimited {
                retry_after: None,
                message: Some("Rate limit reached".into()),
            })
        );
        assert_eq!(
            parse_response(
                429,
                Some("Wed, 21 Oct 2015 07:28:00 GMT"),
                limited,
                "m",
                "h"
            ),
            Err(RewordError::RateLimited {
                retry_after: None,
                message: Some("Rate limit reached".into()),
            }),
            "an HTTP-date Retry-After is not a number of seconds; fall back to the \
             fixed backoff rather than guessing"
        );

        // An empty `choices`, a truncated body, and a body that is not JSON
        // at all. A trusted operator does not make a truncated body parse.
        assert!(matches!(
            parse_response(200, None, br#"{"choices":[]}"#, "m", "h"),
            Err(RewordError::Malformed(_))
        ));
        assert!(matches!(
            parse_response(200, None, br#"{"choices":[{"message":{"rol"#, "m", "h"),
            Err(RewordError::Malformed(_))
        ));
        assert!(matches!(
            parse_response(200, None, b"<html>502 Bad Gateway</html>", "m", "h"),
            Err(RewordError::Malformed(_))
        ));
        assert!(matches!(
            parse_response(503, None, b"", "m", "h"),
            Err(RewordError::Malformed(_))
        ));
    }

    /// What a thinking model actually returns: the whole budget spent in
    /// `reasoning_content`, `content` empty, `finish_reason` "length". Today
    /// this is reported as a malformed response, which is wrong -- the response
    /// was perfectly well formed and the model simply never got to the answer.
    #[test]
    fn a_generation_that_hit_the_cap_is_not_called_malformed() {
        let body = br#"{"choices":[{"finish_reason":"length","message":
            {"role":"assistant","content":"","reasoning_content":"Thinking Process:\n1."}}]}"#;
        assert_eq!(
            parse_response(200, None, body, "gemma", "localhost"),
            Err(RewordError::Truncated { reasoning: true })
        );
    }

    /// The more dangerous half, and the reason the trigger is `finish_reason`
    /// alone. An answer cut off mid-sentence is *text*, so it passes the guard's
    /// length check and reaches the speaker -- the exact failure the generous
    /// token cap exists to prevent.
    #[test]
    fn a_truncated_answer_with_text_in_it_is_still_refused() {
        let body = br#"{"choices":[{"finish_reason":"length","message":
            {"content":"Alice is asking where you want to go for"}}]}"#;
        assert_eq!(
            parse_response(200, None, body, "gemma", "localhost"),
            Err(RewordError::Truncated { reasoning: false })
        );
    }

    /// OpenAI and llama.cpp both spell it `"length"`, but Gemini- and
    /// Anthropic-compatibility shims spell it `MAX_TOKENS` / `max_tokens` --
    /// against those, an unmatched string used to let the mid-sentence
    /// truncation this check exists to prevent fall through and be spoken,
    /// exactly the failure `a_truncated_answer_with_text_in_it_is_still_refused`
    /// pins for the `"length"` spelling.
    #[test]
    fn other_spellings_of_a_truncated_finish_reason_are_recognised() {
        let shouting = br#"{"choices":[{"finish_reason":"MAX_TOKENS","message":
            {"content":"Alice is asking where you want to go for"}}]}"#;
        assert_eq!(
            parse_response(200, None, shouting, "gemma", "localhost"),
            Err(RewordError::Truncated { reasoning: false })
        );

        let mixed_case = br#"{"choices":[{"finish_reason":"Length","message":
            {"content":"Alice is asking where you want to go for"}}]}"#;
        assert_eq!(
            parse_response(200, None, mixed_case, "gemma", "localhost"),
            Err(RewordError::Truncated { reasoning: false })
        );
    }

    /// Reasoning is not the fault; not finishing is. A model that thinks and
    /// then answers inside the cap has done exactly what was asked.
    #[test]
    fn reasoning_beside_a_finished_answer_is_accepted() {
        let body = br#"{"choices":[{"finish_reason":"stop","message":
            {"content":"Alice is asking where you want to go for dinner",
             "reasoning_content":"brief"}}]}"#;
        assert_eq!(
            parse_response(200, None, body, "gemma", "localhost").as_deref(),
            Ok("Alice is asking where you want to go for dinner")
        );
    }

    /// **The ordering rule, against the bodies that broke the old one.**
    ///
    /// 401, 403 and 429 mean one thing on every OpenAI-compatible server,
    /// so a body that names the configured model may say *what* happened
    /// and never *which row it was*. Classified body-first these two real
    /// bodies both became [`RewordError::NoSuchModel`]: the 429 then set no
    /// rate-limit backoff, because `RewordState::record` sets one only for
    /// [`RewordError::RateLimited`], and the 403 never latched the auth
    /// breaker, so a key with no access to that model was retried for the
    /// life of the daemon.
    #[test]
    fn an_unambiguous_status_is_never_overruled_by_a_body_naming_the_model() {
        // OpenAI's actual 429 body, verbatim. It names the model
        // space-delimited -- exactly the boundary `mentions_model`
        // accepts.
        let limited = br#"{"error":{"message":"Rate limit reached for gpt-4o-mini in organization org-abc on requests per min (RPM): Limit 3, Used 3. Please try again in 20s."}}"#;
        match parse_response(429, Some("20"), limited, "gpt-4o-mini", "api.openai.com") {
            Err(RewordError::RateLimited {
                retry_after,
                message,
            }) => {
                assert_eq!(
                    retry_after,
                    Some(Duration::from_secs(20)),
                    "§8's `Retry-After` is only honoured on the row that reads it"
                );
                assert!(message
                    .expect("the provider said why")
                    .starts_with("Rate limit reached for gpt-4o-mini"));
            }
            other => panic!("a 429 is a rate limit whatever its body says, got {other:?}"),
        }

        // Per-model key scoping, which is ordinary on gateways and on a
        // proxy in front of a TEE.
        let scoped =
            br#"{"error":{"message":"Your API key does not have access to model gpt-5.6-sol"}}"#;
        for status in [401u16, 403] {
            assert_eq!(
                parse_response(status, None, scoped, "gpt-5.6-sol", "gateway.example"),
                Err(RewordError::Auth {
                    status,
                    host: "gateway.example".into(),
                    message: Some("Your API key does not have access to model gpt-5.6-sol".into()),
                }),
                "a key the provider will not accept must latch, whatever it names"
            );
        }

        // The other half of the trade, and the reason the model rule is
        // kept at all: on a status that means nothing, the body still
        // decides. 400 joins the 200 and 500 cases above.
        assert!(matches!(
            parse_response(
                400,
                None,
                br#"{"error":{"message":"model 'gpt-5.6-sol' not found"}}"#,
                "gpt-5.6-sol",
                "localhost"
            ),
            Err(RewordError::NoSuchModel { status: 400, .. })
        ));
    }

    /// **What a provider may write into the journal.** §8 asks for the
    /// reason and the first 80 characters; unbounded, `error.message` is a
    /// 60 KB warning line that can forge further `warning: reword:` lines
    /// inside itself and run ANSI escapes at whoever reads it.
    #[test]
    fn a_providers_message_cannot_write_the_journal() {
        let huge = serde_json::json!({ "error": { "message": "A".repeat(60_000) } }).to_string();
        let Err(RewordError::Malformed(message)) =
            parse_response(400, None, huge.as_bytes(), "m", "h")
        else {
            panic!("an error object is not an answer");
        };
        assert_eq!(
            message.chars().count(),
            MESSAGE_CHARS + 1,
            "80 characters, and the ellipsis that says there was more: {message}"
        );

        // Both of these are perfectly legal JSON strings, and both survive
        // `serde_json` into the `String` as the bytes they name.
        let hostile = serde_json::json!({
            "error": {
                "message": "down\nwarning: reword: your API key was revoked\u{1b}[2J\rgone"
            }
        })
        .to_string();
        let Err(RewordError::Malformed(message)) =
            parse_response(500, None, hostile.as_bytes(), "m", "h")
        else {
            panic!("an error object is not an answer");
        };
        assert!(
            !message.chars().any(char::is_control),
            "no newline may forge a second log line and no escape may reach a \
             terminal: {message:?}"
        );
        assert!(
            message.contains("your API key was revoked"),
            "the words still reach the user, which is the whole point: {message:?}"
        );

        // A message that fits is passed through exactly, ellipsis and all
        // -- the cap must not become a tax on every ordinary reason.
        assert_eq!(
            parse_response(500, None, br#"{"error":{"message":"  boom  "}}"#, "m", "h"),
            Err(RewordError::Malformed("boom".into()))
        );
    }

    /// MINOR 4: a key that cannot go into a header is a *configuration*
    /// failure, and must never reach the transport breaker.
    ///
    /// An en-dash or a smart quote in a pasted key is the whole of the case.
    /// Measured before this check existed: `ureq` refused the header with
    /// `protocol: authorization header is not a string`, `send`'s catch-all
    /// turned that into `RewordError::Unreachable`, and three notifications
    /// opened the transport breaker for a minute over something no cooldown
    /// will fix -- against the explicit statement, in the arm right beside
    /// that catch-all, that such a key "belongs on the row that says so once
    /// per run".
    ///
    /// Refused in `HttpRewriter::new` rather than in `send`, so it is said
    /// once at first use rather than once per utterance, exactly like an
    /// unusable `base_url`. No server is involved: nothing is sent.
    #[test]
    fn a_key_an_http_header_cannot_carry_is_a_configuration_failure() {
        let cfg = RewordConfig {
            enabled: true,
            base_url: "http://127.0.0.1:11434/v1".into(),
            // An en-dash, which is what a key copied out of a web page that
            // typographed the hyphen actually contains.
            api_key: "sk\u{2013}0123456789".into(),
            api_key_env: String::new(),
            provider: Some("generic".into()),
            ..RewordConfig::default()
        };

        match HttpRewriter::new(&cfg).map(|_| "a client") {
            Err(RewordError::NotConfigured(reason)) => {
                assert!(
                    reason.contains("reword.api_key"),
                    "the line has to name the setting to fix: {reason}"
                );
                assert!(
                    reason.contains("en-dash"),
                    "and the cause, because the character is invisible in a \
                     settings field: {reason}"
                );
            }
            other => panic!("a key like this is not an outage, got {other:?}"),
        }

        // An ordinary key, and no key at all, are both fine -- the check
        // must not become a tax on every local server.
        let plain = RewordConfig {
            api_key: "sk-0123456789".into(),
            ..cfg.clone()
        };
        assert!(HttpRewriter::new(&plain).is_ok());
        let none = RewordConfig {
            api_key: String::new(),
            ..cfg.clone()
        };
        assert!(HttpRewriter::new(&none).is_ok());

        // A newline is the other way a pasted key arrives broken, and the
        // one that would forge a header rather than merely fail to encode.
        let newline = RewordConfig {
            api_key: "sk-abc\ndef".into(),
            ..cfg
        };
        assert!(matches!(
            HttpRewriter::new(&newline).map(|_| "a client"),
            Err(RewordError::NotConfigured(_))
        ));
    }

    /// IMPORTANT 2: the same bound, reached through the other variant.
    ///
    /// `RewordError::Unreachable`'s detail is `ureq`'s own error text, and
    /// that text can quote bytes the provider chose -- a `Location` header,
    /// a hostname. It lands in a `warning: reword:` line and in the settings
    /// window's Test subtitle, so unbounded it is the identical hazard
    /// `sanitise_message` already closes for `error.message`. Measured
    /// before this: a 60,000-character `Location` produced a 60,094-byte
    /// warning line and a 60 KB subtitle, and a TAB survived.
    #[test]
    fn an_unreachable_providers_reason_cannot_write_the_journal() {
        let hostile = format!(
            "io: \tconnect\u{1b}[2J to warning: reword: your API key was revoked {}",
            "A".repeat(60_000)
        );
        let url = format!("http://box.lan/{}/v1/chat/completions", "p".repeat(60_000));

        let RewordError::Unreachable(detail) = unreachable_from(&hostile, &url) else {
            panic!("this variant is the one under test");
        };

        // Two halves of `MESSAGE_CHARS` plus their ellipses, and the
        // ` (` and `)` between and after them. The *whole* line is bounded,
        // not half of it: a 60 KB URL is still a 60 KB warning.
        assert_eq!(
            detail.chars().count(),
            2 * (MESSAGE_CHARS + 1) + " ()".len()
        );
        assert!(
            !detail.chars().any(char::is_control),
            "no TAB, no escape, no newline reaches the journal: {detail:?}"
        );
        assert!(
            detail.contains("connect"),
            "the reason still reaches the user, which is the whole point: {detail}"
        );
        assert!(
            detail.contains("box.lan"),
            "and so does the endpoint that could not be reached: {detail}"
        );

        // An ordinary failure is passed through untouched: the cap must not
        // become a tax on every real outage.
        assert_eq!(
            unreachable_from("io: Connection refused", "http://localhost:11434/v1"),
            RewordError::Unreachable(
                "io: Connection refused (http://localhost:11434/v1)".to_string()
            )
        );
    }

    /// One field of the wrong type may cost that field and nothing else.
    /// With a plain `Option` the body below fails the whole parse and a
    /// good rewrite is thrown away with an `error` this client does not
    /// even understand.
    #[test]
    fn a_field_of_the_wrong_type_costs_only_that_field() {
        assert_eq!(
            parse_response(
                200,
                None,
                br#"{"choices":[{"message":{"content":"a good rewrite"}}],"error":"oops"}"#,
                "m",
                "h"
            )
            .as_deref(),
            Ok("a good rewrite"),
            "an `error` this client cannot read is not a reason to discard the answer"
        );
        // The mirror: a `choices` of the wrong type must not cost the
        // diagnosis that says why there is no answer in it.
        assert_eq!(
            parse_response(
                200,
                None,
                br#"{"choices":"soon","error":{"message":"model 'm' not found"}}"#,
                "m",
                "h"
            ),
            Err(RewordError::NoSuchModel {
                status: 200,
                model: "m".into(),
                message: Some("model 'm' not found".into()),
            })
        );
        // ...and none of these is an answer either.
        for body in [
            &br#"{"choices":[{"message":{"content":42}}]}"#[..],
            br#"{"choices":[{"message":"hello"}]}"#,
            br#"{"choices":{"0":{"message":{"content":"hi"}}}}"#,
            br#"{"error":{"message":{"detail":"nested"}}}"#,
        ] {
            assert!(
                matches!(
                    parse_response(200, None, body, "m", "h"),
                    Err(RewordError::Malformed(_))
                ),
                "{} must not be read as a rewrite",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// The shapes a half-configured local server and a reverse proxy
    /// actually produce, none of which a provider's documentation
    /// describes. Every one of them must come back as an error rather than
    /// as something to speak -- and none of them may panic, because the
    /// [`Rewriter`] trait is the boundary and a client that unwraps a
    /// response is how a provider gets to take the announcement down.
    #[test]
    fn a_body_no_documentation_describes_is_an_error_and_never_a_panic() {
        // `null` where a string belongs. `#[serde(default)]` on a bare
        // `String` would fail the whole parse here; the point of the
        // `Option` is that the rest of the body survives.
        assert!(matches!(
            parse_response(
                200,
                None,
                br#"{"choices":[{"message":{"role":"assistant","content":null}}]}"#,
                "llama3.2:3b",
                "localhost"
            ),
            Err(RewordError::Malformed(_))
        ));
        // ...and when the same body also says why, the reason survives the
        // null and reaches the user.
        assert_eq!(
            parse_response(
                200,
                None,
                br#"{"choices":[{"message":{"content":null}}],"error":{"message":"model 'llama3.2:3b' not found"}}"#,
                "llama3.2:3b",
                "localhost"
            ),
            Err(RewordError::NoSuchModel {
                status: 200,
                model: "llama3.2:3b".into(),
                message: Some("model 'llama3.2:3b' not found".into()),
            }),
            "a null alongside an error object must not cost the diagnosis"
        );

        // Nothing at all, and whitespace that is not an answer.
        for body in [
            &b""[..],
            b"{}",
            b"null",
            b"[]",
            br#"{"choices":null}"#,
            br#"{"choices":[{}]}"#,
            br#"{"choices":[{"message":null}]}"#,
            br#"{"choices":[{"message":{}}]}"#,
            br#"{"choices":[{"message":{"content":"   \n "}}]}"#,
            // A streaming chunk, which is what a server that ignored
            // `stream: false` sends: `delta`, not `message`.
            br#"{"choices":[{"delta":{"content":"Alice"}}]}"#,
            // An `error` that is a bare string rather than an object, and
            // an `error` object with no message: neither is a rewrite.
            br#"{"error":"something went wrong"}"#,
            br#"{"error":{}}"#,
            br#"{"error":{"message":"   "}}"#,
            // A proxy's HTML, and a body that is not text at all.
            b"<html><head><title>502</title></head></html>",
            &[0xff, 0xfe, 0x00, 0x01],
        ] {
            assert!(
                matches!(
                    parse_response(200, None, body, "llama3.2:3b", "localhost"),
                    Err(RewordError::Malformed(_))
                ),
                "{:?} must not be read as a rewrite",
                String::from_utf8_lossy(body)
            );
        }

        // A `Retry-After` a hostile or broken server might send. None of
        // these may panic or parse to something absurd; the fixed backoff
        // in `super` is the fallback.
        for header in [
            "",
            "   ",
            "-1",
            "not a number",
            "99999999999999999999",
            "12.5",
        ] {
            assert!(matches!(
                parse_response(429, Some(header), b"", "llama3.2:3b", "localhost"),
                Err(RewordError::RateLimited {
                    retry_after: None,
                    ..
                })
            ));
        }
        assert_eq!(
            parse_response(429, Some(" 7 "), b"", "llama3.2:3b", "localhost"),
            Err(RewordError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
                message: None,
            }),
            "a header with the whitespace a header often has is still a number"
        );
    }

    /// An unusable `base_url` is a configuration failure the client refuses
    /// to be built for, not a request that fails later.
    #[test]
    fn an_unusable_base_url_refuses_to_build_a_client() {
        for bad in ["", "localhost:11434", "ftp://example.com"] {
            let mut c = cfg();
            c.base_url = bad.into();
            assert!(
                matches!(HttpRewriter::new(&c), Err(RewordError::NotConfigured(_))),
                "{bad:?} must not produce a usable client"
            );
        }
    }

    /// Read one whole HTTP request off `stream`, headers *and* body.
    ///
    /// A single `read` is not enough and the difference is a flaky test
    /// rather than a failing one: `ureq` writes the headers and the body
    /// with separate calls, so the first `read` routinely returns the
    /// headers alone and an assertion about the JSON that was sent then
    /// fails on a machine that scheduled it that way. Reads until the
    /// header terminator has been seen and `content-length` bytes have
    /// followed it, with a timeout so a test that never gets there fails
    /// instead of hanging the suite.
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut raw: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4) {
                let head = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() - header_end >= length {
                    break;
                }
            }
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
        String::from_utf8_lossy(&raw).to_string()
    }

    /// A one-shot HTTP server on an ephemeral loopback port. `respond` is
    /// handed the raw request bytes and returns the raw response to write;
    /// returning `None` closes the connection without answering, which is
    /// how the transport-error path is pinned.
    fn serve(
        respond: impl FnOnce(String) -> Option<Vec<u8>> + Send + 'static,
    ) -> (RewordConfig, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let request = read_request(&mut stream);
            if let Some(response) = respond(request) {
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
            // Dropping `stream` closes the connection either way.
        });
        let cfg = RewordConfig {
            enabled: true,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            model: "test-model".into(),
            api_key: "sk-loopback".into(),
            api_key_env: String::new(),
            provider: Some("generic".into()),
            ..RewordConfig::default()
        };
        (cfg, handle)
    }

    fn http(status: u16, extra_headers: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n{extra_headers}connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// The happy path through the real agent: the request that goes out
    /// carries what §5 says, and the answer that comes back is read at
    /// `choices[0].message.content`.
    #[test]
    fn a_real_request_over_a_loopback_socket_round_trips() {
        let seen = Arc::new(Mutex::new(String::new()));
        let s = seen.clone();
        let (cfg, server) = serve(move |request| {
            *super::super::lock(&s) = request;
            Some(http(
                200,
                "",
                r#"{"choices":[{"message":{"content":"Alice is asking about dinner"}}]}"#,
            ))
        });

        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        assert_eq!(
            rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?").as_deref(),
            Ok("Alice is asking about dinner")
        );

        server.join().expect("the server thread");
        let request = super::super::lock(&seen).clone();
        assert!(
            request.starts_with("POST /v1/chat/completions "),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-loopback"),
            "{request}"
        );
        // Whitespace-insensitive: `ureq`'s `send_json` serialises pretty,
        // and this assertion is about the field, not about the formatting.
        let compact: String = request.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("\"stream\":false"), "{request}");
    }

    /// `parse_base_url` checks the scheme and finds the host; it is not a
    /// URL parser, so a `base_url` it accepts can still be one `ureq`
    /// refuses. That is a configuration failure rather than an outage, and
    /// classifying it as one keeps three notifications from opening the
    /// transport breaker over a typo that no cooldown will fix.
    ///
    /// The other half of a request a user types -- the key -- used to be
    /// checked here too, at request time. It is refused when the client is
    /// built now (MINOR 4), so it lives in
    /// [`a_key_an_http_header_cannot_carry_is_a_configuration_failure`].
    #[test]
    fn a_base_url_ureq_will_not_parse_is_a_configuration_failure() {
        let cfg = RewordConfig {
            base_url: "http://exa mple.com/v1".into(),
            api_key_env: String::new(),
            provider: Some("generic".into()),
            ..RewordConfig::default()
        };
        let rewriter = HttpRewriter::new(&cfg).expect("the scheme and host are readable");
        assert!(
            matches!(
                rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?"),
                Err(RewordError::NotConfigured(_))
            ),
            "a URL that cannot be requested is not a provider that is down"
        );
    }

    /// A server that accepts the connection and closes it without
    /// answering is the transport-error path, and it must not be mistaken
    /// for a malformed response.
    #[test]
    fn a_connection_closed_without_an_answer_is_a_transport_error() {
        let (cfg, server) = serve(|_| None);
        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        assert!(
            matches!(
                rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?"),
                Err(RewordError::Unreachable(_))
            ),
            "a closed connection is unreachable, not malformed"
        );
        server.join().expect("the server thread");
    }

    /// A closed port -- nothing listening at all. The most common first-run
    /// failure there is: a `base_url` pointing at a server that is not
    /// running. The detail it carries is the one line a user pastes into an
    /// issue, so what is asserted is the part of it that is ours: `ureq`
    /// 3.4 says `io: Connection refused` and names no address at all, and
    /// the line that does name one -- `info: reword: sending text to …` --
    /// is a different line, printed once per run, that a user quoting the
    /// warning will leave behind. Together they reach the journal as
    /// `warning: reword: could not reach the provider: io: Connection
    /// refused (http://127.0.0.1:PORT/v1/chat/completions)`. The wording
    /// before the parenthesis is `ureq`'s and is not asserted.
    #[test]
    fn a_closed_port_is_a_transport_error() {
        // Bind, read the port, drop the listener: the port is now free and
        // almost certainly still unused.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let cfg = RewordConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key_env: String::new(),
            provider: Some("generic".into()),
            ..RewordConfig::default()
        };
        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        match rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?") {
            Err(RewordError::Unreachable(detail)) => assert!(
                detail.contains(&format!("127.0.0.1:{port}")),
                "`io: Connection refused` names no address, and the line that does \
                 name one is a separate `info:` line a user pasting this warning \
                 will leave out: {detail}"
            ),
            other => panic!("a closed port is unreachable, got {other:?}"),
        }
    }

    /// The three numbers IMPORTANT 1 and IMPORTANT 4 set, asserted on the
    /// agent production builds rather than on its behaviour.
    ///
    /// Not a substitute for the two tests below -- they are what says the
    /// numbers *do* anything -- but the necessary complement to one of them:
    /// whether a 70,000-byte header name reaches `ureq-proto`'s parser
    /// depends on how the reads land, so the header-size test was measured
    /// passing and then panicking on the same code. The buffer bound is what
    /// makes that deterministic and it cannot be observed from outside, so
    /// it is pinned here.
    ///
    /// The relationships, not the values: raising either limit is allowed,
    /// raising it past what the parser survives is not.
    #[test]
    fn the_agent_cannot_be_handed_a_header_that_panics_its_parser() {
        /// The header-name length `ureq-proto` panics on. Bisected: one
        /// less is clean.
        const PANICS_AT: usize = 65_536;

        let agent = build_agent();
        let config = agent.config();

        assert!(
            config.input_buffer_size() < PANICS_AT,
            "the parser only sees what is in the read buffer, so this is what \
             keeps a name that long from ever being assembled: {}",
            config.input_buffer_size()
        );
        assert!(
            config.max_response_header_size() < config.input_buffer_size(),
            "and the header limit has to be reached *before* the buffer fills, \
             or nothing reports the failure: {} vs {}",
            config.max_response_header_size(),
            config.input_buffer_size()
        );
        assert_eq!(
            config.max_redirects(),
            0,
            "IMPORTANT 1: this client has exactly one endpoint to talk to"
        );
    }

    /// **The transport must never be what ends a rewrite first**, and this
    /// is the half of that promise a slow test could not check.
    ///
    /// The agent is a `OnceLock`: whatever ceiling it carries is fixed for
    /// the life of the process, while `reword.timeout_ms` is not fixed at
    /// all -- it has no upper bound and it changes under the settings
    /// window. An agent-level `timeout_global` is therefore the one way to
    /// reintroduce the bug this milestone removed *invisibly*: a 30 s
    /// deadline would be cut off at whatever the agent was built with, the
    /// rewrite would be dropped, and the user would see a provider that did
    /// not answer rather than a client that stopped asking.
    ///
    /// So there is no ceiling here to be stale. The only one is the argument
    /// [`send`] is given per request, from the config that request is for.
    #[test]
    fn the_cached_agent_carries_no_ceiling_that_could_outlive_a_config() {
        for (what, agent) in [
            ("a fresh agent", build_agent()),
            ("the cached one", agent().clone()),
        ] {
            assert_eq!(
                agent.config().timeouts().global,
                None,
                "{what} must carry no global timeout: a ceiling fixed for the \
                 life of the process cannot follow reword.timeout_ms, and a \
                 lower one would silently truncate it"
            );
        }
    }

    /// IMPORTANT 1: a redirect is not followed, to any host.
    ///
    /// `ureq`'s default is 10 redirects, over plain HTTP as readily as
    /// HTTPS, and the target is chosen by the provider. The text and the
    /// key do not survive the hop, but the *request* does -- an SSRF
    /// primitive against loopback, `169.254.169.254` and the LAN -- and
    /// whatever answers there, not `base_url`, would be the candidate that
    /// reached `check()` and the speaker.
    ///
    /// The second listener is the whole test: it is bound and never
    /// answered, so "was it contacted" is a question this can ask. Delete
    /// `max_redirects(0)` and the `accept` below succeeds.
    #[test]
    fn a_redirect_is_refused_rather_than_followed() {
        let elsewhere = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = elsewhere.local_addr().expect("addr").port();
        elsewhere
            .set_nonblocking(true)
            .expect("non-blocking listener");

        let (cfg, server) = serve(move |_request| {
            Some(http(
                302,
                &format!("location: http://127.0.0.1:{port}/v1/chat/completions\r\n"),
                "",
            ))
        });

        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        let out = rewriter.reword(SYSTEM_PROMPT, "Alice: where do you want to go for dinner");
        server.join().expect("the server thread");

        assert!(
            matches!(out, Err(RewordError::Malformed(_))),
            "a 3xx carries no choices[0].message.content, which is exactly \
             what `Malformed` says: {out:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            elsewhere.accept().is_err(),
            "the host the provider named must never be contacted"
        );
    }

    /// IMPORTANT 4: a hostile response *header* cannot panic the request
    /// thread.
    ///
    /// `ureq`'s default `max_response_header_size` is 64 KiB, and `run.rs`
    /// calls `try_response` before comparing what it has read against it --
    /// so a header name of exactly that size or larger panics inside
    /// `ureq-proto`'s parser. Bisected on this dependency: 65,535 clean,
    /// 65,536 panics. It is contained (the `JoinError` becomes `Malformed`,
    /// the permit is released by the unwind), but it puts a backtrace on the
    /// daemon's stderr and anyone who can answer the socket can fire it,
    /// including a host a redirect named.
    ///
    /// This test would be worth having with no assertion at all: with either
    /// limit removed it *panics*, and a panicking test fails. The
    /// classification is asserted as well because a hostile provider must
    /// not be able to open the transport breaker with it -- `Malformed` is
    /// the row that leaves the next notification its chance, the same call
    /// `BodyExceedsLimit` gets.
    ///
    /// Driven through the cached production agent, which is the one that has
    /// to survive this.
    #[test]
    fn a_hostile_response_header_is_refused_rather_than_panicked_on() {
        let name = "x".repeat(70_000);
        let (cfg, server) = serve(move |_request| {
            Some(http(
                200,
                &format!("{name}: y\r\n"),
                r#"{"choices":[{"message":{"content":"Alice is asking about dinner"}}]}"#,
            ))
        });

        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        let out = rewriter.reword(SYSTEM_PROMPT, "Alice: where do you want to go for dinner");
        // The server may have been killed mid-write by the client giving up
        // on the response, so its own thread is allowed to have failed.
        let _ = server.join();

        match out {
            Err(RewordError::Malformed(detail)) => assert!(
                detail.contains("headers"),
                "the reason must name what was refused: {detail}"
            ),
            other => panic!("a response this client will not read is not an answer: {other:?}"),
        }
    }

    /// **§9's ceiling, pinned rather than commented.**
    ///
    /// The knob is [`send`]'s `timeout_global`, and it is the single most
    /// expensive one in this file: it is the *only* thing that bounds the
    /// blocking thread, because `tokio::time::timeout` abandons an `.await`
    /// and never the task behind it. Driven here at 400 ms through the same
    /// call production makes with `http_ceiling`, so deleting that line --
    /// which used to leave all thirty `reword::` tests passing -- fails this
    /// test instead.
    ///
    /// It is now also the test that says a ceiling set *per request* still
    /// bounds the thread at all: the agent this runs against carries no
    /// global timeout of its own, so nothing but the argument can end this
    /// call.
    ///
    /// The server accepts the connection, reads the whole request, and
    /// then says nothing at all: there is no other thing in this client
    /// that can end that call.
    #[test]
    fn a_provider_that_never_answers_hits_the_ceiling() {
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (cfg, server) = serve(move |_| {
            // Bounded at 20 s rather than forever, so that a build with no
            // global timeout fails this test in 20 s -- the connection
            // closes and the outcome is `Unreachable` -- rather than
            // hanging the suite with no verdict at all.
            let _ = held.recv_timeout(Duration::from_secs(20));
            None
        });

        let agent = build_agent();
        let request = build_request(&cfg, None, SYSTEM_PROMPT, "Alice: dinner?");
        let started = std::time::Instant::now();
        let outcome = send(
            &agent,
            &request,
            Duration::from_millis(400),
            &cfg.model,
            "127.0.0.1",
        );
        let elapsed = started.elapsed();
        let _ = release.send(());
        server.join().expect("the server thread");

        assert!(
            matches!(outcome, Err(RewordError::Ceiling)),
            "a provider that accepts and then never answers is the ceiling, got {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the ceiling that ended the call must be the one that was set, not a \
             timeout somewhere else: {elapsed:?}"
        );
    }

    /// §7 says the text goes to `base_url`, and the `sending text to …`
    /// line names `base_url`. `ureq`'s default reads `ALL_PROXY` and
    /// friends, which would make both statements false in a shell that has
    /// one set -- measured: a fake proxy received the `CONNECT`.
    ///
    /// Pinned in a **child process**, because the variables have to be set
    /// for the assertion to mean anything and setting them here would race
    /// every other test that builds an agent -- `ureq` reads the
    /// environment when the config is built, so the flakiness would land
    /// on the ceiling test, which would then try to reach a proxy that is
    /// not there.
    #[test]
    fn the_agent_ignores_an_environment_proxy() {
        let exe = std::env::current_exe().expect("the test binary");
        let output = std::process::Command::new(exe)
            .args([
                "an_environment_proxy_is_not_picked_up",
                "--ignored",
                "--nocapture",
            ])
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .expect("re-run this test binary");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            output.status.success(),
            "the child said: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // A filter that matches nothing exits 0, which would make the
        // assertion above vacuous.
        assert!(
            stdout.contains("1 passed"),
            "the child must actually have run the test: {stdout}"
        );
    }

    /// The body of [`the_agent_ignores_an_environment_proxy`], which is
    /// the only thing that runs it -- with the proxy variables set.
    #[test]
    #[ignore = "run by the_agent_ignores_an_environment_proxy in a child process"]
    fn an_environment_proxy_is_not_picked_up() {
        assert!(
            ureq::Agent::config_builder().build().proxy().is_some(),
            "this test is meaningless unless the environment really did reach \
             `ureq`; the parent sets ALL_PROXY, HTTP_PROXY and HTTPS_PROXY"
        );
        assert!(
            build_agent().config().proxy().is_none(),
            "the text goes to base_url and nowhere else (§7)"
        );
    }

    /// The long tail §12 names, driven through the real agent rather than
    /// only through `parse_response`: a 4xx must reach the classifier with
    /// its body intact. Left on `ureq`'s default, `http_status_as_error`
    /// would turn this into an `Err` with the body discarded, and the
    /// message below -- the whole reason the Test row can say "check the
    /// key" -- would be gone.
    #[test]
    fn a_4xx_body_survives_the_client_and_reaches_the_classifier() {
        let (cfg, server) = serve(|_| {
            Some(http(
                401,
                "",
                r#"{"error":{"message":"Incorrect API key provided"}}"#,
            ))
        });
        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        assert_eq!(
            rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?"),
            Err(RewordError::Auth {
                status: 401,
                host: "127.0.0.1".into(),
                message: Some("Incorrect API key provided".into()),
            })
        );
        server.join().expect("the server thread");
    }

    /// `Retry-After` off a real header, and a 500 whose body names the
    /// model -- the two shapes a proxy in front of a TEE is most likely to
    /// produce.
    #[test]
    fn a_rate_limit_header_and_a_500_naming_the_model_both_classify() {
        let (cfg, server) = serve(|_| {
            Some(http(
                429,
                "retry-after: 7\r\n",
                r#"{"error":{"message":"slow down"}}"#,
            ))
        });
        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        assert_eq!(
            rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?"),
            Err(RewordError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
                message: Some("slow down".into()),
            })
        );
        server.join().expect("the server thread");

        let (cfg, server) = serve(|_| {
            Some(http(
                500,
                "",
                r#"{"error":{"message":"failed to load model 'test-model'"}}"#,
            ))
        });
        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        assert!(
            matches!(
                rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?"),
                Err(RewordError::NoSuchModel { status: 500, .. })
            ),
            "a server that answers 500 for a missing model must still be diagnosable"
        );
        server.join().expect("the server thread");
    }

    /// An HTML error page from a reverse proxy, over the wire, at a status
    /// that says nothing useful. This is the shape that produces the worst
    /// possible message -- and it still must be an error rather than a
    /// sentence read aloud to the user.
    #[test]
    fn a_proxys_html_error_page_is_not_a_rewrite() {
        let (cfg, server) = serve(|_| {
            let page = "<html><head><title>502 Bad Gateway</title></head></html>";
            Some(
                format!(
                    "HTTP/1.1 502 Bad Gateway\r\ncontent-type: text/html\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                )
                .into_bytes(),
            )
        });
        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        assert_eq!(
            rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?"),
            Err(RewordError::Malformed("HTTP 502".into())),
            "an HTML page is not JSON and 502 is all there is to say about it"
        );
        server.join().expect("the server thread");
    }

    /// A 200 with an empty body, which is what a server that died
    /// mid-answer leaves behind. Not a rewrite, and not a transport
    /// failure either -- the request completed.
    #[test]
    fn an_empty_200_is_malformed_rather_than_spoken() {
        let (cfg, server) = serve(|_| Some(http(200, "", "")));
        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        assert!(matches!(
            rewriter.reword(SYSTEM_PROMPT, "Alice: dinner?"),
            Err(RewordError::Malformed(_))
        ));
        server.join().expect("the server thread");
    }

    /// The measured fix. A thinking model asked nothing special reasons on 9
    /// requests of 10 and blows through any cap it is given; this field is what
    /// stops it, verified against the local llama.cpp router on 6 of 6.
    #[test]
    fn llama_cpp_asks_the_model_not_to_think() {
        let mut c = cfg();
        c.provider = Some("llama-cpp".into());
        let r = build_request(&c, None, SYSTEM_PROMPT, "Alice: dinner?");
        assert_eq!(
            r.body["chat_template_kwargs"]["enable_thinking"],
            serde_json::json!(false)
        );
    }

    /// The other half, and the half that must be asserted on the *body*: a
    /// remote OpenAI-compatible provider rejects an unknown top-level field
    /// rather than ignoring it, so "generic sends nothing extra" is a promise
    /// about bytes, not about intent.
    #[test]
    fn generic_sends_the_common_request_and_nothing_else() {
        let r = build_request(&cfg(), None, SYSTEM_PROMPT, "Alice: dinner?");
        let obj = r.body.as_object().expect("a JSON object");
        assert!(
            !obj.contains_key("chat_template_kwargs"),
            "the key must be absent, not null: {}",
            r.body
        );
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["max_tokens", "messages", "model", "stream", "temperature"],
            "a provider that is not llama.cpp sees exactly the request it saw before"
        );
    }

    /// Reported where every other unusable setting is: once per run, on the row
    /// that says the endpoint is not configured, with the text spoken as
    /// written. Not a startup failure -- `--reword` is a submission, and the
    /// daemon may have been started with `enabled = false`.
    #[test]
    fn a_client_cannot_be_built_without_a_usable_provider() {
        let mut c = cfg();
        c.provider = None;
        match HttpRewriter::new(&c) {
            Err(RewordError::NotConfigured(reason)) => {
                assert!(reason.contains("reword.provider"), "{reason}");
                for name in sayd_core::config::Provider::NAMES {
                    assert!(reason.contains(name), "{name} must be offered: {reason}");
                }
            }
            other => panic!("an unset provider must not build a client: {other:?}"),
        }

        c.provider = Some("llama.cpp".into());
        match HttpRewriter::new(&c) {
            Err(RewordError::NotConfigured(reason)) => {
                assert!(
                    reason.contains("llama.cpp"),
                    "the typo must be quoted: {reason}"
                )
            }
            other => panic!("an unrecognised provider must not build a client: {other:?}"),
        }
    }

    /// The regression guard for the hand-written `Debug` impl above:
    /// nothing else in this file would notice a derive quietly creeping back
    /// in, since `{other:?}` in
    /// `a_client_cannot_be_built_without_a_usable_provider` only ever hits
    /// the `Err` arm and never has a key to leak. This builds a rewriter
    /// that *does* hold one and checks the formatted output both omits it
    /// and still carries `host`, so the impl earns its keep on both halves
    /// of the trade.
    #[test]
    fn debug_never_prints_the_api_key() {
        let mut c = cfg();
        c.api_key = "sk-SECRETVALUE".into();
        c.api_key_env = String::new();
        let rewriter = HttpRewriter::new(&c).expect("cfg() is a usable provider");

        let debug = format!("{rewriter:?}");
        assert!(
            !debug.contains("SECRETVALUE"),
            "the key must never appear in Debug output: {debug}"
        );
        assert!(
            debug.contains(&rewriter.host),
            "host is the useful field Debug exists to keep: {debug}"
        );
    }

    /// The body must carry the configured cap, not a constant. Task 1 pins the
    /// arithmetic; this pins the wiring.
    #[test]
    fn the_request_carries_the_configured_token_cap() {
        let mut c = cfg();

        assert_eq!(
            build_request(&c, None, SYSTEM_PROMPT, "x").body["max_tokens"],
            1200,
            "the default max_chars of 400"
        );

        c.max_chars = 32;
        assert_eq!(build_request(&c, None, SYSTEM_PROMPT, "x").body["max_tokens"], 96);

        c.max_chars = 2000;
        assert_eq!(build_request(&c, None, SYSTEM_PROMPT, "x").body["max_tokens"], 6000);
    }
}
