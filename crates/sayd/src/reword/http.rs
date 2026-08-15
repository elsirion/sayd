//! The OpenAI-compatible client. The one file behind
//! `#[cfg(feature = "reword")]`.
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
//! documentation describes. Every response field here is an `Option`,
//! nothing is indexed, and there is no `unwrap`, `expect` or `panic!` on any
//! value that arrived over the wire. The read is length-limited for the same
//! reason.

use std::sync::OnceLock;
use std::time::Duration;

use sayd_core::config::{resolve_api_key, RewordConfig};
use sayd_core::reword::{chat_completions_url, parse_base_url};
use serde::{Deserialize, Serialize};

use super::{RewordError, Rewriter, REWORD_HTTP_CEILING};

/// §3's prompt, verbatim. One request, one system prompt, one user message
/// containing the text. No history, no tools, no schema.
const SYSTEM_PROMPT: &str = "\
You rewrite short desktop notifications so a speech synthesiser can
read them aloud. Notifications are written to be read at a glance, so
they are terse and often not sentences.

Rules:
- Reply with the rewritten text and nothing else: no preamble, no quotes, no
  explanation, no markdown.
- Keep every fact. Names, numbers, times and places stay exactly as written.
  Add nothing.
- Turn labels into sentences. \"Alice: where do you want to go for dinner\"
  becomes \"Alice is asking where you want to go for dinner\".
- One or two sentences at most, and no longer than the original needs.
- Do not expand abbreviations, identifiers or names you are unsure of.
- If the text is already a natural spoken sentence, is not English, or you
  cannot improve it, reply with it unchanged.";

/// `f64` rather than `f32`, because this is serialised into JSON and JSON
/// numbers are `f64`. As an `f32` the literal `0.2` widens to
/// `0.20000000298023224`, and that is what goes out on the wire -- harmless
/// to a model and needless noise in a request a user may well be reading in
/// a proxy log.
const TEMPERATURE: f64 = 0.2;

/// Generous on purpose, against a strict character ceiling in the guard. A
/// tight token limit truncates mid-sentence, and a truncated sentence passes
/// a length check and gets *spoken*; a generous one means an over-long
/// answer arrives complete and is rejected whole.
const MAX_TOKENS: u32 = 256;

/// How much of a response body is read before the read fails.
///
/// The body is untrusted, so an unbounded read is a memory bug: a server
/// that streams gigabytes must not be able to grow this process. 64 KiB is
/// far more than [`MAX_TOKENS`] of UTF-8 plus the envelope, and far more
/// than any `error.message` worth reading.
const BODY_LIMIT: u64 = 64 * 1024;

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [Message<'a>; 2],
    stream: bool,
    temperature: f64,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

/// Every field an `Option`: this is untrusted input off a socket, whoever is
/// running the server.
///
/// `Option` rather than the bare type with `#[serde(default)]`, which is the
/// obvious spelling and the wrong one -- `default` covers a *missing* field
/// but not an explicit `null`, and `"content": null` is exactly what a local
/// server returns when generation produced nothing. With the bare type one
/// `null` anywhere fails the whole parse, and the `error.message` that would
/// have said *why* goes with it.
#[derive(Deserialize, Default)]
struct ChatResponse {
    #[serde(default)]
    choices: Option<Vec<Choice>>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<ChoiceMessage>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
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
pub fn build_request(cfg: &RewordConfig, key: Option<&str>, text: &str) -> Request {
    let request = ChatRequest {
        model: &cfg.model,
        messages: [
            Message {
                role: "system",
                content: SYSTEM_PROMPT,
            },
            Message {
                role: "user",
                content: text,
            },
        ],
        stream: false,
        temperature: TEMPERATURE,
        max_tokens: MAX_TOKENS,
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
/// A message is used for its wording wherever one is present.
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
        .filter(|m| !m.trim().is_empty());

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
    if let Some(message) = message {
        return Err(RewordError::Malformed(message));
    }
    if status >= 400 {
        return Err(RewordError::Malformed(format!("HTTP {status}")));
    }
    // No indexing anywhere: an empty `choices`, a `choices` that is not an
    // array, a choice with no `message` and a `content` that is `null` all
    // arrive here as `None`.
    let content = parsed
        .choices
        .unwrap_or_default()
        .into_iter()
        .next()
        .and_then(|c| c.message)
        .and_then(|m| m.content)
        .filter(|c| !c.trim().is_empty());
    content.ok_or_else(|| {
        RewordError::Malformed("the response carried no choices[0].message.content".to_string())
    })
}

/// The agent, built once and cached.
///
/// An `Agent` owns a connection pool, and against a 1.5 s budget a fresh DNS
/// lookup plus a TLS handshake is most of the budget. It outlives config
/// changes: `base_url`, `model` and the key are per-request inputs, not
/// client state, so only a change to [`REWORD_HTTP_CEILING`] would require
/// rebuilding it -- and that is a constant, which is why a `OnceLock` with
/// no way to replace its value is the right shape.
///
/// Nothing is pre-warmed at startup, because that would be a network call
/// the user did not ask for. The **first** rewrite of a run is therefore
/// expected to miss the deadline and speak the original. That is the
/// fallback working, not a bug, and the settings window's Test row says so
/// on screen.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = ureq::Agent::config_builder()
            // The only thing that bounds the blocking thread.
            // `tokio::time::timeout` cannot -- see the module doc one level
            // up.
            .timeout_global(Some(REWORD_HTTP_CEILING))
            // Load-bearing. Left on (the default), a 4xx becomes an `Err`
            // and the body is discarded -- and the body is where
            // `error.message` lives.
            .http_status_as_error(false)
            .build();
        ureq::Agent::new_with_config(config)
    })
}

pub struct HttpRewriter {
    cfg: RewordConfig,
    key: Option<String>,
    host: String,
}

impl HttpRewriter {
    /// Refuses an unusable `base_url` here rather than at request time, so
    /// the daemon logs it once at first use instead of once per utterance.
    pub fn new(cfg: &RewordConfig) -> Result<HttpRewriter, RewordError> {
        let endpoint = parse_base_url(&cfg.base_url)
            .map_err(|e| RewordError::NotConfigured(format!("reword.base_url: {e}")))?;
        Ok(HttpRewriter {
            cfg: cfg.clone(),
            key: resolve_api_key(cfg),
            host: endpoint.host,
        })
    }
}

impl Rewriter for HttpRewriter {
    fn reword(&self, text: &str) -> Result<String, RewordError> {
        let request = build_request(&self.cfg, self.key.as_deref(), text);
        let mut call = agent()
            .post(&request.url)
            .header("content-type", "application/json");
        if let Some(auth) = &request.authorization {
            call = call.header("authorization", auth);
        }
        let mut response = match call.send_json(&request.body) {
            Ok(r) => r,
            Err(ureq::Error::Timeout(_)) => return Err(RewordError::Ceiling),
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
            Err(e) => return Err(RewordError::Unreachable(e.to_string())),
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
            Err(e) => return Err(RewordError::Unreachable(e.to_string())),
        };
        parse_response(
            status,
            retry_after.as_deref(),
            &body,
            &self.cfg.model,
            &self.host,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn cfg() -> RewordConfig {
        RewordConfig {
            enabled: true,
            base_url: "https://api.ppq.ai/v1".into(),
            model: "gpt-4o-mini".into(),
            ..RewordConfig::default()
        }
    }

    /// The URL for every row of §6's endpoint table, the trailing-slash
    /// case included, and the header that must *not* be sent to a local
    /// server that does not want one.
    #[test]
    fn a_request_names_one_url_and_omits_an_absent_key() {
        let mut c = cfg();
        c.base_url = "http://localhost:11434/v1/".into();
        let r = build_request(&c, None, "Alice: dinner?");
        assert_eq!(r.url, "http://localhost:11434/v1/chat/completions");
        assert_eq!(
            r.authorization, None,
            "no key means no Authorization header at all -- which is exactly \
             right for a local server"
        );

        let r = build_request(&cfg(), Some("sk-abc"), "Alice: dinner?");
        assert_eq!(r.url, "https://api.ppq.ai/v1/chat/completions");
        assert_eq!(r.authorization.as_deref(), Some("Bearer sk-abc"));
    }

    /// The body: both messages in order, `stream: false`, and the two
    /// generation parameters §3 fixes.
    #[test]
    fn the_body_carries_both_messages_in_order_and_never_streams() {
        let r = build_request(&cfg(), None, "Alice: where do you want to go for dinner");
        assert_eq!(r.body["model"], "gpt-4o-mini");
        assert_eq!(r.body["stream"], false);
        assert_eq!(r.body["max_tokens"], 256);
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
            rewriter.reword("Alice: dinner?").as_deref(),
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
    #[test]
    fn a_base_url_ureq_will_not_parse_is_a_configuration_failure() {
        let cfg = RewordConfig {
            base_url: "http://exa mple.com/v1".into(),
            api_key_env: String::new(),
            ..RewordConfig::default()
        };
        let rewriter = HttpRewriter::new(&cfg).expect("the scheme and host are readable");
        assert!(
            matches!(
                rewriter.reword("Alice: dinner?"),
                Err(RewordError::NotConfigured(_))
            ),
            "a URL that cannot be requested is not a provider that is down"
        );

        // The same row for the other half of a request a user types: a key
        // pasted with a newline in it is not a header value.
        let cfg = RewordConfig {
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "sk-abc\ndef".into(),
            api_key_env: String::new(),
            ..RewordConfig::default()
        };
        let rewriter = HttpRewriter::new(&cfg).expect("the base_url is fine");
        assert!(
            matches!(
                rewriter.reword("Alice: dinner?"),
                Err(RewordError::NotConfigured(_))
            ),
            "a key that cannot be a header value is a configuration failure"
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
                rewriter.reword("Alice: dinner?"),
                Err(RewordError::Unreachable(_))
            ),
            "a closed connection is unreachable, not malformed"
        );
        server.join().expect("the server thread");
    }

    /// A closed port -- nothing listening at all. The most common first-run
    /// failure there is: a `base_url` pointing at a server that is not
    /// running. The detail it carries is what a user pastes into an issue,
    /// so it is worth asserting that there is one. Recorded here rather
    /// than asserted verbatim, because it is `ureq`'s wording and not
    /// ours: `ureq` 3.4 produces `io: Connection refused`, which reaches
    /// the journal as `warning: reword: could not reach the provider: io:
    /// Connection refused` -- one line after the `info: reword: sending
    /// text to …` line that names the endpoint it could not reach.
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
            ..RewordConfig::default()
        };
        let rewriter = HttpRewriter::new(&cfg).expect("a usable client");
        match rewriter.reword("Alice: dinner?") {
            Err(RewordError::Unreachable(detail)) => assert!(
                !detail.trim().is_empty(),
                "the one line a user will paste into an issue must say something"
            ),
            other => panic!("a closed port is unreachable, got {other:?}"),
        }
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
            rewriter.reword("Alice: dinner?"),
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
            rewriter.reword("Alice: dinner?"),
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
                rewriter.reword("Alice: dinner?"),
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
            rewriter.reword("Alice: dinner?"),
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
            rewriter.reword("Alice: dinner?"),
            Err(RewordError::Malformed(_))
        ));
        server.join().expect("the server thread");
    }
}
