//! The `ureq` client. Filled in by the next task.
//!
//! What stands here now is a placeholder with the right name and the right
//! signature and no request in it, because [`super::build_rewriter`] under
//! `--features reword` names it: without this file the feature build does
//! not compile at all, and a task whose commit is green in only one of the
//! two build configurations cannot be reviewed in the other. The next task
//! replaces this file wholesale -- the request, the response parsing, the
//! `REWORD_HTTP_CEILING` on the agent, and the classification of every
//! [`RewordError`] variant the breakers in `super` already know how to fold
//! in.

use sayd_core::config::RewordConfig;

use super::{RewordError, Rewriter};

/// A rewriter that speaks HTTP to an OpenAI-compatible endpoint.
pub struct HttpRewriter;

impl HttpRewriter {
    /// Refuses to be built, so a build with the feature on and the client
    /// not yet written behaves exactly like a build with the feature off:
    /// the text is spoken as written. This is the whole of the placeholder.
    pub fn new(_cfg: &RewordConfig) -> Result<HttpRewriter, RewordError> {
        Err(RewordError::Unavailable)
    }
}

impl Rewriter for HttpRewriter {
    fn reword(&self, _text: &str) -> Result<String, RewordError> {
        Err(RewordError::Unavailable)
    }
}
