//! On-disk configuration and the built-in provider preset table.
//!
//! Config, logs, and history live in `~/.termplete` by default. Explicit
//! `TERMPLETE_HOME` and `XDG_CONFIG_HOME` overrides are honored.
//! Credentials never live here — see [`crate::auth`].
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// Whether remembered command evidence is shared across folders or scoped to
/// the current project folder with global history as a fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SuggestionScope {
    Global,
    #[default]
    Folder,
}

impl SuggestionScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Folder => "folder",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "global" => Some(Self::Global),
            "folder" | "project" => Some(Self::Folder),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not determine config directory")]
    NoConfigDir,
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("could not serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("unknown provider `{0}` — run `{1} auth providers` to list known providers")]
    UnknownProvider(String, &'static str),
    #[error("provider `{name}` is already configured; use `auth login --provider {name} --force` to replace the stored key")]
    AlreadyLoggedIn { name: String },
    #[error("no API key configured for provider `{0}`")]
    MissingKey(String),
    #[error("no model configured for `{0}` — run `tp setup`, or `tp config set --key model --value <id>`")]
    NoModel(String),
    #[error("decision timeout must be between 100 and 10000 ms, got {0}")]
    InvalidDecisionTimeout(u64),
    #[error("decision endpoint must be an HTTP(S) URL without credentials, query, or fragment")]
    InvalidDecisionEndpoint,
    #[error("remote decision endpoint is disabled while local_only is enabled")]
    RemoteDecisionDenied,
    #[error("no model configured for decision provider `{0}` — set decision.model before using the judge")]
    NoDecisionModel(String),
    #[error("provider `{0}` uses ACP and cannot be used as a decision provider")]
    UnsupportedDecisionProvider(String),
}

/// Wire protocol spoken to providers. OpenAI-compatible gateways share one wire;
/// providers with distinct payloads or endpoints get a dedicated variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Wire {
    #[serde(rename = "openai-compat", alias = "open-ai-compat")]
    OpenAiCompat,
    #[serde(rename = "openai-acp", alias = "open-ai-acp")]
    OpenAiAcp,
    Anthropic,
    Ollama,
    /// TypeSafe/JEV's native systemone protocol.
    Jev,
}

impl Wire {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompat => "openai-compat",
            Self::OpenAiAcp => "openai-acp",
            Self::Anthropic => "anthropic",
            Self::Ollama => "ollama",
            Self::Jev => "jev",
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let normalized = host.to_ascii_lowercase();
    normalized == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A built-in provider preset. Users can add their own via
/// `[[providers]]` in the config file, which is how every other OpenAI-compatible
/// gateway gets supported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPreset {
    /// Identifier used as `--provider` and, for API-key providers, the keyring account.
    pub name: String,
    /// Base URL without a trailing slash. Empty means "derive from wire".
    #[serde(default)]
    pub base_url: String,
    /// Default model used when a provider does not specify one in config.
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_wire")]
    pub wire: Wire,
    /// Env var consulted when no key is stored in the keychain.
    #[serde(default)]
    pub env_key: Option<String>,
    /// Human-readable blurb for `tp auth providers`.
    #[serde(default)]
    pub description: String,
    /// Key is stored in the keychain but never sent as a bearer token — local
    /// runtimes usually need no credential at all.
    #[serde(default)]
    pub no_auth: bool,
}

impl ProviderPreset {
    /// Human-facing provider label. The wire/provider id remains stable for
    /// configuration and CLI use.
    pub fn display_name(&self) -> &str {
        if self.wire == Wire::OpenAiAcp {
            "ChatGPT subscription"
        } else {
            &self.name
        }
    }
}

fn default_wire() -> Wire {
    Wire::OpenAiCompat
}

/// The preset table. Ordered so `tp auth providers` output is stable.
pub fn builtin_providers() -> Vec<ProviderPreset> {
    vec![
        preset(
            "openai",
            "https://api.openai.com/v1",
            "",
            Wire::OpenAiCompat,
            "OpenAI",
        ),
        preset_full(
            "openai-acp",
            "",
            "",
            Wire::OpenAiAcp,
            "ChatGPT subscription via OpenAI Codex",
            None,
            false,
        ),
        preset(
            "anthropic",
            "https://api.anthropic.com/v1",
            "",
            Wire::Anthropic,
            "Anthropic Claude",
        ),
        preset(
            "openrouter",
            "https://openrouter.ai/api/v1",
            "",
            Wire::OpenAiCompat,
            "OpenRouter aggregator",
        ),
        preset(
            "groq",
            "https://api.groq.com/openai/v1",
            "",
            Wire::OpenAiCompat,
            "Groq",
        ),
        preset(
            "together",
            "https://api.together.xyz/v1",
            "",
            Wire::OpenAiCompat,
            "Together AI",
        ),
        preset(
            "deepseek",
            "https://api.deepseek.com/v1",
            "",
            Wire::OpenAiCompat,
            "DeepSeek",
        ),
        preset(
            "mistral",
            "https://api.mistral.ai/v1",
            "",
            Wire::OpenAiCompat,
            "Mistral",
        ),
        preset(
            "gemini",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "",
            Wire::OpenAiCompat,
            "Google Gemini (OpenAI-compat)",
        ),
        preset(
            "xai",
            "https://api.x.ai/v1",
            "",
            Wire::OpenAiCompat,
            "xAI Grok",
        ),
        preset(
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
            "",
            Wire::OpenAiCompat,
            "Fireworks AI",
        ),
        preset_full(
            "lmstudio",
            "http://127.0.0.1:1234/v1",
            "",
            Wire::OpenAiCompat,
            "LM Studio (local, no API key)",
            None,
            true,
        ),
        preset_full(
            "ollama",
            "http://127.0.0.1:11434/v1",
            "",
            Wire::Ollama,
            "Ollama (local, no API key)",
            None,
            true,
        ),
        preset_full(
            "jev",
            "https://api.typesafe.ai/v1",
            "jev-latest",
            Wire::Jev,
            "TypeSafe JEV decision service",
            Some("TYPESAFE_API_KEY"),
            false,
        ),
    ]
}

fn preset(
    name: &str,
    base_url: &str,
    model: &str,
    wire: Wire,
    description: &str,
) -> ProviderPreset {
    preset_full(
        name,
        base_url,
        model,
        wire,
        description,
        Some(&format!("{}_API_KEY", name.to_ascii_uppercase())),
        false,
    )
}

fn preset_full(
    name: &str,
    base_url: &str,
    model: &str,
    wire: Wire,
    description: &str,
    env_key: Option<&str>,
    no_auth: bool,
) -> ProviderPreset {
    ProviderPreset {
        name: name.into(),
        base_url: base_url.into(),
        model: model.into(),
        wire,
        env_key: env_key.map(str::to_string),
        description: description.into(),
        no_auth,
    }
}

/// Suggestion behavior, mirroring Zed's model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Legacy configuration support: local history only, never network.
    Off,
    /// Fetch quietly; reveal a suggestion before accepting it.
    #[serde(rename = "on-demand", alias = "subtle", alias = "normal")]
    Subtle,
    /// Fetch suggestions as the user types, even before the line looks command-like.
    #[default]
    #[serde(rename = "automatic", alias = "aggressive", alias = "eager")]
    Aggressive,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Subtle => "on-demand",
            Self::Aggressive => "automatic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Subtle => "On demand",
            Self::Aggressive => "Automatic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => Some(Self::Off),
            "on-demand" | "on_demand" | "subtle" | "normal" => Some(Self::Subtle),
            "automatic" | "aggressive" | "eager" => Some(Self::Aggressive),
            _ => None,
        }
    }
}

/// Per-mode tuning knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Modes {
    /// Minimum characters before any suggestion is produced at all.
    pub min_chars: usize,
    /// Character classes that make a line "command-like" enough to warrant the
    /// LLM in on-demand mode.
    pub subtle_trigger_chars: Vec<char>,
    /// Debounce before an LLM request is dispatched, per mode, in milliseconds.
    #[serde(rename = "on_demand_debounce_ms", alias = "subtle_debounce_ms")]
    pub subtle_debounce_ms: u64,
    #[serde(
        rename = "automatic_debounce_ms",
        alias = "aggressive_debounce_ms",
        alias = "eager_debounce_ms"
    )]
    pub eager_debounce_ms: u64,
    /// Hard ceiling on characters sent as "the command being typed".
    pub max_input_chars: usize,
    /// Skip the LLM entirely for these completions; they are cheap and exact.
    pub skip_for_common_commands: bool,
    /// Maximum suggestions returned to the shell.
    pub max_suggestions: usize,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            min_chars: 2,
            subtle_trigger_chars: vec![' ', '-', '/', '.', '_', '=', ':', '@'],
            subtle_debounce_ms: 220,
            eager_debounce_ms: 400,
            max_input_chars: 2000,
            skip_for_common_commands: true,
            max_suggestions: 3,
        }
    }
}

/// History and context-window tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Entries retained in memory and on disk.
    pub history_entries: usize,
    /// Distinct commands retained for instant, frequency-ranked completion.
    pub command_memory_entries: usize,
    /// Import existing zsh/bash history on daemon startup.
    pub import_shell_history: bool,
    /// Optional native history path; defaults to HISTFILE or the shell's home history.
    pub shell_history_file: Option<String>,
    /// Relevant historical options supplied to the model.
    pub history_candidates: usize,
    /// Ask the model to choose among exact matches instead of returning them instantly.
    pub ai_select_history: bool,
    /// Scope command usage and accepted choices to the current project folder.
    #[serde(default)]
    pub suggestion_scope: SuggestionScope,
    /// Observe interactive zsh output through a transparent PTY.
    pub capture_output: bool,
    /// Bytes of captured output kept per command.
    pub max_output_bytes: usize,
    /// Bytes of the most recent output embedded in a prompt.
    pub max_prompt_output_bytes: usize,
    /// File name inside the config dir.
    pub history_file: String,
    /// Commands matching these prefixes are never recorded (secrets in argv).
    pub ignore_prefixes: Vec<String>,
    /// Command output is only captured when it is at most this many bytes,
    /// to avoid hoisting a huge log into the prompt.
    pub ignore_output_over_bytes: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            history_entries: 200,
            command_memory_entries: 10_000,
            import_shell_history: true,
            shell_history_file: None,
            history_candidates: 8,
            ai_select_history: false,
            suggestion_scope: SuggestionScope::Folder,
            capture_output: true,
            max_output_bytes: 4096,
            max_prompt_output_bytes: 1024,
            history_file: "history.json".into(),
            ignore_prefixes: vec![
                "sudo ".into(),
                "passwd".into(),
                "ssh-keygen".into(),
                "security find-generic-password".into(),
            ],
            ignore_output_over_bytes: 256 * 1024,
        }
    }
}

/// User-configurable keybindings.
///
/// Keys are written the way zsh and bash write them, so `^G`, `^[f` and
/// `\e[Z` all mean what they normally mean. Every binding is overridable; the
/// defaults are chosen to avoid collisions with common readline emacs bindings.
///
/// Two-step acceptance in on-demand mode uses reveal then accept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyBindings {
    /// Reveal the pending suggestion (first press in subtle mode).
    pub reveal: String,
    /// Accept the revealed suggestion, inserting it without running it.
    pub accept: String,
    /// Cycle to the next candidate.
    pub cycle: String,
    /// Dismiss the current suggestion.
    pub dismiss: String,
    /// Toggle termplete on or off for this session.
    pub toggle: String,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            reveal: "^G".to_string(),
            accept: "^[[C".to_string(),
            cycle: "^[[Z".to_string(),
            dismiss: "^C".to_string(),
            toggle: "^X^X".to_string(),
        }
    }
}

impl KeyBindings {
    pub fn migrate_legacy_accept(&mut self) {
        if Self::normalize(&self.accept) == "^I" {
            self.accept = "^[[C".into();
        }
    }

    /// Normalize a user-supplied key so caret, readline, and readable spellings
    /// resolve to the same shell binding notation.
    pub fn normalize(spec: &str) -> String {
        let s = spec.trim();
        if s.is_empty() {
            return String::new();
        }
        if let Some(alias) = Self::friendly_alias(s) {
            return alias;
        }
        let chord_parts: Vec<_> = s.split_whitespace().collect();
        if chord_parts.len() > 1 && chord_parts.iter().any(|part| part.contains('+')) {
            return chord_parts
                .into_iter()
                .map(|part| Self::friendly_alias(part).unwrap_or_else(|| Self::normalize(part)))
                .collect();
        }
        // Translate the readline convention (`C-g`, `M-f`, `^[[Z`) into the
        // caret/bracket notation zsh's bindkey expects.
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(c) = rest.chars().next() {
            // A space between chord members (`C-x C-x`) is a separator, not part
            // of the key: bindkey reads consecutive keys, so it must be dropped.
            if c.is_whitespace() {
                rest = &rest[c.len_utf8()..];
                continue;
            }
            // Check the prefix at the head of `rest`, not after `c`: a spec like
            // `C-g` begins with the prefix itself.
            let is_ctrl = rest.starts_with("C-") || rest.starts_with("c-");
            let is_meta = rest.starts_with("M-") || rest.starts_with("m-");
            if (is_ctrl || is_meta) && rest.len() > 2 {
                let tail = &rest[2..];
                let raw = tail.chars().next().unwrap();
                // Ctrl keys are conventionally written uppercase (`C-g` -> `^G`);
                // meta keys are not (`M-f` -> `^[f`), so only the ctrl form is
                // upper-cased.
                let key = if is_ctrl {
                    raw.to_ascii_uppercase()
                } else {
                    raw
                };
                out.push_str(if is_ctrl { "^" } else { "^[" });
                out.push(key);
                rest = &tail[key.len_utf8()..];
                continue;
            }
            let tail = &rest[c.len_utf8()..];
            if c == '^' {
                match tail.chars().next() {
                    // `^[` introduces a bracketed escape sequence such as `^[Z`.
                    Some('[') => {
                        out.push_str("^[");
                        rest = &tail[1..];
                    }
                    Some(_) => {
                        out.push('^');
                        rest = tail;
                    }
                    None => {
                        out.push('^');
                        rest = "";
                    }
                }
                continue;
            }
            out.push(c);
            rest = tail;
        }
        out
    }

    /// Convert common shortcut names such as `Right Arrow` and
    /// `Ctrl+Shift+Z` to the same notation used in config files and by shells.
    fn friendly_alias(spec: &str) -> Option<String> {
        let compact: String = spec
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        let alias = match compact.as_str() {
            "right" | "rightarrow" => "^[[C".to_string(),
            "left" | "leftarrow" => "^[[D".to_string(),
            "up" | "uparrow" => "^[[A".to_string(),
            "down" | "downarrow" => "^[[B".to_string(),
            "tab" => "^I".to_string(),
            "shift+tab" | "shifttab" => "^[[Z".to_string(),
            "escape" | "esc" => "^[".to_string(),
            "backspace" => "^?".to_string(),
            "home" => "^[[H".to_string(),
            "end" => "^[[F".to_string(),
            "insert" => "^[[2~".to_string(),
            "delete" => "^[[3~".to_string(),
            "pageup" => "^[[5~".to_string(),
            "pagedown" => "^[[6~".to_string(),
            "f1" => "^[OP".to_string(),
            "f2" => "^[OQ".to_string(),
            "f3" => "^[OR".to_string(),
            "f4" => "^[OS".to_string(),
            _ => return Self::normalize_chord_alias(spec),
        };
        Some(alias)
    }

    fn normalize_chord_alias(spec: &str) -> Option<String> {
        let parts: Vec<_> = spec.split('+').map(str::trim).collect();
        if !(2..=3).contains(&parts.len()) {
            return None;
        }
        let key = *parts.last()?;
        if key.chars().count() != 1 || !key.chars().next()?.is_ascii_alphabetic() {
            return None;
        }
        let mut control = false;
        let mut shift = false;
        let mut alt = false;
        for modifier in &parts[..parts.len() - 1] {
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => control = true,
                "shift" => shift = true,
                "alt" | "meta" => alt = true,
                _ => return None,
            }
        }
        if control && shift {
            let code = key.as_bytes()[0].to_ascii_uppercase();
            Some(format!("^[[27;6;{code}~"))
        } else if control {
            Some(format!("^{}", key.to_ascii_uppercase()))
        } else if alt {
            Some(format!("^[{}", key.chars().next()?.to_ascii_lowercase()))
        } else if shift {
            // Shifted printable keys are usually indistinguishable from their
            // resulting character; preserve the visible character.
            Some(key.to_ascii_uppercase())
        } else {
            None
        }
    }

    /// Human-readable form of a stored shell key sequence. This is for UI
    /// labels only; config getters continue returning the stored raw spelling.
    pub fn describe(spec: &str) -> String {
        let normalized = Self::normalize(spec);
        match normalized.as_str() {
            "^[[C" => return "Right Arrow".into(),
            "^[[D" => return "Left Arrow".into(),
            "^[[A" => return "Up Arrow".into(),
            "^[[B" => return "Down Arrow".into(),
            "^[[H" | "^[OH" | "^[[1~" => return "Home".into(),
            "^[[F" | "^[OF" | "^[[4~" => return "End".into(),
            "^[[2~" => return "Insert".into(),
            "^[[3~" => return "Delete".into(),
            "^[[5~" => return "Page Up".into(),
            "^[[6~" => return "Page Down".into(),
            "^[[Z" | "^[[1;2Z" | "^[[9;2u" | "^[[27;2;9~" => return "Shift+Tab".into(),
            "^[OP" => return "F1".into(),
            "^[OQ" => return "F2".into(),
            "^[OR" => return "F3".into(),
            "^[OS" => return "F4".into(),
            "^I" => return "Tab".into(),
            "^?" => return "Backspace".into(),
            "^[" => return "Escape".into(),
            _ => {}
        }
        if let Some((modifier, final_byte)) = parse_modified_arrow(&normalized) {
            let key_name = match final_byte {
                'A' => "Up Arrow",
                'B' => "Down Arrow",
                'C' => "Right Arrow",
                'D' => "Left Arrow",
                _ => "Arrow",
            };
            return match modifier_name(modifier) {
                Some(name) => format!("{name}+{key_name}"),
                None => "Custom key sequence".into(),
            };
        }
        if let Some((key, modifier)) = parse_csi_u(&normalized) {
            return describe_modified_character(key, modifier);
        }
        if let Some((modifier, key)) = parse_xterm_modified_character(&normalized) {
            return describe_modified_character(key, modifier);
        }
        match normalized.as_str() {
            "^[OA" => return "Up Arrow".into(),
            "^[OB" => return "Down Arrow".into(),
            "^[OC" => return "Right Arrow".into(),
            "^[OD" => return "Left Arrow".into(),
            _ => {}
        }
        if normalized.starts_with("^[[") || normalized.starts_with("^[O") {
            return "Custom key sequence".into();
        }
        if let Some(tail) = normalized.strip_prefix("^[") {
            if tail.len() == 1 {
                return format!("Alt+{}", tail.to_ascii_uppercase());
            }
            return "Custom key sequence".into();
        }
        if let Some(label) = describe_control_chord(&normalized) {
            return label;
        }
        if normalized.chars().any(char::is_control) {
            return "Custom key sequence".into();
        }
        normalized
    }
}

/// CSI `;modifier` number to human label. Used by arrow and character forms.
fn modifier_name(modifier: u8) -> Option<&'static str> {
    match modifier {
        2 => Some("Shift"),
        3 => Some("Alt"),
        4 => Some("Alt+Shift"),
        5 => Some("Ctrl"),
        6 => Some("Ctrl+Shift"),
        7 => Some("Ctrl+Alt"),
        8 => Some("Ctrl+Alt+Shift"),
        _ => None,
    }
}

fn describe_modified_character(key: u16, modifier: u8) -> String {
    if key == 9 && modifier == 2 {
        return "Shift+Tab".into();
    }
    let Some(modifier_name) = modifier_name(modifier) else {
        return "Custom key sequence".into();
    };
    let Some(ch) = char::from_u32(key as u32).filter(|ch| ch.is_ascii_graphic()) else {
        return "Custom key sequence".into();
    };
    format!("{modifier_name}+{}", ch.to_ascii_uppercase())
}

fn describe_control_chord(spec: &str) -> Option<String> {
    let bytes = spec.as_bytes();
    let mut labels = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'^' || index + 1 >= bytes.len() {
            return None;
        }
        let next = bytes[index + 1];
        if next == b'[' {
            // CSI and SS3 sequences are handled before this helper.
            if index + 2 < bytes.len() {
                let key = bytes[index + 2];
                if key.is_ascii_graphic() && bytes.get(index + 3).is_none() {
                    labels.push(format!("Alt+{}", (key as char).to_ascii_uppercase()));
                    index += 3;
                    continue;
                }
            }
            return None;
        }
        if next.is_ascii_graphic() {
            labels.push(format!("Ctrl+{}", (next as char).to_ascii_uppercase()));
            index += 2;
        } else {
            return None;
        }
    }
    (!labels.is_empty()).then(|| labels.join(" "))
}

fn parse_xterm_modified_character(spec: &str) -> Option<(u8, u16)> {
    let body = spec.strip_prefix("^[[27;")?.strip_suffix('~')?;
    let (modifier, key) = body.split_once(';')?;
    Some((modifier.parse().ok()?, key.parse().ok()?))
}

fn parse_modified_arrow(spec: &str) -> Option<(u8, char)> {
    let bytes = spec.as_bytes();
    if bytes.len() != 7 || !spec.starts_with("^[[1;") {
        return None;
    }
    let modifier = bytes[5].checked_sub(b'0')?;
    let final_byte = *bytes.last()? as char;
    if !matches!(final_byte, 'A' | 'B' | 'C' | 'D') {
        return None;
    }
    Some((modifier, final_byte))
}

fn parse_csi_u(spec: &str) -> Option<(u16, u8)> {
    let body = spec.strip_prefix("^[[")?.strip_suffix('u')?;
    let (key, modifier) = body.split_once(';')?;
    Some((key.parse().ok()?, modifier.parse().ok()?))
}

/// Rendering hints for the ghost text. The shell plugins read these via
/// `tp config get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleConfig {
    /// Suggestion colour as a hex string, e.g. `#6b7280`.
    pub ghost_color: String,
    /// Colour used in automatic mode.
    #[serde(
        rename = "automatic_color",
        alias = "aggressive_color",
        alias = "eager_color"
    )]
    pub eager_color: String,
    /// Show the inline hint key (default `end` / `tab` / `ctrl-f`).
    pub show_hint: bool,
    /// Delay in ms between showing and accepting a suggestion.
    pub fade_in_ms: u64,
    /// Master switch for the ghost shimmer and inline spinner.
    pub animations: bool,
    /// Frames per shimmer cycle. Lower is calmer, higher is livelier.
    pub animation_frames: usize,
    /// Master switch for the in-flight spinner shown while a request is pending.
    pub spinner: bool,
}

impl Default for StyleConfig {
    fn default() -> Self {
        Self {
            ghost_color: "#6b7280".into(),
            eager_color: "#8b5cf6".into(),
            show_hint: true,
            fade_in_ms: 60,
            animations: true,
            animation_frames: crate::anim::DEFAULT_FRAMES,
            spinner: true,
        }
    }
}

/// Daemon behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Auto-start a daemon on demand when a shell plugin asks for a suggestion.
    pub autostart: bool,
    /// Seconds to wait for the daemon to accept a connection.
    pub connect_timeout_ms: u64,
    /// Seconds a non-interactive suggestion may block the shell.
    pub request_timeout_secs: u64,
    /// Idle shutdown after this many seconds with no connected shell; 0 disables.
    pub idle_shutdown_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            autostart: true,
            connect_timeout_ms: 2500,
            request_timeout_secs: 10,
            idle_shutdown_secs: 1800,
        }
    }
}

/// How command decisions are made. `Llm` preserves the original completion flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionStrategy {
    #[default]
    Llm,
    Decision,
    Hybrid,
}

impl DecisionStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Decision => "decision",
            Self::Hybrid => "hybrid",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Llm => "Standard AI",
            Self::Decision => "Decision only",
            Self::Hybrid => "Decision + AI",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "llm" | "standard ai" => Some(Self::Llm),
            "decision" | "decision only" => Some(Self::Decision),
            "hybrid" | "decision + ai" | "decision and ai" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

/// Select how the chosen provider receives a decision request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionProtocol {
    /// Use the provider's normal wire protocol.
    #[default]
    Auto,
    /// Use the native JEV/System One decision endpoint.
    SystemOne,
    /// Use an OpenAI-compatible JSON request.
    Chat,
}

impl DecisionProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::SystemOne => "system-one",
            Self::Chat => "chat",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Provider default",
            Self::SystemOne => "System One / JEV",
            Self::Chat => "OpenAI-compatible JSON",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "provider default" => Some(Self::Auto),
            "system-one" | "system one" | "system one / jev" | "jev" => Some(Self::SystemOne),
            "chat" | "openai-compatible json" | "openai compatible json" => Some(Self::Chat),
            _ => None,
        }
    }
}

/// Settings for the optional command decision judge. The default keeps existing
/// completions unchanged and points the configured judge at local tev1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionConfig {
    /// Master switch; preserves the selected strategy and model while disabled.
    pub enabled: bool,
    pub strategy: DecisionStrategy,
    pub protocol: DecisionProtocol,
    /// Provider name from built-ins or `[[providers]]`.
    pub provider: String,
    pub model: String,
    /// Optional per-decision provider endpoint override.
    pub base_url: Option<String>,
    /// Maximum time spent waiting for the decision judge.
    pub timeout_ms: u64,
    /// Require the decision judge to use a loopback endpoint.
    pub local_only: bool,
    /// Path fragments kept out of suggestion and decision requests.
    pub private_paths: Vec<String>,
}

impl Default for DecisionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            strategy: DecisionStrategy::Llm,
            protocol: DecisionProtocol::Auto,
            provider: "ollama".into(),
            model: "tev1:0.8b".into(),
            base_url: None,
            timeout_ms: 1500,
            local_only: true,
            private_paths: Vec::new(),
        }
    }
}

impl DecisionConfig {
    pub const MIN_TIMEOUT_MS: u64 = 100;
    pub const MAX_TIMEOUT_MS: u64 = 10_000;

    pub fn active(&self) -> bool {
        self.enabled && self.strategy != DecisionStrategy::Llm
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(Self::MIN_TIMEOUT_MS..=Self::MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(ConfigError::InvalidDecisionTimeout(self.timeout_ms));
        }
        Ok(())
    }
}

/// Whether private metadata is blocked or reviewed by a local judge first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SensitiveContext {
    #[default]
    Block,
    Decision,
}

impl SensitiveContext {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Decision => "decision",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Block => "Block",
            Self::Decision => "Review with a local decision model",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "block" => Some(Self::Block),
            "decision" | "review" => Some(Self::Decision),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PrivacyConfig {
    pub sensitive_context: SensitiveContext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Active provider; must resolve to a preset or a `[[providers]]` entry.
    pub provider: Option<String>,
    /// Model override; wins over the provider default.
    pub model: Option<String>,
    /// Last selected completion model for each provider. Credentials stay in
    /// the system keychain under the provider's name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_models: BTreeMap<String, String>,
    pub mode: Mode,
    /// Master switch; absent in older configs means enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Terminal application names where shell integration and requests are disabled.
    pub disabled_terminals: Vec<String>,
    pub modes: Modes,
    pub context: ContextConfig,
    pub style: StyleConfig,
    /// Keybindings, overridable by the user.
    pub keys: KeyBindings,
    pub daemon: DaemonConfig,
    /// User-defined providers, appended to the built-in table. Later entries
    /// with the same name shadow built-ins.
    pub providers: Vec<ProviderPreset>,
    /// Optional command decision judge settings; defaults to the original LLM flow.
    pub decision: DecisionConfig,
    pub privacy: PrivacyConfig,
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.decision.validate()
    }

    pub fn terminal_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
            && crate::terminal::current()
                .is_none_or(|name| !crate::terminal::disabled(&self.disabled_terminals, &name))
    }

    pub fn load() -> Result<Self, ConfigError> {
        let dir = config_dir()?;
        if dir.exists() {
            secure_config_dir()?;
        }
        let path = dir.join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut config: Self =
                    toml::from_str(&text).map_err(|source| ConfigError::Parse {
                        path: path.clone(),
                        source,
                    })?;
                config.keys.migrate_legacy_accept();
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io {
                path: config_path()?,
                source,
            }),
        }
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.validate()?;
        let path = config_path()?;
        let dir = secure_config_dir()?;
        let mut file = tempfile::NamedTempFile::new_in(&dir).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        use std::io::Write;
        file.write_all(toml::to_string_pretty(self)?.as_bytes())
            .map_err(|source| ConfigError::Io {
                path: path.clone(),
                source,
            })?;
        file.persist(&path)
            .map(|_| ())
            .map_err(|error| ConfigError::Io {
                path,
                source: error.error,
            })
    }

    /// Switch completion providers while retaining each provider's model.
    /// This does not access or replace provider-specific keychain entries.
    pub fn select_provider(&mut self, name: &str) {
        if self.provider.as_deref() == Some(name) {
            return;
        }
        if let Some(previous) = &self.provider {
            if let Some(model) = self.model.as_ref().filter(|model| !model.trim().is_empty()) {
                self.provider_models.insert(previous.clone(), model.clone());
            } else {
                self.provider_models.remove(previous);
            }
        }
        self.provider = Some(name.to_string());
        self.model = self.provider_models.get(name).cloned().or_else(|| {
            self.provider(Some(name))
                .ok()
                .map(|provider| provider.model)
                .filter(|model| !model.trim().is_empty())
        });
    }

    /// Built-ins first, then user entries shadowing by name.
    pub fn provider_table(&self) -> BTreeMap<String, ProviderPreset> {
        let mut table: BTreeMap<String, ProviderPreset> = builtin_providers()
            .into_iter()
            .map(|p| (p.name.clone(), p))
            .collect();
        for p in &self.providers {
            table.insert(p.name.clone(), p.clone());
        }
        table
    }

    /// Resolve a provider by name, honoring the active-provider default.
    pub fn provider(&self, name: Option<&str>) -> Result<ProviderPreset, ConfigError> {
        let name = name
            .map(str::to_owned)
            .or_else(|| self.provider.clone())
            .ok_or(ConfigError::UnknownProvider(
                "<none>".into(),
                crate::BIN_NAME,
            ))?;
        self.provider_table()
            .remove(&name)
            .ok_or(ConfigError::UnknownProvider(name, crate::BIN_NAME))
    }

    /// Resolve the decision provider independently of the active completion
    /// provider, applying decision-specific model and endpoint overrides.
    pub fn decision_provider(&self) -> Result<ProviderPreset, ConfigError> {
        self.decision.validate()?;
        let mut provider = self.provider(Some(&self.decision.provider))?;
        if provider.wire == Wire::OpenAiAcp {
            return Err(ConfigError::UnsupportedDecisionProvider(provider.name));
        }
        provider.wire = match self.decision.protocol {
            DecisionProtocol::Auto => provider.wire,
            DecisionProtocol::SystemOne => Wire::Jev,
            DecisionProtocol::Chat => match provider.wire {
                Wire::Anthropic => Wire::Anthropic,
                _ => Wire::OpenAiCompat,
            },
        };
        if let Some(base_url) = self
            .decision
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            provider.base_url = base_url.trim_end_matches('/').to_string();
        }
        let model = self.decision.model.trim();
        if model.is_empty() {
            return Err(ConfigError::NoDecisionModel(provider.name));
        }
        provider.model = model.to_string();
        let endpoint = reqwest::Url::parse(&provider.base_url)
            .map_err(|_| ConfigError::InvalidDecisionEndpoint)?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(ConfigError::InvalidDecisionEndpoint);
        }
        let loopback = endpoint.host_str().is_some_and(is_loopback_host);
        if endpoint.scheme() != "https" && !(endpoint.scheme() == "http" && loopback) {
            return Err(ConfigError::InvalidDecisionEndpoint);
        }
        if self.decision.local_only && !loopback {
            return Err(ConfigError::RemoteDecisionDenied);
        }
        Ok(provider)
    }

    /// The model in force: config override, else preset default.
    ///
    /// Presets ship an empty model on purpose — provider catalogues move faster
    /// than this binary does, so a stale guess is worse than asking. Callers must
    /// therefore treat an empty result as "not configured yet" rather than
    /// sending it to the API.
    pub fn effective_model(&self, provider: &ProviderPreset) -> String {
        self.model
            .clone()
            .filter(|_| {
                self.provider
                    .as_deref()
                    .is_none_or(|active| active == provider.name)
            })
            .or_else(|| self.provider_models.get(&provider.name).cloned())
            .or_else(|| {
                let user = self.providers.iter().find(|p| p.name == provider.name);
                user.map(|p| p.model.clone()).filter(|m| !m.is_empty())
            })
            .or_else(|| {
                let m = provider.model.clone();
                if m.is_empty() {
                    None
                } else {
                    Some(m)
                }
            })
            .unwrap_or_default()
    }

    /// The model to use, or a clear error when none is configured.
    pub fn require_model(&self, provider: &ProviderPreset) -> Result<String, ConfigError> {
        let m = self.effective_model(provider);
        if provider.wire == Wire::OpenAiAcp {
            // An empty model lets Codex use its configured default model.
            return Ok(m);
        }
        if m.is_empty() {
            return Err(ConfigError::NoModel(provider.name.clone()));
        }
        Ok(m)
    }
}

/// Configuration directory.
///
/// Defaults to `~/.termplete`, alongside other home-directory tool configs.
/// `TERMPLETE_HOME` wins over `XDG_CONFIG_HOME`; both retain their existing
/// `<override>/termplete` layout. The previous platform default is migrated
/// as a whole so config, history, and logs stay together.
pub fn config_dir() -> Result<PathBuf, ConfigError> {
    if let Some(dir) = std::env::var_os("TERMPLETE_HOME") {
        return Ok(PathBuf::from(dir).join(crate::APP_NAME));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir).join(crate::APP_NAME));
        }
    }
    let home = dirs::home_dir().ok_or(ConfigError::NoConfigDir)?;
    let destination = home.join(format!(".{}", crate::APP_NAME));
    if let Some(legacy) = dirs::config_dir().map(|dir| dir.join(crate::APP_NAME)) {
        migrate_config_dir(&legacy, &destination)?;
    }
    Ok(destination)
}

fn migrate_config_dir(legacy: &Path, destination: &Path) -> Result<(), ConfigError> {
    if destination.exists() || !legacy.is_dir() {
        return Ok(());
    }
    if let Err(source) = std::fs::rename(legacy, destination) {
        // Another shell may have completed the same atomic migration.
        if !destination.is_dir() {
            return Err(ConfigError::Io {
                path: destination.to_path_buf(),
                source,
            });
        }
    }
    Ok(())
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn history_path() -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?.join(&Config::default().context.history_file))
}

/// Per-user daemon socket. `$TMPDIR` is preferred so a stale socket from a
/// previous boot never collides.
pub fn socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("TERMPLETE_SOCKET") {
        return PathBuf::from(dir);
    }
    let base = std::env::temp_dir();
    let uid = unsafe { libc::getuid() };
    base.join(format!("{}-{uid}", crate::APP_NAME))
        .join(format!("{}.sock", crate::APP_NAME))
}

/// Ensure the app-owned configuration directory cannot be read by other users.
pub(crate) fn secure_config_dir() -> Result<PathBuf, ConfigError> {
    let dir = config_dir()?;
    ensure_private_dir(
        &dir,
        "configuration directory must be a real directory owned by the current user",
    )
    .map_err(|source| ConfigError::Io {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

/// Create and verify the private directory used by the default daemon socket.
pub(crate) fn prepare_socket_dir(path: &Path) -> std::io::Result<()> {
    ensure_private_dir(
        path,
        "daemon socket directory is not owned by the current user",
    )
    .map(|_| ())
}

fn ensure_private_dir(path: &Path, ownership_msg: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::getuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            ownership_msg,
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Expand `~` and `$VAR` in a user-supplied string.
pub fn expand(s: &str) -> String {
    shellexpand::full(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

pub fn expand_path(s: &str) -> PathBuf {
    Path::new(&expand(s)).to_path_buf()
}

/// Write the default config with a commented provider example, for
/// `tp init`.
pub fn default_config_toml() -> String {
    let mut out = String::new();
    out.push_str(
        "# termplete configuration\n\
         # Secrets are NOT stored here — they live in your system keychain.\n\
         # Run `tp auth login --provider <name>`.\n\n",
    );
    out.push_str("# Provider key from `tp auth providers`, or any custom name below.\n");
    out.push_str("provider = \"openai\"\n\n");
    out.push_str("# on-demand = local history first; the LLM refines command-like input.\n");
    out.push_str("# automatic = suggestions appear as you type (default).\n");
    out.push_str("mode = \"automatic\"\n\n");
    out.push_str(
        "# Turn off temporarily from `tp settings`; this does not uninstall shell integration.\n",
    );
    out.push_str("enabled = true\n\n");
    out.push_str("# Disable integration in selected terminal apps, e.g. [\"warp\"].\n");
    out.push_str("disabled_terminals = []\n\n");
    out.push_str("[modes]\n");
    out.push_str("min_chars = 2\n");
    out.push_str("on_demand_debounce_ms = 220\n");
    out.push_str("automatic_debounce_ms = 400\n");
    out.push_str("max_suggestions = 3\n\n");
    out.push_str("[context]\n");
    out.push_str("history_entries = 200\n");
    out.push_str("command_memory_entries = 10000\n");
    out.push_str("import_shell_history = true\n");
    out.push_str("history_candidates = 8\n");
    out.push_str("ai_select_history = false # instant local history; true asks AI to choose\n");
    out.push_str(
        "suggestion_scope = \"folder\" # folder uses project-local evidence with global fallback\n",
    );
    out.push_str("capture_output = true # recent terminal results inform suggestions\n");
    out.push_str("max_output_bytes = 4096\n\n");
    out.push_str("[style]\n");
    out.push_str("ghost_color = \"#6b7280\"\n");
    out.push_str("automatic_color = \"#8b5cf6\"\n\n");
    out.push_str("[decision]\n");
    out.push_str("enabled = true # master switch; Standard AI does not call the helper\n");
    out.push_str("strategy = \"llm\" # llm, decision, or hybrid\n");
    out.push_str("protocol = \"auto\" # auto, system-one, or chat\n");
    out.push_str("provider = \"ollama\"\n");
    out.push_str("model = \"tev1:0.8b\"\n");
    out.push_str("timeout_ms = 1500\n");
    out.push_str("local_only = true # allow loopback decision endpoints only\n");
    out.push_str("private_paths = [] # path fragments excluded from AI and decision requests\n");
    out.push_str(
        "# base_url = \"https://example.com/v1\" # optional decision-only endpoint override\n\n",
    );
    out.push_str("[privacy]\n");
    out.push_str("sensitive_context = \"block\" # block, or decision for local review of private metadata\n\n");
    out.push_str("# Any OpenAI-compatible endpoint. `wire` defaults to openai-compat.\n");
    out.push_str("# [[providers]]\n");
    out.push_str("# name = \"my-gateway\"\n");
    out.push_str("# base_url = \"https://example.com/v1\"\n");
    out.push_str("# model = \"some-model\"\n");
    out.push_str("# env_key = \"MY_GATEWAY_API_KEY\"\n\n");
    out.push_str("# Fully local, no key required.\n");
    out.push_str("# [[providers]]\n");
    out.push_str("# name = \"lmstudio\"\n");
    out.push_str("# base_url = \"http://127.0.0.1:1234/v1\"\n");
    out.push_str("# no_auth = true\n\n");
    out
}

/// Re-exported so `tp auth providers` can print a stable table.
pub fn provider_names() -> Vec<String> {
    builtin_providers().into_iter().map(|p| p.name).collect()
}

/// A redacted view of the config, safe to print.
pub fn redacted_summary(cfg: &Config) -> String {
    let mut s = format!(
        "provider = {}\nmodel = {}\nmode = {}\nhistory_entries = {}\n",
        cfg.provider.as_deref().unwrap_or("<unset>"),
        cfg.model.as_deref().unwrap_or("<preset default>"),
        cfg.mode.as_str(),
        cfg.context.history_entries
    );
    s.push_str(&format!(
        "disabled_terminals = {}\n",
        cfg.disabled_terminals.join(", ")
    ));
    s.push_str(&format!("command_memory_entries = {}\nimport_shell_history = {}\nhistory_candidates = {}\nai_select_history = {}\nsuggestion_scope = {}\n",
        cfg.context.command_memory_entries, cfg.context.import_shell_history,
        cfg.context.history_candidates, cfg.context.ai_select_history,
        cfg.context.suggestion_scope.as_str()));
    if !cfg.providers.is_empty() {
        s.push_str(&format!("custom_providers = {}\n", cfg.providers.len()));
    }
    s.push_str(&format!("config_path = {:?}\n", config_path().ok()));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_moves_config_logs_and_history_together_and_is_repeatable() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join("old-config");
        let destination = home.path().join(".termplete");
        std::fs::create_dir(&legacy).unwrap();
        for (name, contents) in [
            ("config.toml", "disabled_terminals = ['warp']"),
            ("daemon.log", "previous diagnostic"),
            ("history.json", "[]"),
        ] {
            std::fs::write(legacy.join(name), contents).unwrap();
        }
        migrate_config_dir(&legacy, &destination).unwrap();
        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("config.toml")).unwrap(),
            "disabled_terminals = ['warp']"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("daemon.log")).unwrap(),
            "previous diagnostic"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("history.json")).unwrap(),
            "[]"
        );
        migrate_config_dir(&legacy, &destination).unwrap();
    }

    #[test]
    fn migration_preserves_an_existing_destination() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join("old-config");
        let destination = home.path().join(".termplete");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(legacy.join("config.toml"), "old").unwrap();
        std::fs::write(destination.join("config.toml"), "current").unwrap();
        migrate_config_dir(&legacy, &destination).unwrap();
        assert_eq!(
            std::fs::read_to_string(destination.join("config.toml")).unwrap(),
            "current"
        );
        assert_eq!(
            std::fs::read_to_string(legacy.join("config.toml")).unwrap(),
            "old"
        );
    }

    #[test]
    fn builtin_table_has_unique_names() {
        let mut seen = std::collections::HashSet::new();
        for p in builtin_providers() {
            assert!(seen.insert(p.name.clone()), "duplicate preset {}", p.name);
        }
    }

    #[test]
    fn ollama_requires_no_key() {
        let ollama = builtin_providers()
            .into_iter()
            .find(|p| p.name == "ollama")
            .expect("ollama preset");
        assert!(ollama.no_auth);
        assert!(ollama.env_key.is_none());
    }

    #[test]
    fn disabling_the_judge_preserves_its_configuration_and_old_configs_still_work() {
        let old: Config =
            toml::from_str("[decision]\nstrategy = 'hybrid'\nmodel = 'custom'").unwrap();
        assert!(old.decision.active());
        assert_eq!(old.privacy.sensitive_context, SensitiveContext::Block);
        let disabled: Config = toml::from_str("[decision]\nenabled = false\nstrategy = 'decision'\nmodel = 'custom'\n[privacy]\nsensitive_context = 'decision'").unwrap();
        assert!(!disabled.decision.active());
        assert_eq!(disabled.decision.strategy, DecisionStrategy::Decision);
        assert_eq!(disabled.decision.model, "custom");
        assert_eq!(
            disabled.privacy.sensitive_context,
            SensitiveContext::Decision
        );
        assert!(!Config::default().decision.active());
        let round_trip: Config = toml::from_str(&toml::to_string(&disabled).unwrap()).unwrap();
        assert_eq!(disabled, round_trip);
    }

    #[test]
    fn old_configs_get_the_disabled_decision_defaults() {
        let cfg: Config = toml::from_str("provider = 'openai'\nmodel = 'gpt-4o'").unwrap();
        assert_eq!(cfg.provider.as_deref(), Some("openai"));
        assert_eq!(cfg.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cfg.decision.strategy, DecisionStrategy::Llm);
        assert_eq!(cfg.decision.protocol, DecisionProtocol::Auto);
        assert_eq!(cfg.decision.provider, "ollama");
        assert_eq!(cfg.decision.model, "tev1:0.8b");
        assert_eq!(cfg.decision.base_url, None);
        assert_eq!(cfg.decision.timeout_ms, 1500);
        assert!(cfg.decision.local_only);
        assert!(cfg.decision.private_paths.is_empty());
    }

    #[test]
    fn openai_wire_accepts_legacy_spelling_and_writes_canonical_name() {
        for wire in ["openai-compat", "open-ai-compat"] {
            let preset: ProviderPreset = toml::from_str(&format!(
                "name = 'custom'\nbase_url = 'https://example.com/v1'\nwire = '{wire}'"
            ))
            .unwrap();
            assert_eq!(preset.wire, Wire::OpenAiCompat);
            assert!(toml::to_string(&preset)
                .unwrap()
                .contains("wire = \"openai-compat\""));
        }
    }

    #[test]
    fn decision_strategy_serializes_with_kebab_case_names() {
        for (strategy, serialized) in [
            (DecisionStrategy::Llm, "llm"),
            (DecisionStrategy::Decision, "decision"),
            (DecisionStrategy::Hybrid, "hybrid"),
        ] {
            let cfg = Config {
                decision: DecisionConfig {
                    strategy,
                    ..DecisionConfig::default()
                },
                ..Config::default()
            };
            let text = toml::to_string(&cfg).unwrap();
            assert!(
                text.contains(&format!("strategy = \"{serialized}\"")),
                "{text}"
            );
            let restored: Config = toml::from_str(&text).unwrap();
            assert_eq!(restored.decision.strategy, strategy);
        }
    }

    #[test]
    fn decision_provider_resolves_separately_with_overrides() {
        let cfg = Config {
            provider: Some("openai".into()),
            decision: DecisionConfig {
                provider: "judge-gateway".into(),
                model: "judge-model".into(),
                base_url: Some("https://judge.example/v1/".into()),
                local_only: false,
                ..DecisionConfig::default()
            },
            providers: vec![ProviderPreset {
                name: "judge-gateway".into(),
                base_url: "https://gateway.example/v1".into(),
                model: "gateway-default".into(),
                wire: Wire::OpenAiCompat,
                env_key: Some("JUDGE_API_KEY".into()),
                description: "test gateway".into(),
                no_auth: false,
            }],
            ..Config::default()
        };
        let judge = cfg.decision_provider().unwrap();
        assert_eq!(judge.name, "judge-gateway");
        assert_eq!(judge.base_url, "https://judge.example/v1");
        assert_eq!(judge.model, "judge-model");
        assert_eq!(cfg.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn decision_protocol_selects_provider_wire_and_keeps_loopback_local() {
        let config = |protocol| Config {
            decision: DecisionConfig {
                provider: "judge-gateway".into(),
                model: "judge-model".into(),
                protocol,
                base_url: Some("http://127.0.0.1:9999/v1".into()),
                ..DecisionConfig::default()
            },
            providers: vec![ProviderPreset {
                name: "judge-gateway".into(),
                base_url: "http://127.0.0.1:8080/v1".into(),
                model: "default-model".into(),
                wire: Wire::Ollama,
                env_key: None,
                description: "local test provider".into(),
                no_auth: true,
            }],
            ..Config::default()
        };

        assert_eq!(
            config(DecisionProtocol::Auto)
                .decision_provider()
                .unwrap()
                .wire,
            Wire::Ollama
        );
        assert_eq!(
            config(DecisionProtocol::SystemOne)
                .decision_provider()
                .unwrap()
                .wire,
            Wire::Jev
        );
        assert_eq!(
            config(DecisionProtocol::Chat)
                .decision_provider()
                .unwrap()
                .wire,
            Wire::OpenAiCompat
        );
        let mut anthropic = config(DecisionProtocol::Chat);
        anthropic.providers[0].wire = Wire::Anthropic;
        assert_eq!(anthropic.decision_provider().unwrap().wire, Wire::Anthropic);

        for (protocol, serialized) in [
            (DecisionProtocol::Auto, "auto"),
            (DecisionProtocol::SystemOne, "system-one"),
            (DecisionProtocol::Chat, "chat"),
        ] {
            let cfg = Config {
                decision: DecisionConfig {
                    protocol,
                    ..DecisionConfig::default()
                },
                ..Config::default()
            };
            let text = toml::to_string(&cfg).unwrap();
            assert!(
                text.contains(&format!("protocol = \"{serialized}\"")),
                "{text}"
            );
            let restored: Config = toml::from_str(&text).unwrap();
            assert_eq!(restored.decision.protocol, protocol);
        }
    }

    #[test]
    fn decision_endpoint_rejects_remote_or_unsafe_urls_without_leaking_them() {
        let config = |base_url: &str, local_only: bool| Config {
            decision: DecisionConfig {
                provider: "ollama".into(),
                base_url: Some(base_url.into()),
                local_only,
                ..DecisionConfig::default()
            },
            ..Config::default()
        };

        assert!(matches!(
            config("https://api.example/v1", true).decision_provider(),
            Err(ConfigError::RemoteDecisionDenied)
        ));
        assert!(config("https://api.example/v1", false)
            .decision_provider()
            .is_ok());
        assert!(matches!(
            config("http://api.example/v1", false).decision_provider(),
            Err(ConfigError::InvalidDecisionEndpoint)
        ));
        for (endpoint, secret) in [
            ("https://user:secret@example.test/v1", "secret"),
            ("https://example.test/v1?key=secret", "secret"),
            ("https://example.test/v1#secret", "secret"),
            ("file:///tmp/judge", "tmp/judge"),
        ] {
            let error = config(endpoint, false).decision_provider().unwrap_err();
            assert!(matches!(error, ConfigError::InvalidDecisionEndpoint));
            assert!(!error.to_string().contains(secret));
        }
    }

    #[test]
    fn decision_provider_requires_a_model_before_returning() {
        let cfg = Config {
            decision: DecisionConfig {
                provider: "openai".into(),
                model: "  ".into(),
                local_only: false,
                ..DecisionConfig::default()
            },
            ..Config::default()
        };
        assert!(matches!(
            cfg.decision_provider(),
            Err(ConfigError::NoDecisionModel(_))
        ));
    }

    #[test]
    fn decision_timeout_is_bounded_and_provider_is_configured() {
        let mut decision = DecisionConfig {
            timeout_ms: DecisionConfig::MIN_TIMEOUT_MS,
            ..DecisionConfig::default()
        };
        assert!(decision.validate().is_ok());
        decision.timeout_ms = DecisionConfig::MAX_TIMEOUT_MS;
        assert!(decision.validate().is_ok());
        decision.timeout_ms = DecisionConfig::MIN_TIMEOUT_MS - 1;
        assert!(decision.validate().is_err());
        decision.timeout_ms = DecisionConfig::MAX_TIMEOUT_MS + 1;
        assert!(decision.validate().is_err());

        let jev = builtin_providers()
            .into_iter()
            .find(|provider| provider.name == "jev")
            .unwrap();
        assert_eq!(jev.base_url, "https://api.typesafe.ai/v1");
        assert_eq!(jev.model, "jev-latest");
        assert_eq!(jev.env_key.as_deref(), Some("TYPESAFE_API_KEY"));
        assert_eq!(jev.wire, Wire::Jev);
        assert!(!jev.no_auth);
    }

    #[test]
    fn only_jev_preset_ships_its_verified_model_alias() {
        // Other provider catalogs move faster than this binary and need user choice.
        for p in builtin_providers() {
            if p.name == "jev" {
                assert_eq!(p.model, "jev-latest");
                continue;
            }
            assert!(
                p.model.is_empty(),
                "preset {} should not hardcode a model",
                p.name
            );
        }
    }

    #[test]
    fn require_model_errors_when_none_is_set() {
        let cfg = Config::default();
        let openai = builtin_providers()
            .into_iter()
            .find(|p| p.name == "openai")
            .unwrap();
        let err = cfg.require_model(&openai).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no model configured"), "{msg}");
        assert!(
            msg.contains("tp setup"),
            "the error must name the fix: {msg}"
        );
    }

    #[test]
    fn provider_switch_restores_models_after_serialization() {
        // Migrate the existing single-model config when leaving its provider.
        let mut cfg: Config =
            toml::from_str("provider = 'openai'\nmodel = 'openai-selected'").unwrap();
        cfg.select_provider("anthropic");
        assert_eq!(
            cfg.model, None,
            "a new provider must not inherit the old model"
        );
        cfg.model = Some("anthropic-selected".into());
        let mut cfg: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        cfg.select_provider("openai");
        assert_eq!(cfg.model.as_deref(), Some("openai-selected"));
        let anthropic = cfg.provider(Some("anthropic")).unwrap();
        assert_eq!(cfg.effective_model(&anthropic), "anthropic-selected");
        cfg.model = Some("openai-updated".into());
        cfg.select_provider("anthropic");
        assert_eq!(cfg.model.as_deref(), Some("anthropic-selected"));
        cfg.select_provider("openai");
        assert_eq!(cfg.model.as_deref(), Some("openai-updated"));
    }

    #[test]
    fn provider_switch_preserves_default_model_selection() {
        let mut cfg = Config::default();
        cfg.select_provider("openai-acp");
        cfg.model = Some("explicit-codex-model".into());
        cfg.select_provider("ollama");
        cfg.model = Some("local-model".into());
        cfg.select_provider("openai-acp");
        cfg.model = None; // Choose the Codex default instead of the saved override.
        cfg.select_provider("ollama");
        assert_eq!(cfg.model.as_deref(), Some("local-model"));
        cfg.select_provider("openai-acp");
        assert_eq!(cfg.model, None);
        assert_eq!(cfg.require_model(&cfg.provider(None).unwrap()).unwrap(), "");
    }

    #[test]
    fn explicit_model_wins_over_everything() {
        let cfg = Config {
            model: Some("my-model".into()),
            ..Config::default()
        };
        let openai = builtin_providers()
            .into_iter()
            .find(|p| p.name == "openai")
            .unwrap();
        assert_eq!(cfg.require_model(&openai).unwrap(), "my-model");
    }

    #[test]
    fn user_provider_base_url_shadows_the_preset() {
        // What the wizard writes when the user edits the base URL.
        let mut preset = builtin_providers()
            .into_iter()
            .find(|p| p.name == "mistral")
            .unwrap();
        preset.base_url = "https://gateway.internal/v1".into();
        let cfg = Config {
            providers: vec![preset],
            ..Config::default()
        };
        let mistral = cfg.provider(Some("mistral")).unwrap();
        assert_eq!(mistral.base_url, "https://gateway.internal/v1");
        assert_eq!(mistral.name, "mistral");
        // Everything else about the preset survives.
        assert_eq!(mistral.wire, Wire::OpenAiCompat);
        assert!(!mistral.no_auth);
    }

    #[test]
    fn local_providers_require_no_key() {
        // Ollama and LM Studio run on localhost; marking them as
        // needing a credential sends users looking for a key that does not exist.
        for name in ["ollama", "lmstudio"] {
            let p = builtin_providers()
                .into_iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} preset"));
            assert!(p.no_auth, "{name} must not require a key");
            assert!(p.env_key.is_none(), "{name} must not name an env var");
            assert!(
                p.base_url.starts_with("http://127.0.0.1"),
                "{name} should point at localhost"
            );
        }
    }

    #[test]
    fn removed_presets_are_not_offered() {
        let names = provider_names();
        for removed in ["llamacpp", "perplexity", "cerebras", "gw"] {
            assert!(!names.iter().any(|name| name == removed));
        }
    }

    #[test]
    fn hosted_providers_do_require_a_key() {
        for name in ["openai", "anthropic", "mistral", "groq"] {
            let p = builtin_providers()
                .into_iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} preset"));
            assert!(!p.no_auth, "{name} should need a credential");
            assert!(p.env_key.is_some(), "{name} should name an env var");
        }
    }

    #[test]
    fn mode_parsing_is_lenient() {
        assert_eq!(Mode::parse("automatic"), Some(Mode::Aggressive));
        assert_eq!(Mode::parse("on-demand"), Some(Mode::Subtle));
        assert_eq!(Mode::parse("AGGRESSIVE"), Some(Mode::Aggressive));
        assert_eq!(Mode::parse("EAGER"), Some(Mode::Aggressive));
        assert_eq!(Mode::parse("subtle"), Some(Mode::Subtle));
        assert_eq!(Mode::parse(" normal "), Some(Mode::Subtle));
        assert_eq!(Mode::parse("nope"), None);
    }

    #[test]
    fn automatic_is_the_default_in_memory_and_in_generated_config() {
        assert_eq!(Config::default().mode, Mode::Aggressive);
        assert_eq!(Config::default().enabled, None);
        let generated: Config = toml::from_str(&default_config_toml()).unwrap();
        assert_eq!(generated.mode, Mode::Aggressive);
        assert_eq!(generated.enabled, Some(true));
        assert_eq!(generated.mode.as_str(), "automatic");
        assert_eq!(generated.keys.accept, "^[[C");
        assert_eq!(generated.decision.strategy, DecisionStrategy::Llm);
        assert_eq!(generated.decision.protocol, DecisionProtocol::Auto);
        assert_eq!(generated.context.suggestion_scope, SuggestionScope::Folder);
        let saved = toml::to_string(&generated).unwrap();
        assert!(saved.contains("on_demand_debounce_ms"));
        assert!(saved.contains("automatic_debounce_ms"));
        assert!(saved.contains("automatic_color"));
    }

    #[test]
    fn global_enable_defaults_on_for_existing_configs_and_can_be_paused() {
        let existing: Config = toml::from_str("mode = \"automatic\"\n").unwrap();
        assert!(existing.terminal_enabled());

        let paused: Config = toml::from_str("enabled = false\n").unwrap();
        assert!(!paused.terminal_enabled());
    }

    #[test]
    fn legacy_mode_and_tuning_load_but_save_with_current_names() {
        let cfg: Config = toml::from_str(
            "mode = 'eager'\n[modes]\neager_debounce_ms = 175\nsubtle_debounce_ms = 260\n[style]\neager_color = '#123456'",
        )
        .unwrap();
        assert_eq!(cfg.mode, Mode::Aggressive);
        assert_eq!(cfg.modes.eager_debounce_ms, 175);
        assert_eq!(cfg.modes.subtle_debounce_ms, 260);
        assert_eq!(cfg.style.eager_color, "#123456");
        let saved = toml::to_string(&cfg).unwrap();
        assert!(saved.contains("mode = \"automatic\""));
        assert!(saved.contains("automatic_debounce_ms = 175"));
        assert!(saved.contains("on_demand_debounce_ms = 260"));
        assert!(saved.contains("automatic_color = \"#123456\""));
        assert!(!saved.contains("eager"));
        assert!(!saved.contains("aggressive"));
        assert!(!saved.contains("subtle_debounce_ms"));
        // Existing offline preferences must continue to avoid network access.
        let offline: Config = toml::from_str("mode = 'off'").unwrap();
        assert_eq!(offline.mode, Mode::Off);
    }

    #[test]
    fn aggressive_debounce_key_remains_a_legacy_alias() {
        let cfg: Config = toml::from_str("[modes]\naggressive_debounce_ms = 175").unwrap();
        assert_eq!(cfg.modes.eager_debounce_ms, 175);
        assert!(toml::to_string(&cfg)
            .unwrap()
            .contains("automatic_debounce_ms = 175"));
    }

    #[test]
    fn user_provider_shadows_builtin() {
        let cfg = Config {
            providers: vec![ProviderPreset {
                name: "openai".into(),
                base_url: "http://127.0.0.1:11434/v1".into(),
                model: "qwen".into(),
                ..ProviderPreset {
                    name: String::new(),
                    base_url: String::new(),
                    model: String::new(),
                    wire: Wire::Ollama,
                    env_key: None,
                    description: String::new(),
                    no_auth: true,
                }
            }],
            ..Config::default()
        };
        let table = cfg.provider_table();
        assert_eq!(table["openai"].model, "qwen");
    }

    #[test]
    fn effective_model_prefers_override() {
        let cfg = Config {
            model: Some("gpt-4o".into()),
            ..Config::default()
        };
        let openai = builtin_providers()
            .into_iter()
            .find(|p| p.name == "openai")
            .unwrap();
        assert_eq!(cfg.effective_model(&openai), "gpt-4o");
    }

    #[test]
    fn normalize_translates_readline_spellings() {
        assert_eq!(KeyBindings::normalize("C-g"), "^G");
        assert_eq!(KeyBindings::normalize("M-f"), "^[f");
        assert_eq!(KeyBindings::normalize("C-c"), "^C");
    }

    #[test]
    fn normalize_passes_through_caret_notation() {
        assert_eq!(KeyBindings::normalize("^G"), "^G");
        assert_eq!(KeyBindings::normalize("^[Z"), "^[Z");
        assert_eq!(KeyBindings::normalize("^[f"), "^[f");
    }

    #[test]
    fn normalize_handles_empty_and_odd_input() {
        assert_eq!(KeyBindings::normalize(""), "");
        assert_eq!(KeyBindings::normalize("   "), "");
        // A trailing prefix with no key is preserved rather than panicking.
        assert_eq!(KeyBindings::normalize("C-"), "C-");
        assert_eq!(KeyBindings::normalize("^"), "^");
    }

    #[test]
    fn normalize_handles_chord_spec() {
        // Ctrl-X Ctrl-X, the double-tap toggle.
        // bindkey reads consecutive keys, so the separator must be dropped.
        assert_eq!(KeyBindings::normalize("C-x C-x"), "^X^X");
    }

    #[test]
    fn legacy_tab_accept_migrates_to_right_arrow_but_custom_keys_stay() {
        let mut legacy = KeyBindings {
            accept: "^I".into(),
            ..KeyBindings::default()
        };
        legacy.migrate_legacy_accept();
        assert_eq!(legacy.accept, "^[[C");

        let mut custom = KeyBindings {
            accept: "M-y".into(),
            ..KeyBindings::default()
        };
        custom.migrate_legacy_accept();
        assert_eq!(custom.accept, "M-y");
    }

    #[test]
    fn default_bindings_are_non_empty_and_distinct() {
        let k = KeyBindings::default();
        for (name, spec) in [
            ("reveal", &k.reveal),
            ("accept", &k.accept),
            ("cycle", &k.cycle),
            ("dismiss", &k.dismiss),
            ("toggle", &k.toggle),
        ] {
            assert!(!spec.is_empty(), "{name} must be bound");
            assert_eq!(
                &KeyBindings::normalize(spec),
                spec,
                "{name} must be canonical"
            );
        }
        // reveal and accept are separate keys, not one key pressed twice, so each
        // can be rebound independently.
        assert_ne!(k.reveal, k.accept);
        assert_ne!(k.accept, k.cycle);
    }

    #[test]
    fn friendly_shortcut_names_normalize_and_describe() {
        for (input, want) in [
            ("Right Arrow", "^[[C"),
            ("Control+G", "^G"),
            ("Shift+Tab", "^[[Z"),
            ("Ctrl+Shift+Z", "^[[27;6;90~"),
            ("Ctrl+X Ctrl+X", "^X^X"),
            ("Home", "^[[H"),
            ("End", "^[[F"),
            ("Insert", "^[[2~"),
            ("Delete", "^[[3~"),
            ("Page Up", "^[[5~"),
            ("PageDown", "^[[6~"),
            ("F1", "^[OP"),
            ("F2", "^[OQ"),
            ("F3", "^[OR"),
            ("F4", "^[OS"),
        ] {
            assert_eq!(KeyBindings::normalize(input), want, "{input}");
        }
        for (input, want) in [
            ("^[[C", "Right Arrow"),
            ("^G", "Ctrl+G"),
            ("^Z", "Ctrl+Z"),
            ("^[[9;2u", "Shift+Tab"),
            ("^[[27;6;90~", "Ctrl+Shift+Z"),
            ("^[[122;6u", "Ctrl+Shift+Z"),
            ("^[[27;6;122~", "Ctrl+Shift+Z"),
            ("^[[1;5C", "Ctrl+Right Arrow"),
            ("^[[1;2Z", "Shift+Tab"),
            ("^[OH", "Home"),
            ("^[[3~", "Delete"),
            ("^[OP", "F1"),
            ("^[[6~", "Page Down"),
            ("^[OC", "Right Arrow"),
            ("^X^X", "Ctrl+X Ctrl+X"),
        ] {
            assert_eq!(KeyBindings::describe(input), want, "{input}");
        }
    }

    #[test]
    fn keybindings_round_trip_through_toml() {
        let cfg = Config {
            keys: KeyBindings {
                accept: "^[y".into(),
                ..KeyBindings::default()
            },
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("[keys]"), "{text}");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.keys.accept, "^[y");
        assert_eq!(back.keys.cycle, "^[[Z");
    }

    #[test]
    fn unknown_provider_errors_carry_hint() {
        let cfg = Config::default();
        let err = cfg.provider(Some("nope")).unwrap_err();
        assert!(matches!(err, ConfigError::UnknownProvider(..)));
        assert!(err.to_string().contains("auth providers"));
    }

    #[test]
    fn missing_active_provider_is_reported() {
        let err = Config::default().provider(None).unwrap_err();
        assert!(err.to_string().contains("<none>"));
    }

    #[test]
    fn roundtrips_through_toml() {
        let cfg = Config {
            provider: Some("groq".into()),
            mode: Mode::Aggressive,
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn older_context_config_defaults_to_folder_scope() {
        let cfg: Config =
            toml::from_str("provider = \"openai\"\n[context]\nhistory_entries = 123\n").unwrap();
        assert_eq!(cfg.context.history_entries, 123);
        assert_eq!(cfg.context.suggestion_scope, SuggestionScope::Folder);
    }
}
