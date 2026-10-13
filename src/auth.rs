//! Credential storage backed by the platform keychain.
//!
//! `keyring` maps to macOS Keychain Services, Windows Credential Manager and the
//! Linux Secret Service. Keys are stored
//! under service `termplete`, account = provider name, so a user can inspect and
//! revoke them with the native tools (`security`, `cmdkey`, `secret-tool`).
//!
//! Resolution order for a provider's key:
//!   1. an explicit key argument (used by `tp suggest --api-key`),
//!   2. the keychain entry,
//!   3. the provider's `env_key` environment variable,
//!   4. nothing — providers flagged `no_auth` proceed without a credential.

use keyring::Entry;

use crate::config::{Config, ConfigError, ProviderPreset, Wire};
use crate::{Error, Result};

/// Keychain service name. Also the account prefix for per-provider entries.
pub const SERVICE: &str = crate::APP_NAME;

fn entry(provider: &str) -> Result<Entry> {
    Entry::new(SERVICE, provider).map_err(|e| Error::Auth(format!("keychain unavailable: {e}")))
}

/// Store `key` for `provider`, replacing any existing entry.
pub fn store(provider: &str, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err(Error::Auth("refusing to store an empty API key".into()));
    }
    entry(provider)?
        .set_password(key)
        .map_err(|e| Error::Auth(format!("could not write key for `{provider}`: {e}")))
}

/// Read the stored key for `provider`, if any. Missing entries are `Ok(None)`
/// rather than an error so callers can fall through to the environment.
pub fn load(provider: &str) -> Result<Option<String>> {
    match entry(provider)?.get_password() {
        Ok(key) if !key.is_empty() => Ok(Some(key)),
        Ok(_) => Ok(None),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(Error::Auth(format!(
            "could not read key for `{provider}`: {e}"
        ))),
    }
}

pub fn delete(provider: &str) -> Result<()> {
    match entry(provider)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Auth(format!(
            "could not delete key for `{provider}`: {e}"
        ))),
    }
}

/// Stop a live daemon after a credential change so it cannot keep using a key
/// cached at startup. It will be started again by the next request when
/// auto-start is enabled.
pub fn stop_daemon_if_running() -> Result<bool> {
    fn is_live() -> bool {
        std::os::unix::net::UnixStream::connect(crate::daemon::socket_display()).is_ok()
    }

    if !is_live() {
        return Ok(false);
    }
    let _ = crate::client::ask(
        crate::protocol::Request::Shutdown { id: 1 },
        std::time::Duration::from_millis(500),
        false,
    );
    for _ in 0..40 {
        if !is_live() {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Err(Error::Auth(
        "the running daemon did not stop after the key change; stop it manually to discard its cached credential".into(),
    ))
}

/// True when a key is present in the keychain for `provider`.
pub fn is_logged_in(provider: &str) -> bool {
    matches!(load(provider), Ok(Some(_)))
}

/// Whether credentials are managed by an external ACP provider instead of the
/// termplete keychain or environment.
pub fn uses_external_auth(preset: &ProviderPreset) -> bool {
    preset.wire == Wire::OpenAiAcp
}

/// Where a provider's key will come from, for `tp auth status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    Keychain,
    Environment(String),
    ExplicitArgument,
    CodexAcp,
    None,
}

impl std::fmt::Display for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keychain => write!(f, "keychain"),
            Self::Environment(v) => write!(f, "env:{v}"),
            Self::ExplicitArgument => write!(f, "argument"),
            Self::CodexAcp => write!(f, "codex-acp"),
            Self::None => write!(f, "none"),
        }
    }
}

/// Resolve the credential for `preset` without touching the network.
pub fn resolve(preset: &ProviderPreset) -> Result<Option<String>> {
    if preset.no_auth || uses_external_auth(preset) {
        return Ok(None);
    }
    let environment = preset
        .env_key
        .as_deref()
        .and_then(|var| std::env::var(var).ok())
        .filter(|v| !v.trim().is_empty());
    resolve_sources(load(&preset.name), environment).map_err(|error| {
        let fallback = preset.env_key.as_deref().map_or(String::new(), |var| {
            format!(" or set the {var} environment variable")
        });
        Error::Auth(format!(
            "{error}; unlock your credential store and run `{} auth login --provider {}`{fallback}",
            crate::BIN_NAME,
            preset.name
        ))
    })
}

fn resolve_sources(
    stored: Result<Option<String>>,
    environment: Option<String>,
) -> Result<Option<String>> {
    match stored {
        Ok(Some(key)) => Ok(Some(key)),
        Ok(None) => Ok(environment),
        Err(_) if environment.is_some() => Ok(environment),
        Err(error) => Err(error),
    }
}

/// Report where the credential comes from, for `tp auth status`.
pub fn source(preset: &ProviderPreset) -> Result<KeySource> {
    if uses_external_auth(preset) {
        return Ok(KeySource::CodexAcp);
    }
    if preset.no_auth {
        return Ok(KeySource::None);
    }
    if is_logged_in(&preset.name) {
        return Ok(KeySource::Keychain);
    }
    if let Some(var) = &preset.env_key {
        if std::env::var(var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(KeySource::Environment(var.clone()));
        }
    }
    if preset.no_auth {
        return Ok(KeySource::None);
    }
    Ok(KeySource::None)
}

/// Resolve a credential or explain what the user must do.
///
/// `explicit` is used by the `--api-key` escape hatch; it never reaches disk.
pub fn require(preset: &ProviderPreset, explicit: Option<&str>) -> Result<Option<String>> {
    if uses_external_auth(preset) {
        if explicit.is_some_and(|key| !key.trim().is_empty()) {
            return Err(Error::Auth(
                "OpenAI ACP uses ChatGPT authentication managed by Codex; API keys are not accepted".into(),
            ));
        }
        return Ok(None);
    }
    if let Some(k) = explicit.map(str::trim).filter(|k| !k.is_empty()) {
        return Ok(Some(k.to_string()));
    }
    let resolved = resolve(preset)?;
    if resolved.is_none() && !preset.no_auth {
        return Err(Error::Auth(format!(
            "no credential for provider `{}` — run `{} auth login --provider {}`",
            preset.name,
            crate::BIN_NAME,
            preset.name
        )));
    }
    Ok(resolved)
}

/// Persist the credential for `preset`, honoring the `no_auth` opt-out.
pub fn login(preset: &ProviderPreset, key: &str, force: bool) -> Result<()> {
    if uses_external_auth(preset) {
        return Err(Error::Auth(
            "OpenAI ACP uses ChatGPT authentication; run `tp auth login --provider openai-acp`"
                .into(),
        ));
    }
    if preset.no_auth {
        return Err(Error::Auth(format!(
            "provider `{}` is local and needs no API key; just set `provider = \"{}\"`",
            preset.name, preset.name
        )));
    }
    if !force && is_logged_in(&preset.name) {
        return Err(Error::Auth(format!(
            "provider `{}` already has a key stored; pass --force to replace it",
            preset.name
        )));
    }
    store(&preset.name, key)
}

/// Mask a key for display: keep a short prefix and suffix.
pub fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n <= 10 {
        return "*".repeat(n);
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// Write the active provider into the config file when one is given.
pub fn set_active_provider(cfg: &mut Config, name: &str) -> Result<(), ConfigError> {
    cfg.select_provider(name);
    cfg.save()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset() -> ProviderPreset {
        crate::config::builtin_providers()
            .into_iter()
            .find(|p| p.name == "openai")
            .unwrap()
    }

    #[test]
    fn mask_hides_middle() {
        assert_eq!(mask("sk-abcdefghijklmnop"), "sk-a…mnop");
    }

    #[test]
    fn mask_short_keys_fully() {
        assert_eq!(mask("short"), "*****");
    }

    #[test]
    fn empty_key_is_rejected() {
        let err = store("openai", "   ").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn no_auth_provider_requires_nothing() {
        let ollama = crate::config::builtin_providers()
            .into_iter()
            .find(|p| p.name == "ollama")
            .unwrap();
        assert_eq!(require(&ollama, None).unwrap(), None);
        assert!(login(&ollama, "x", false).is_err());
    }

    #[test]
    fn explicit_argument_wins_and_never_touches_disk() {
        let key = require(&preset(), Some("sk-explicit")).unwrap();
        assert_eq!(key.as_deref(), Some("sk-explicit"));
    }

    #[test]
    fn missing_key_names_the_fix() {
        let mut p = preset();
        p.name = "termplete-test-absent-provider".into();
        p.env_key = Some("TERMPLETE_TEST_ABSENT_KEY".into());
        let err = require(&p, None).unwrap_err();
        assert!(err.to_string().contains("auth login"), "{err}");
    }

    #[test]
    fn env_var_satisfies_requirement() {
        let mut p = preset();
        p.name = "termplete-test-env-provider".into();
        p.env_key = Some("TERMPLETE_TEST_KEY".into());
        std::env::set_var("TERMPLETE_TEST_KEY", "from-env");
        assert_eq!(require(&p, None).unwrap().as_deref(), Some("from-env"));
        std::env::remove_var("TERMPLETE_TEST_KEY");
    }

    #[test]
    fn unavailable_keychain_uses_environment_without_losing_stored_key_priority() {
        assert_eq!(
            resolve_sources(
                Err(Error::Auth("unavailable".into())),
                Some("from-env".into())
            )
            .unwrap(),
            Some("from-env".into())
        );
        assert_eq!(
            resolve_sources(Ok(Some("stored".into())), Some("from-env".into())).unwrap(),
            Some("stored".into())
        );
        assert!(resolve_sources(Err(Error::Auth("unavailable".into())), None).is_err());
    }

    #[test]
    fn deleting_absent_entry_is_ok() {
        let _ = delete("termplete-test-never-set");
    }
}
