//! Tractus console: the Rust control plane that serves the firewall dashboard,
//! extracts Intent Contracts with GPT-5.6, and bridges the `tractusd` event
//! stream to the browser. Replaces the former Python/FastAPI `control/` service.

pub mod daemon;
pub mod explain;
pub mod intent;
pub mod server;
pub mod terminal;

use std::error::Error;
use std::fmt;

/// Failure modes for the GPT-5.6 intent-extraction path.
#[derive(Debug)]
pub enum ConsoleError {
    /// `OPENAI_API_KEY` is unset or empty.
    NoCredentials,
    /// The HTTP request to the Responses API failed to complete.
    Upstream(reqwest::Error),
    /// The Responses API returned a non-success status.
    UpstreamStatus(u16),
    /// The completion contained no structured contract to parse.
    EmptyCompletion,
}

impl fmt::Display for ConsoleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCredentials => write!(formatter, "OPENAI_API_KEY is not set"),
            Self::Upstream(error) => write!(formatter, "intent request failed: {error}"),
            Self::UpstreamStatus(status) => {
                write!(formatter, "intent model returned status {status}")
            }
            Self::EmptyCompletion => write!(formatter, "intent model returned no contract"),
        }
    }
}

impl Error for ConsoleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Upstream(error) => Some(error),
            _ => None,
        }
    }
}
