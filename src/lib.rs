//! termplete — LLM-powered terminal autocomplete.
//!
//! Architecture:
//!
//! - [`config`] — on-disk TOML configuration, the provider preset table and the
//!   user-configurable keybindings.
//! - [`auth`] — API key storage backed by the platform keychain (macOS Keychain,
//!   Windows Credential Manager, Linux Secret Service / kernel keyring).
//! - [`provider`] — OpenAI-compatible chat-completions client, plus a native
//!   Anthropic Messages path so Claude works without a proxy.
//! - [`context`] — rolling shell history (commands *and* captured output) plus
//!   the heuristics deciding whether an input is worth a model round-trip.
//! - [`fs_context`] — bounded local directory completion and prompt context.
//! - [`suggest`] — history matching, prompt assembly, and the sanitizing that
//!   turns a model's prose into literal ghost text.
//! - [`daemon`] — a background process owning the HTTP client and the shared
//!   context store, spoken to over a newline-delimited JSON unix socket.
//! - [`client`] — the shell-side socket client the plugins call per keystroke.
//! - [`anim`] — the animation layer (ghost shimmer, inline spinner), gated off by
//!   `NO_COLOR`, a dumb terminal, or `animations = false`.
//! - [`pty`] — the `tp shell` wrapper that runs a real shell in a PTY and
//!   captures command output, so context is always fresh.
//! - [`redact`] — secret scrubbing applied before anything is stored or sent.
//! - [`setup`] — the interactive `tp setup` wizard, so no provider name ever
//!   has to be memorised or typed.
//! - [`settings`] — the `tp settings` menu for mode, keys and behaviour.
//!
//! The shell integrations in `shells/` are deliberately thin: they capture the
//! current buffer, ship it to the daemon, and render the ghost text that comes
//! back. All decision-making lives here, so both shells behave identically.

pub mod acp;
pub mod anim;
pub mod auth;
pub mod client;
pub mod config;
pub mod context;
pub mod daemon;
pub mod decision;
pub mod decision_runtime;
pub mod fs_context;
mod local_reasoning;
pub mod memory;
pub mod menu;
pub mod privacy;
pub mod protocol;
pub mod provider;
pub mod pty;
pub mod redact;
pub mod settings;
pub mod setup;
pub mod shellutil;
pub mod suggest;
pub mod terminal;

/// Name used for the config directory and keychain service.
pub const APP_NAME: &str = "termplete";
/// The short binary users invoke (`tp`).
pub const BIN_NAME: &str = "tp";

/// Errors surfaced to the user.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(#[from] config::ConfigError),
    #[error("credential store error: {0}")]
    Auth(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("no provider configured — run `{BIN_NAME} auth login --provider <name>`")]
    NoProvider,
    #[error("daemon error: {0}")]
    Daemon(String),
    #[error("context error: {0}")]
    Context(#[from] context::ContextError),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The error type callers get by default. Override with `Result<T, MyErr>` when a
/// call site needs a narrower error.
pub type Result<T, E = Error> = std::result::Result<T, E>;
