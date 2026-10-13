//! The interactive settings menu behind `tp settings`.
//!
//! `tp config set --key …` is fine for scripts and miserable for people, so the
//! common choices — mode, keybindings and preferences — are offered as a menu. The
//! menu reads the same config the daemon reads and writes through the same
//! `Config::save`, so there is one source of truth.

use dialoguer::console::Term;
use dialoguer::Input;
use std::io::Write;

use crate::config::{
    Config, DecisionConfig, DecisionProtocol, DecisionStrategy, KeyBindings, Mode,
    SensitiveContext, SuggestionScope, Wire,
};

/// One editable setting shown in the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Enabled,
    Mode,
    RevealKey,
    AcceptKey,
    CycleKey,
    DismissKey,
    ToggleKey,
    ResetShortcuts,
    ReplaceProviderKey,
    RemoveProviderKey,
    ChatGptSignIn,
    ChatGptSignOut,
    Animations,
    Spinner,
    AnimationFrames,
    GhostColor,
    EagerColor,
    MinChars,
    Debounce,
    HistoryEntries,
    DisabledTerminals,
    AiSelectHistory,
    SuggestionScope,
    CaptureOutput,
    Provider,
    Model,
    DecisionSettings,
    DecisionStrategy,
    DecisionProtocol,
    DecisionProvider,
    DecisionModel,
    DecisionBaseUrl,
    DecisionTimeout,
    DecisionLocalOnly,
    DecisionPrivatePaths,
    DecisionEnabled,
    SensitiveContext,
    Exit,
}

impl Field {
    fn is_shortcut(&self) -> bool {
        matches!(
            self,
            Self::RevealKey | Self::AcceptKey | Self::CycleKey | Self::DismissKey | Self::ToggleKey
        )
    }

    /// Human label plus the value currently in force.
    pub fn label(&self, cfg: &Config) -> String {
        let keys = &cfg.keys;
        match self {
            Self::Enabled => format!("TermPlete enabled: {}", onoff(cfg.enabled.unwrap_or(true))),
            Self::Mode => format!("Mode: {}", cfg.mode.label()),
            Self::RevealKey => format!("Reveal: {}", KeyBindings::describe(&keys.reveal)),
            Self::AcceptKey => format!(
                "Accept key (Right Arrow by default): {}",
                KeyBindings::describe(&keys.accept)
            ),
            Self::CycleKey => format!(
                "Previous option (Shift+Tab): {}",
                KeyBindings::describe(&keys.cycle)
            ),
            Self::DismissKey => format!("Dismiss: {}", KeyBindings::describe(&keys.dismiss)),
            Self::ToggleKey => format!("Toggle: {}", KeyBindings::describe(&keys.toggle)),
            Self::ResetShortcuts => "Reset shortcuts to defaults".into(),
            Self::Animations => format!("Animations               {}", onoff(cfg.style.animations)),
            Self::Spinner => format!("Spinner                  {}", onoff(cfg.style.spinner)),
            Self::AnimationFrames => {
                format!("Animation frames         {}", cfg.style.animation_frames)
            }
            Self::GhostColor => format!("Ghost colour             {}", cfg.style.ghost_color),
            Self::EagerColor => format!("Accent colour            {}", cfg.style.eager_color),
            Self::MinChars => format!("Suggest after: {} chars", cfg.modes.min_chars),
            Self::Debounce => format!(
                "Delay: {} ms",
                if cfg.mode == Mode::Aggressive {
                    cfg.modes.eager_debounce_ms
                } else {
                    cfg.modes.subtle_debounce_ms
                }
            ),
            Self::HistoryEntries => format!("History: {} commands", cfg.context.history_entries),
            Self::DisabledTerminals => {
                format!("Terminal apps: {} disabled", cfg.disabled_terminals.len())
            }
            Self::AiSelectHistory => format!(
                "History matches: {}",
                if cfg.context.ai_select_history {
                    "AI choice"
                } else {
                    "Instant"
                }
            ),
            Self::SuggestionScope => format!(
                "Suggestion history: {}",
                match cfg.context.suggestion_scope {
                    SuggestionScope::Folder => "Current project folder",
                    SuggestionScope::Global => "Global",
                }
            ),
            Self::CaptureOutput => format!(
                "Capture terminal output: {}",
                onoff(cfg.context.capture_output)
            ),
            Self::Provider => format!("Provider: {}", provider_display_name(cfg)),
            Self::ReplaceProviderKey => format!(
                "Replace API key for {}",
                cfg.provider.as_deref().unwrap_or("<choose provider>")
            ),
            Self::RemoveProviderKey => format!(
                "Remove stored key for {}",
                cfg.provider.as_deref().unwrap_or("<choose provider>")
            ),
            Self::ChatGptSignIn => "Sign in to ChatGPT".into(),
            Self::ChatGptSignOut => "Sign out of ChatGPT".into(),
            Self::Model => format!("Model: {}", model_display_name(cfg)),
            Self::DecisionSettings => format!(
                "Decision helper: {}",
                if cfg.decision.active() {
                    cfg.decision.strategy.label()
                } else {
                    "off"
                }
            ),
            Self::DecisionEnabled => {
                format!("Use decision helper: {}", onoff(cfg.decision.active()))
            }
            Self::SensitiveContext => format!(
                "Sensitive context: {}",
                cfg.privacy.sensitive_context.label()
            ),
            Self::DecisionStrategy => {
                format!("Strategy: {}", cfg.decision.strategy.label())
            }
            Self::DecisionProtocol => {
                format!("Request protocol: {}", cfg.decision.protocol.label())
            }
            Self::DecisionProvider => {
                format!("Judge provider: {}", cfg.decision.provider)
            }
            Self::DecisionModel => format!("Judge model: {}", cfg.decision.model),
            Self::DecisionBaseUrl => format!(
                "Judge endpoint: {}",
                cfg.decision
                    .base_url
                    .as_deref()
                    .unwrap_or("<provider default>")
            ),
            Self::DecisionTimeout => format!("Judge latency limit: {} ms", cfg.decision.timeout_ms),
            Self::DecisionLocalOnly => {
                format!("Require a local judge: {}", onoff(cfg.decision.local_only))
            }
            Self::DecisionPrivatePaths => format!(
                "Private path fragments: {}",
                if cfg.decision.private_paths.is_empty() {
                    "none".into()
                } else {
                    cfg.decision.private_paths.join(", ")
                }
            ),
            Self::Exit => "Save and exit".into(),
        }
    }
}

fn onoff(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

/// The menu order.
pub fn fields() -> Vec<Field> {
    vec![
        Field::Enabled,
        Field::Mode,
        Field::Provider,
        Field::Model,
        Field::DecisionSettings,
        Field::DecisionEnabled,
        Field::SensitiveContext,
        Field::RevealKey,
        Field::AcceptKey,
        Field::CycleKey,
        Field::DismissKey,
        Field::ToggleKey,
        Field::ResetShortcuts,
        Field::ReplaceProviderKey,
        Field::RemoveProviderKey,
        Field::ChatGptSignIn,
        Field::ChatGptSignOut,
        Field::Animations,
        Field::Spinner,
        Field::AnimationFrames,
        Field::GhostColor,
        Field::EagerColor,
        Field::MinChars,
        Field::Debounce,
        Field::HistoryEntries,
        Field::DisabledTerminals,
        Field::AiSelectHistory,
        Field::SuggestionScope,
        Field::CaptureOutput,
        Field::Exit,
    ]
}

/// Apply one edited value to the config. Returns whether anything changed.
pub fn apply(field: &Field, value: &str, cfg: &mut Config) -> Result<bool, String> {
    let trimmed = value.trim();
    match field {
        Field::Enabled => cfg.enabled = Some(parse_bool(trimmed)?),
        Field::Mode => {
            cfg.mode = Mode::parse(trimmed).ok_or_else(|| {
                format!("`{trimmed}` is not one of automatic, on-demand, off (legacy aliases: aggressive, eager, subtle)")
            })?;
        }
        Field::Provider => cfg.select_provider(trimmed),
        Field::Model => cfg.model = Some(trimmed.to_string()),
        Field::DecisionSettings => return Ok(false),
        Field::DecisionStrategy => {
            cfg.decision.strategy = DecisionStrategy::parse(trimmed)
                .ok_or_else(|| "choose standard AI, decision only, or decision + AI".to_string())?;
            cfg.decision.enabled = true;
        }
        Field::DecisionEnabled => {
            cfg.decision.enabled = parse_bool(trimmed)?;
            if cfg.decision.enabled && cfg.decision.strategy == DecisionStrategy::Llm {
                cfg.decision.strategy = DecisionStrategy::Hybrid;
            }
        }
        Field::SensitiveContext => {
            cfg.privacy.sensitive_context = SensitiveContext::parse(trimmed)
                .ok_or_else(|| "choose block or decision".to_string())?
        }
        Field::DecisionProtocol => {
            cfg.decision.protocol = DecisionProtocol::parse(trimmed).ok_or_else(|| {
                "choose provider default, System One / JEV, or OpenAI-compatible JSON".to_string()
            })?;
        }
        Field::DecisionProvider => cfg.decision.provider = trimmed.to_string(),
        Field::DecisionModel => cfg.decision.model = trimmed.to_string(),
        Field::DecisionBaseUrl => {
            cfg.decision.base_url = (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Field::DecisionTimeout => {
            let timeout_ms = trimmed
                .parse()
                .map_err(|_| "expected a whole number of milliseconds".to_string())?;
            if !(DecisionConfig::MIN_TIMEOUT_MS..=DecisionConfig::MAX_TIMEOUT_MS)
                .contains(&timeout_ms)
            {
                return Err("decision latency limit must be between 100 and 10000 ms".into());
            }
            cfg.decision.timeout_ms = timeout_ms;
        }
        Field::DecisionLocalOnly => cfg.decision.local_only = parse_bool(trimmed)?,
        Field::DecisionPrivatePaths => {
            cfg.decision.private_paths = trimmed
                .split(',')
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_string)
                .collect()
        }
        Field::DisabledTerminals => cfg.disabled_terminals = crate::terminal::parse_list(trimmed),
        Field::AiSelectHistory => cfg.context.ai_select_history = parse_bool(trimmed)?,
        Field::SuggestionScope => {
            cfg.context.suggestion_scope = SuggestionScope::parse(trimmed)
                .ok_or_else(|| "choose current folder or global suggestion history".to_string())?;
        }
        Field::CaptureOutput => cfg.context.capture_output = parse_bool(trimmed)?,
        Field::RevealKey => cfg.keys.reveal = shortcut_value(trimmed)?,
        Field::AcceptKey => {
            cfg.keys.accept = KeyBindings::normalize(trimmed);
            cfg.keys.migrate_legacy_accept();
        }
        Field::CycleKey => cfg.keys.cycle = shortcut_value(trimmed)?,
        Field::DismissKey => cfg.keys.dismiss = shortcut_value(trimmed)?,
        Field::ToggleKey => cfg.keys.toggle = shortcut_value(trimmed)?,
        Field::ResetShortcuts => cfg.keys = KeyBindings::default(),
        Field::ReplaceProviderKey
        | Field::RemoveProviderKey
        | Field::ChatGptSignIn
        | Field::ChatGptSignOut => return Ok(false),
        Field::Animations => cfg.style.animations = parse_bool(trimmed)?,
        Field::Spinner => cfg.style.spinner = parse_bool(trimmed)?,
        Field::AnimationFrames => {
            cfg.style.animation_frames = trimmed
                .parse()
                .map_err(|_| "expected a whole number".to_string())?
        }
        Field::GhostColor => cfg.style.ghost_color = trimmed.to_string(),
        Field::EagerColor => cfg.style.eager_color = trimmed.to_string(),
        Field::MinChars => {
            cfg.modes.min_chars = trimmed
                .parse()
                .map_err(|_| "expected a whole number".to_string())?
        }
        Field::Debounce => {
            let ms = trimmed
                .parse()
                .map_err(|_| "expected a whole number".to_string())?;
            if cfg.mode == Mode::Aggressive {
                cfg.modes.eager_debounce_ms = ms;
            } else {
                cfg.modes.subtle_debounce_ms = ms;
            }
        }
        Field::HistoryEntries => {
            cfg.context.history_entries = trimmed
                .parse()
                .map_err(|_| "expected a whole number".to_string())?
        }
        Field::Exit => return Ok(false),
    }
    Ok(true)
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(format!("`{v}` is not on/off")),
    }
}

fn shortcut_value(value: &str) -> Result<String, String> {
    let normalized = KeyBindings::normalize(value);
    if normalized == "^I" {
        Err("Tab is reserved for opening and cycling suggestion options".into())
    } else {
        Ok(normalized)
    }
}

/// The value a field should be seeded with when editing.
pub fn current(field: &Field, cfg: &Config) -> String {
    match field {
        Field::Enabled => cfg.enabled.unwrap_or(true).to_string(),
        Field::CaptureOutput => cfg.context.capture_output.to_string(),
        Field::Mode => cfg.mode.as_str().into(),
        Field::Provider => cfg.provider.clone().unwrap_or_default(),
        Field::Model => cfg.model.clone().unwrap_or_default(),
        Field::DecisionSettings => String::new(),
        Field::DecisionEnabled => onoff(cfg.decision.active()).into(),
        Field::SensitiveContext => cfg.privacy.sensitive_context.as_str().into(),
        Field::DecisionStrategy => cfg.decision.strategy.as_str().into(),
        Field::DecisionProtocol => cfg.decision.protocol.as_str().into(),
        Field::DecisionProvider => cfg.decision.provider.clone(),
        Field::DecisionModel => cfg.decision.model.clone(),
        Field::DecisionBaseUrl => cfg.decision.base_url.clone().unwrap_or_default(),
        Field::DecisionTimeout => cfg.decision.timeout_ms.to_string(),
        Field::DecisionLocalOnly => onoff(cfg.decision.local_only).into(),
        Field::DecisionPrivatePaths => cfg.decision.private_paths.join(","),
        Field::RevealKey => cfg.keys.reveal.clone(),
        Field::AcceptKey => cfg.keys.accept.clone(),
        Field::CycleKey => cfg.keys.cycle.clone(),
        Field::DismissKey => cfg.keys.dismiss.clone(),
        Field::ToggleKey => cfg.keys.toggle.clone(),
        Field::ResetShortcuts => String::new(),
        Field::ReplaceProviderKey
        | Field::RemoveProviderKey
        | Field::ChatGptSignIn
        | Field::ChatGptSignOut => String::new(),
        Field::Animations => onoff(cfg.style.animations).into(),
        Field::Spinner => onoff(cfg.style.spinner).into(),
        Field::AnimationFrames => cfg.style.animation_frames.to_string(),
        Field::GhostColor => cfg.style.ghost_color.clone(),
        Field::EagerColor => cfg.style.eager_color.clone(),
        Field::MinChars => cfg.modes.min_chars.to_string(),
        Field::Debounce => if cfg.mode == Mode::Aggressive {
            cfg.modes.eager_debounce_ms
        } else {
            cfg.modes.subtle_debounce_ms
        }
        .to_string(),
        Field::HistoryEntries => cfg.context.history_entries.to_string(),
        Field::DisabledTerminals => cfg.disabled_terminals.join(","),
        Field::AiSelectHistory => onoff(cfg.context.ai_select_history).into(),
        Field::SuggestionScope => cfg.context.suggestion_scope.as_str().into(),
        Field::Exit => String::new(),
    }
}

/// Run a short menu, with details one level below the common choices.
pub fn run() -> Result<(), String> {
    let mut cfg = Config::load().map_err(|e| e.to_string())?;
    let original_provider = cfg.provider.clone();
    let original_model = cfg.model.clone();
    let original_providers = cfg.providers.clone();
    let mut pending_keys = std::collections::BTreeMap::<String, String>::new();
    let original_capture_output = cfg.context.capture_output;
    let original_decision = cfg.decision.clone();
    let original_privacy = cfg.privacy.clone();
    let term = Term::stderr();
    let interactive = crate::menu::interactive();
    loop {
        let labels = vec![
            format!("Suggestion mode  [{}]", cfg.mode.label()),
            format!("AI provider  [{}]", provider_display_name(&cfg)),
            "Keyboard shortcuts — Tab opens/cycles options; Right Arrow accepts".into(),
            "Preferences".into(),
            "Appearance".into(),
            "Save and exit".into(),
        ];
        let Some(index) = crate::menu::select("Settings", &labels, 0).map_err(|e| e.to_string())?
        else {
            eprintln!("Changes discarded.");
            return Ok(());
        };
        if index == 5 {
            cfg.save().map_err(|e| e.to_string())?;
            for (provider, key) in &pending_keys {
                crate::setup::explain_keychain_access(provider);
                crate::setup::store_key(provider, key, &cfg).map_err(|e| e.to_string())?;
                eprintln!("Stored the API key for `{provider}` in the system keychain.");
            }
            eprintln!("Settings saved. zsh applies shortcuts and mode at the next prompt.");
            if (cfg.decision != original_decision
                || cfg.privacy != original_privacy
                || cfg.provider != original_provider
                || cfg.model != original_model
                || cfg.providers != original_providers)
                && crate::auth::stop_daemon_if_running().map_err(|e| e.to_string())?
            {
                eprintln!("Stopped the running daemon to apply AI settings on the next request.");
            }
            if cfg.context.capture_output != original_capture_output {
                eprintln!("Open a new terminal session to apply the output capture change.");
            }
            return Ok(());
        }
        if index == 0 {
            let modes = vec![
                "Automatic (default) - suggestions appear as you type".into(),
                "On demand - reveal suggestions with a shortcut or Tab".into(),
            ];
            if let Some(index) = crate::menu::select(
                "Suggestion mode",
                &modes,
                usize::from(cfg.mode == Mode::Subtle),
            )
            .map_err(|e| e.to_string())?
            {
                cfg.mode = if index == 0 {
                    Mode::Aggressive
                } else {
                    Mode::Subtle
                };
            }
            continue;
        }
        loop {
            // Rebuild the rows each time through the submenu. Provider changes
            // then replace API-key actions with ChatGPT sign-in actions at once.
            let items = match index {
                1 => {
                    let mut fields = vec![Field::Provider, Field::Model, Field::DecisionSettings];
                    if cfg
                        .provider(None)
                        .is_ok_and(|provider| crate::auth::uses_external_auth(&provider))
                    {
                        fields.extend([Field::ChatGptSignIn, Field::ChatGptSignOut]);
                    } else if cfg.provider(None).is_ok_and(|provider| !provider.no_auth) {
                        fields.extend([Field::ReplaceProviderKey, Field::RemoveProviderKey]);
                    }
                    fields
                }
                2 => vec![
                    Field::RevealKey,
                    Field::AcceptKey,
                    Field::CycleKey,
                    Field::DismissKey,
                    Field::ToggleKey,
                    Field::ResetShortcuts,
                ],
                4 => vec![Field::Animations, Field::GhostColor, Field::EagerColor],
                _ => vec![
                    Field::DisabledTerminals,
                    Field::Enabled,
                    Field::AiSelectHistory,
                    Field::SuggestionScope,
                    Field::CaptureOutput,
                    Field::MinChars,
                    Field::Debounce,
                    Field::HistoryEntries,
                    Field::SensitiveContext,
                ],
            };
            let mut labels: Vec<_> = items.iter().map(|f| f.label(&cfg)).collect();
            labels.push("Back".into());
            let Some(index) = crate::menu::select(
                match index {
                    1 => "AI provider",
                    2 => "Shortcuts",
                    4 => "Appearance",
                    _ => "Preferences",
                },
                &labels,
                0,
            )
            .map_err(|e| e.to_string())?
            else {
                break;
            };
            let Some(field) = items.get(index) else {
                break;
            };
            let field = field.clone();
            if field == Field::ResetShortcuts {
                let mut updated = cfg.clone();
                apply(&field, "", &mut updated)?;
                cfg = updated;
                eprintln!("Shortcuts reset to defaults. Save and exit to apply them.");
                continue;
            }
            if field == Field::ReplaceProviderKey {
                if let Some(name) = cfg
                    .provider
                    .as_ref()
                    .filter(|name| pending_keys.contains_key(*name))
                {
                    match crate::setup::prompt_api_key(name) {
                        Ok(key) => {
                            pending_keys.insert(name.clone(), key);
                            eprintln!("Replacement key received. Save and exit to store it.");
                        }
                        Err(error) => eprintln!("{error}"),
                    }
                    continue;
                }
                if let Err(error) = replace_provider_key(&cfg) {
                    eprintln!("{error}");
                }
                continue;
            }
            if field == Field::RemoveProviderKey {
                if let Some(name) = cfg
                    .provider
                    .as_ref()
                    .filter(|name| pending_keys.contains_key(*name))
                {
                    pending_keys.remove(name);
                    eprintln!("Removed the API key from this unsaved provider draft.");
                    continue;
                }
                if let Err(error) = remove_provider_key(&cfg) {
                    eprintln!("{error}");
                }
                continue;
            }
            if field == Field::ChatGptSignIn {
                if let Err(error) = chatgpt_sign_in(&cfg) {
                    eprintln!("{error}");
                }
                continue;
            }
            if field == Field::ChatGptSignOut {
                if let Err(error) = chatgpt_sign_out(&cfg) {
                    eprintln!("{error}");
                }
                continue;
            }
            if field == Field::DecisionSettings {
                decision_settings(&mut cfg, interactive, &term)?;
                continue;
            }
            if field == Field::DisabledTerminals {
                terminal_apps(&mut cfg, interactive, &term)?;
                continue;
            }
            if field == Field::AiSelectHistory {
                let options = vec![
                    "Instant local history (default)".into(),
                    "AI chooses a known command".into(),
                ];
                if let Some(index) = crate::menu::select(
                    "History matches",
                    &options,
                    usize::from(cfg.context.ai_select_history),
                )
                .map_err(|e| e.to_string())?
                {
                    cfg.context.ai_select_history = index == 1;
                }
                continue;
            }
            if field == Field::SuggestionScope {
                let options = vec![
                    "Current project folder — local evidence with global fallback".into(),
                    "Global — share command ranking across folders".into(),
                ];
                if let Some(index) = crate::menu::select(
                    "Suggestion history scope",
                    &options,
                    usize::from(cfg.context.suggestion_scope == SuggestionScope::Global),
                )
                .map_err(|e| e.to_string())?
                {
                    cfg.context.suggestion_scope = if index == 0 {
                        SuggestionScope::Folder
                    } else {
                        SuggestionScope::Global
                    };
                }
                continue;
            }
            if field == Field::SensitiveContext {
                eprintln!(
                    "Credential values, credential paths and excluded paths always stay blocked."
                );
                eprintln!("Review lets an enabled local helper use other private metadata; remote prompts remain filtered.");
                let options = [
                    "Block — keep hidden and private metadata out of models (default)".into(),
                    "Review with a local decision model — use metadata for local decisions".into(),
                ];
                if let Some(selected) = crate::menu::select(
                    "Sensitive context",
                    &options,
                    usize::from(cfg.privacy.sensitive_context == SensitiveContext::Decision),
                )
                .map_err(|e| e.to_string())?
                {
                    cfg.privacy.sensitive_context = if selected == 0 {
                        SensitiveContext::Block
                    } else {
                        SensitiveContext::Decision
                    };
                }
                continue;
            }
            if field == Field::CaptureOutput {
                eprintln!(
                    "Recent command output is saved locally and may be sent to your AI provider."
                );
                eprintln!("Helps with file listings and errors. Applies to new terminal sessions.");
                let options = vec![
                    "On — use recent output as AI context".into(),
                    "Off — stop collecting output; keep autocomplete".into(),
                ];
                if let Some(index) = crate::menu::select(
                    "Capture terminal output",
                    &options,
                    usize::from(!cfg.context.capture_output),
                )
                .map_err(|e| e.to_string())?
                {
                    cfg.context.capture_output = index == 0;
                }
                continue;
            }
            if field == Field::Enabled {
                let options = vec![
                    "On — enable TermPlete in supported terminals".into(),
                    "Off — pause TermPlete without uninstalling it".into(),
                ];
                if let Some(index) = crate::menu::select(
                    "TermPlete enabled",
                    &options,
                    usize::from(!cfg.enabled.unwrap_or(true)),
                )
                .map_err(|e| e.to_string())?
                {
                    cfg.enabled = Some(index == 0);
                }
                continue;
            }
            let value = if field == Field::Provider {
                let providers = completion_provider_names(&cfg);
                let mut labels: Vec<_> = providers
                    .iter()
                    .map(|name| {
                        cfg.provider(Some(name))
                            .map(|preset| preset.display_name().to_string())
                            .unwrap_or_else(|_| name.clone())
                    })
                    .collect();
                labels.push(
                    "+ Custom OpenAI-compatible provider — enter base URL and API key".into(),
                );
                let default = providers
                    .iter()
                    .position(|p| Some(p.as_str()) == cfg.provider.as_deref())
                    .unwrap_or(0);
                let Some(index) =
                    crate::menu::select("Provider", &labels, default).map_err(|e| e.to_string())?
                else {
                    continue;
                };
                if index == providers.len() {
                    match crate::setup::run_custom(&cfg) {
                        Ok(mut choices) => {
                            if let Some(key) = choices.api_key.take() {
                                let name = choices
                                    .provider
                                    .as_ref()
                                    .expect("custom provider has a name");
                                pending_keys.insert(name.clone(), key);
                            }
                            crate::setup::apply_in_memory(&mut cfg, &choices);
                            eprintln!("Custom provider configured. Save and exit to store it and its API key.");
                        }
                        Err(error) => eprintln!("{error}"),
                    }
                    continue;
                }
                providers[index].clone()
            } else if field.is_shortcut() && interactive {
                let label = field.label(&cfg);
                match capture_shortcut(&label)? {
                    Some(value) => value,
                    None => continue,
                }
            } else {
                let default = current(&field, &cfg);
                let default = if field.is_shortcut() {
                    KeyBindings::describe(&default)
                } else {
                    default
                };
                read_line_with_default(name_of(&field), &default, interactive, &term)?
            };
            // An invalid edit must not replace the last valid configuration.
            let mut updated = cfg.clone();
            match apply(&field, &value, &mut updated) {
                Ok(true) => {
                    cfg = updated;
                }
                Ok(false) => {}
                Err(e) => eprintln!("{e}"),
            }
        }
    }
}

/// The configuration key associated with a field.
pub fn name_of(field: &Field) -> &'static str {
    match field {
        Field::Enabled => "enabled",
        Field::CaptureOutput => "capture_output",
        Field::Mode => "mode",
        Field::Provider => "provider",
        Field::Model => "model",
        Field::DecisionSettings => "decision",
        Field::DecisionEnabled => "decision.enabled",
        Field::SensitiveContext => "privacy.sensitive_context",
        Field::DecisionStrategy => "decision.strategy",
        Field::DecisionProtocol => "decision.protocol",
        Field::DecisionProvider => "decision.provider",
        Field::DecisionModel => "decision.model",
        Field::DecisionBaseUrl => "decision.base_url",
        Field::DecisionTimeout => "decision.timeout_ms",
        Field::DecisionLocalOnly => "decision.local_only",
        Field::DecisionPrivatePaths => "decision.private_paths",
        Field::RevealKey => "keys.reveal",
        Field::AcceptKey => "keys.accept",
        Field::CycleKey => "keys.cycle",
        Field::DismissKey => "keys.dismiss",
        Field::ToggleKey => "keys.toggle",
        Field::ResetShortcuts => "reset_shortcuts",
        Field::ReplaceProviderKey => "provider_key_replace",
        Field::RemoveProviderKey => "provider_key_remove",
        Field::ChatGptSignIn => "chatgpt_sign_in",
        Field::ChatGptSignOut => "chatgpt_sign_out",
        Field::Animations => "animations",
        Field::Spinner => "spinner",
        Field::AnimationFrames => "animation_frames",
        Field::GhostColor => "ghost_color",
        Field::EagerColor => "automatic_color",
        Field::MinChars => "min_chars",
        Field::Debounce => "debounce",
        Field::HistoryEntries => "history",
        Field::DisabledTerminals => "disabled_terminals",
        Field::AiSelectHistory => "ai_select_history",
        Field::SuggestionScope => "context.suggestion_scope",
        Field::Exit => "exit",
    }
}

fn provider_display_name(cfg: &Config) -> String {
    cfg.provider(None)
        .map(|preset| preset.display_name().to_string())
        .unwrap_or_else(|_| cfg.provider.as_deref().unwrap_or("<unset>").to_string())
}

fn model_display_name(cfg: &Config) -> String {
    cfg.model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if cfg
                .provider(None)
                .is_ok_and(|preset| preset.wire == Wire::OpenAiAcp)
            {
                "<Codex default>".into()
            } else {
                "<preset default>".into()
            }
        })
}

fn completion_provider_names(cfg: &Config) -> Vec<String> {
    cfg.provider_table()
        .into_iter()
        .filter_map(|(name, preset)| (preset.wire != Wire::Jev).then_some(name))
        .collect()
}

fn decision_settings(cfg: &mut Config, interactive: bool, term: &Term) -> Result<(), String> {
    let items = [
        Field::DecisionStrategy,
        Field::DecisionProtocol,
        Field::DecisionProvider,
        Field::DecisionModel,
        Field::DecisionBaseUrl,
        Field::DecisionTimeout,
        Field::DecisionLocalOnly,
        Field::DecisionPrivatePaths,
        Field::DecisionEnabled,
    ];
    loop {
        let mut labels: Vec<_> = items.iter().map(|field| field.label(cfg)).collect();
        labels.push("Back".into());
        let title = format!("Decision helper [{}]", cfg.decision.strategy.label());
        let Some(index) = crate::menu::select(&title, &labels, 0).map_err(|e| e.to_string())?
        else {
            break;
        };
        let Some(field) = items.get(index) else {
            break;
        };

        if *field == Field::DecisionEnabled {
            let options = [
                "On — use the selected decision strategy".into(),
                "Off — bypass the helper and keep its settings".into(),
            ];
            if let Some(selected) = crate::menu::select(
                "Use decision helper",
                &options,
                usize::from(!cfg.decision.active()),
            )
            .map_err(|e| e.to_string())?
            {
                apply(field, if selected == 0 { "on" } else { "off" }, cfg)?;
            }
            continue;
        }

        if *field == Field::DecisionStrategy {
            let strategies = [
                DecisionStrategy::Llm,
                DecisionStrategy::Decision,
                DecisionStrategy::Hybrid,
            ];
            let options: Vec<_> = strategies
                .iter()
                .map(|strategy| strategy.label().to_string())
                .collect();
            let default = strategies
                .iter()
                .position(|strategy| *strategy == cfg.decision.strategy)
                .unwrap_or(0);
            if let Some(selected) = crate::menu::select("Decision strategy", &options, default)
                .map_err(|e| e.to_string())?
            {
                cfg.decision.strategy = strategies[selected];
                cfg.decision.enabled = true;
            }
            continue;
        }

        if *field == Field::DecisionProvider {
            let providers: Vec<_> = cfg
                .provider_table()
                .into_iter()
                .filter_map(|(name, preset)| (preset.wire != Wire::OpenAiAcp).then_some(name))
                .collect();
            let default = providers
                .iter()
                .position(|provider| provider == &cfg.decision.provider)
                .unwrap_or(0);
            if let Some(selected) = crate::menu::select("Decision provider", &providers, default)
                .map_err(|e| e.to_string())?
            {
                let name = &providers[selected];
                if name != &cfg.decision.provider {
                    let preset = cfg.provider(Some(name)).map_err(|e| e.to_string())?;
                    cfg.decision.provider.clone_from(name);
                    cfg.decision.model = if name == "ollama" {
                        "tev1:0.8b".into()
                    } else {
                        preset.model
                    };
                    cfg.decision.protocol = DecisionProtocol::Auto;
                    cfg.decision.base_url = None;
                }
            }
            continue;
        }

        if *field == Field::DecisionProtocol {
            let protocols = [
                DecisionProtocol::Auto,
                DecisionProtocol::SystemOne,
                DecisionProtocol::Chat,
            ];
            let options: Vec<_> = protocols
                .iter()
                .map(|protocol| protocol.label().to_string())
                .collect();
            let default = protocols
                .iter()
                .position(|protocol| *protocol == cfg.decision.protocol)
                .unwrap_or(0);
            if let Some(selected) =
                crate::menu::select("Decision request protocol", &options, default)
                    .map_err(|e| e.to_string())?
            {
                cfg.decision.protocol = protocols[selected];
            }
            continue;
        }

        if *field == Field::DecisionLocalOnly {
            eprintln!("When enabled, decision requests can only go to localhost endpoints.");
            let options = [
                "On — require a local decision provider".into(),
                "Off — allow a remote decision provider".into(),
            ];
            if let Some(selected) = crate::menu::select(
                "Private context",
                &options,
                usize::from(!cfg.decision.local_only),
            )
            .map_err(|e| e.to_string())?
            {
                cfg.decision.local_only = selected == 0;
            }
            continue;
        }

        let default = current(field, cfg);
        let value = read_line_with_default(name_of(field), &default, interactive, term)?;
        let mut updated = cfg.clone();
        match apply(field, &value, &mut updated) {
            Ok(true) => *cfg = updated,
            Ok(false) => {}
            Err(error) => eprintln!("{error}"),
        }
    }
    Ok(())
}

fn selected_provider(cfg: &Config) -> Result<(String, crate::config::ProviderPreset), String> {
    let name = cfg
        .provider
        .as_deref()
        .ok_or_else(|| "Choose a provider before changing its API key".to_string())?;
    let preset = cfg
        .provider(Some(name))
        .map_err(|error| error.to_string())?;
    if crate::auth::uses_external_auth(&preset) {
        return Err("ChatGPT subscription uses Codex; choose Sign in to ChatGPT".into());
    }
    if preset.no_auth {
        return Err(format!("Provider `{name}` does not use an API key"));
    }
    Ok((name.to_string(), preset))
}

fn chatgpt_sign_in(cfg: &Config) -> Result<(), String> {
    let preset = cfg.provider(None).map_err(|error| error.to_string())?;
    if !crate::auth::uses_external_auth(&preset) {
        return Err("Select ChatGPT subscription before signing in".into());
    }
    crate::acp::login().map_err(|error| error.to_string())?;
    eprintln!("Signed in to ChatGPT through Codex.");
    if crate::auth::stop_daemon_if_running().map_err(|error| error.to_string())? {
        eprintln!("Stopped the running daemon to apply the updated ChatGPT sign-in.");
    }
    Ok(())
}

fn chatgpt_sign_out(cfg: &Config) -> Result<(), String> {
    let preset = cfg.provider(None).map_err(|error| error.to_string())?;
    if !crate::auth::uses_external_auth(&preset) {
        return Err("Select ChatGPT subscription before signing out".into());
    }
    crate::acp::logout().map_err(|error| error.to_string())?;
    eprintln!("Signed out of ChatGPT through Codex.");
    if crate::auth::stop_daemon_if_running().map_err(|error| error.to_string())? {
        eprintln!("Stopped the running daemon to clear cached provider state.");
    }
    Ok(())
}

fn replace_provider_key(cfg: &Config) -> Result<(), String> {
    let (name, preset) = selected_provider(cfg)?;
    let key = crate::setup::prompt_api_key(&name).map_err(|error| error.to_string())?;
    crate::setup::explain_keychain_access(&name);
    crate::auth::login(&preset, &key, true).map_err(|error| error.to_string())?;
    let stopped = crate::auth::stop_daemon_if_running().map_err(|error| error.to_string())?;
    eprintln!("Stored a replacement key for `{name}` in the system keychain.");
    if stopped {
        eprintln!(
            "Stopped the running daemon to discard its cached key; it will restart on the next request if auto-start is enabled."
        );
    }
    Ok(())
}

fn remove_provider_key(cfg: &Config) -> Result<(), String> {
    let (name, preset) = selected_provider(cfg)?;
    crate::auth::delete(&name).map_err(|error| error.to_string())?;
    let stopped = crate::auth::stop_daemon_if_running().map_err(|error| error.to_string())?;
    eprintln!("Removed the stored key for `{name}`.");
    if let Some(env_key) = preset.env_key {
        eprintln!("If `{env_key}` is set, termplete can still use that environment key.");
    }
    if stopped {
        eprintln!(
            "Stopped the running daemon to discard its cached key; it will restart on the next request if auto-start is enabled."
        );
    }
    Ok(())
}

fn capture_shortcut(label: &str) -> Result<Option<String>, String> {
    let captured = {
        let _raw = crate::shellutil::RawTerminal::enter(true).map_err(|e| e.to_string())?;
        eprintln!(
            "\n{label}\nPress the shortcut now. Escape cancels. Legacy terminals may report Ctrl+Shift+Z as Ctrl+Z."
        );
        std::io::stderr().flush().map_err(|e| e.to_string())?;
        read_terminal_key()
    };
    match captured {
        Ok(Some(spec)) if spec == "^Z" => {
            eprintln!(
                "Captured Ctrl+Z. If you pressed Ctrl+Shift+Z, your terminal reported the same key."
            );
            Ok(Some(spec))
        }
        Ok(Some(spec)) if spec == "^I" && label.starts_with("Accept key") => {
            eprintln!("Tab is reserved; the accept shortcut will use Right Arrow.");
            Ok(Some("^[[C".into()))
        }
        Ok(Some(spec)) if spec == "^I" => {
            eprintln!("Tab is reserved for opening and cycling options; shortcut not changed.");
            Ok(None)
        }
        Ok(None) => {
            eprintln!("Shortcut capture cancelled.");
            Ok(None)
        }
        Ok(spec) => Ok(spec),
        Err(error) => {
            eprintln!("{error}; shortcut not changed.");
            Ok(None)
        }
    }
}

fn read_terminal_key() -> Result<Option<String>, String> {
    let first = read_stdin_byte().map_err(|e| e.to_string())?;
    match first {
        0x1b => match read_stdin_byte_timeout(100).map_err(|e| e.to_string())? {
            None => Ok(None),
            Some(b'[') => read_csi_key(),
            Some(b'O') => {
                let final_byte = read_stdin_byte_timeout(150)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "Incomplete terminal key sequence".to_string())?;
                if matches!(final_byte, b'A'..=b'D' | b'H' | b'F' | b'P'..=b'S') {
                    Ok(Some(format!("^[O{}", final_byte as char)))
                } else {
                    Err("Unsupported terminal key sequence".into())
                }
            }
            Some(byte @ 0x20..=0x7e) => Ok(Some(format!("^[{}", byte as char))),
            Some(_) => Err("Unsupported terminal key sequence".into()),
        },
        byte => control_or_printable_key(byte),
    }
}

fn read_csi_key() -> Result<Option<String>, String> {
    let mut sequence = Vec::with_capacity(12);
    loop {
        if sequence.len() >= 64 {
            return Err("Terminal key sequence is too long".into());
        }
        let byte = read_stdin_byte_timeout(150)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Incomplete terminal key sequence".to_string())?;
        sequence.push(byte);
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
        if !(0x20..=0x3f).contains(&byte) {
            return Err("Unsupported terminal key sequence".into());
        }
    }
    let body = std::str::from_utf8(&sequence)
        .map_err(|_| "Unsupported terminal key sequence".to_string())?;
    csi_to_shell_spec(body).map(Some)
}

fn csi_to_shell_spec(body: &str) -> Result<String, String> {
    let final_byte = body
        .as_bytes()
        .last()
        .copied()
        .ok_or_else(|| "Incomplete terminal key sequence".to_string())?;
    let params = &body[..body.len() - 1];
    let supported = match final_byte {
        b'A' | b'B' | b'C' | b'D' => params.is_empty() || valid_modified_arrow(params),
        b'H' | b'F' => params.is_empty(),
        b'Z' => params.is_empty() || params == "1;2",
        b'~' => valid_tilde_key(params),
        b'u' => valid_csi_u_key(params),
        _ => false,
    };
    if !supported {
        return Err("Unsupported terminal key sequence".into());
    }
    Ok(format!("^[[{body}"))
}

fn valid_modified_arrow(params: &str) -> bool {
    let Some((key, modifier)) = params.split_once(';') else {
        return false;
    };
    key == "1"
        && modifier
            .parse::<u8>()
            .is_ok_and(|modifier| (2..=8).contains(&modifier))
}

fn valid_tilde_key(params: &str) -> bool {
    if matches!(params, "1" | "2" | "3" | "4" | "5" | "6") {
        return true;
    }
    let parts: Vec<_> = params.split(';').collect();
    if parts.len() != 3 || parts[0] != "27" {
        return false;
    }
    let Ok(modifier) = parts[1].parse::<u8>() else {
        return false;
    };
    let Ok(key) = parts[2].parse::<u32>() else {
        return false;
    };
    (2..=8).contains(&modifier)
        && (key == 9 || char::from_u32(key).is_some_and(|ch| ch.is_ascii_graphic()))
}

fn valid_csi_u_key(params: &str) -> bool {
    let Some((key, modifier)) = params.split_once(';') else {
        return false;
    };
    let Ok(modifier) = modifier.parse::<u8>() else {
        return false;
    };
    let Ok(key) = key.parse::<u32>() else {
        return false;
    };
    (1..=8).contains(&modifier)
        && (key == 9 || char::from_u32(key).is_some_and(|ch| ch.is_ascii_graphic()))
}

fn control_or_printable_key(byte: u8) -> Result<Option<String>, String> {
    let spec = match byte {
        0 => "^@".to_string(),
        1..=26 => format!("^{}", (b'A' + byte - 1) as char),
        28 => "^\\".to_string(),
        29 => "^]".to_string(),
        30 => "^^".to_string(),
        31 => "^_".to_string(),
        127 => "^?".to_string(),
        0x20..=0x7e => (byte as char).to_string(),
        _ => return Err("Unsupported key; use an ASCII key or supported terminal shortcut".into()),
    };
    Ok(Some(spec))
}

fn read_stdin_byte() -> std::io::Result<u8> {
    loop {
        let mut byte = 0u8;
        let count = unsafe { libc::read(libc::STDIN_FILENO, &mut byte as *mut u8 as *mut _, 1) };
        if count == 1 {
            return Ok(byte);
        }
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "terminal closed during shortcut capture",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn read_stdin_byte_timeout(timeout_ms: i32) -> std::io::Result<Option<u8>> {
    loop {
        let mut poll = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
        if ready == 0 {
            return Ok(None);
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return read_stdin_byte().map(Some);
        }
    }
}

/// Toggle exclusions; edits are committed with the rest of Settings.
fn terminal_apps(cfg: &mut Config, interactive: bool, term: &Term) -> Result<(), String> {
    loop {
        let mut apps: Vec<String> = crate::terminal::APPS
            .iter()
            .map(|(id, _)| (*id).into())
            .collect();
        for name in &cfg.disabled_terminals {
            let name = crate::terminal::normalize(name);
            if !apps.contains(&name) {
                apps.push(name);
            }
        }
        if let Some(name) = crate::terminal::current() {
            if !apps.contains(&name) {
                apps.push(name);
            }
        }
        let mut labels: Vec<_> = apps
            .iter()
            .map(|id| {
                format!(
                    "{}  [{}]",
                    crate::terminal::label(id),
                    onoff(!crate::terminal::disabled(&cfg.disabled_terminals, id))
                )
            })
            .collect();
        labels.extend(["Add terminal...".into(), "Back".into()]);
        let Some(index) = crate::menu::select("Use TermPlete in (select to toggle)", &labels, 0)
            .map_err(|e| e.to_string())?
        else {
            return Ok(());
        };
        if index == apps.len() + 1 {
            return Ok(());
        }
        if index == apps.len() {
            let name = read_line_with_default(
                "Terminal app (TERM_PROGRAM) to disable",
                "",
                interactive,
                term,
            )?;
            for name in crate::terminal::parse_list(&name) {
                if !crate::terminal::disabled(&cfg.disabled_terminals, &name) {
                    cfg.disabled_terminals.push(name);
                }
            }
        } else {
            let name = &apps[index];
            if crate::terminal::disabled(&cfg.disabled_terminals, name) {
                cfg.disabled_terminals
                    .retain(|id| crate::terminal::normalize(id) != *name);
            } else {
                cfg.disabled_terminals.push(name.clone());
            }
        }
    }
}

fn read_line(prompt: &str) -> Result<String, String> {
    let term = Term::stderr();
    if !crate::menu::interactive() {
        use std::io::Write;
        eprint!("{prompt}: ");
        std::io::stderr().flush().map_err(|e| e.to_string())?;
        let mut buf = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut buf)
            .map_err(|e| e.to_string())?;
        return Ok(buf.trim_end_matches(['\n', '\r']).to_string());
    }
    let input: String = Input::new()
        .with_prompt(prompt)
        .interact_text_on(&term)
        .map_err(|e| {
            crate::setup::show_cursor();
            e.to_string()
        })?;
    Ok(input)
}

fn read_line_with_default(
    prompt: &str,
    default: &str,
    interactive: bool,
    term: &Term,
) -> Result<String, String> {
    if !interactive {
        let answer = read_line(prompt)?;
        return Ok(if answer.is_empty() {
            default.to_string()
        } else {
            answer
        });
    }
    let _input_mode = crate::shellutil::RawTerminal::enter(true).map_err(|e| e.to_string())?;
    let answer: String = Input::new()
        .with_prompt(prompt)
        .default(default.to_string())
        .interact_text_on(term)
        .map_err(|e| {
            crate::setup::show_cursor();
            e.to_string()
        })?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn helper_switch_preserves_the_selected_strategy_and_privacy_is_explicit() {
        let mut c = cfg();
        apply(&Field::DecisionEnabled, "on", &mut c).unwrap();
        assert!(c.decision.active());
        assert_eq!(c.decision.strategy, DecisionStrategy::Hybrid);
        apply(&Field::DecisionStrategy, "decision", &mut c).unwrap();
        let selected = c.decision.clone();
        apply(&Field::DecisionEnabled, "off", &mut c).unwrap();
        assert!(!c.decision.active());
        assert_eq!(c.decision.strategy, selected.strategy);
        assert_eq!(c.decision.model, selected.model);
        apply(&Field::DecisionEnabled, "on", &mut c).unwrap();
        assert!(c.decision.active());
        assert_eq!(c.decision.strategy, DecisionStrategy::Decision);
        apply(&Field::SensitiveContext, "decision", &mut c).unwrap();
        assert_eq!(c.privacy.sensitive_context, SensitiveContext::Decision);
        assert!(apply(&Field::SensitiveContext, "send-everything", &mut c).is_err());
    }

    #[test]
    fn suggestion_scope_can_switch_between_project_folder_and_global_history() {
        let mut c = cfg();
        assert_eq!(current(&Field::SuggestionScope, &c), "folder");
        assert!(fields().contains(&Field::SuggestionScope));
        assert!(Field::SuggestionScope
            .label(&c)
            .contains("Current project folder"));
        apply(&Field::SuggestionScope, "global", &mut c).unwrap();
        assert_eq!(c.context.suggestion_scope, SuggestionScope::Global);
        assert!(Field::SuggestionScope.label(&c).contains("Global"));
        assert_eq!(name_of(&Field::SuggestionScope), "context.suggestion_scope");
        apply(&Field::SuggestionScope, "project", &mut c).unwrap();
        assert_eq!(c.context.suggestion_scope, SuggestionScope::Folder);
        assert!(apply(&Field::SuggestionScope, "workspace", &mut c).is_err());
        let restored: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(restored.context.suggestion_scope, SuggestionScope::Folder);
    }

    #[test]
    fn decision_settings_parse_strategy_and_validate_values() {
        let mut c = cfg();
        assert_eq!(Field::DecisionSettings.label(&c), "Decision helper: off");
        apply(&Field::DecisionStrategy, "decision + ai", &mut c).unwrap();
        assert_eq!(c.decision.strategy, DecisionStrategy::Hybrid);
        apply(&Field::DecisionProtocol, "system one / jev", &mut c).unwrap();
        assert_eq!(c.decision.protocol, DecisionProtocol::SystemOne);
        apply(&Field::DecisionProvider, "jev", &mut c).unwrap();
        apply(&Field::DecisionModel, "jev-latest", &mut c).unwrap();
        apply(
            &Field::DecisionBaseUrl,
            " https://judge.example/v1/ ",
            &mut c,
        )
        .unwrap();
        apply(&Field::DecisionTimeout, "10000", &mut c).unwrap();
        apply(&Field::DecisionLocalOnly, "off", &mut c).unwrap();
        apply(
            &Field::DecisionPrivatePaths,
            "/private,/secrets, ,/home/user/.ssh",
            &mut c,
        )
        .unwrap();

        assert_eq!(c.decision.provider, "jev");
        assert_eq!(current(&Field::DecisionProtocol, &c), "system-one");
        assert_eq!(
            c.decision.base_url.as_deref(),
            Some("https://judge.example/v1/")
        );
        assert_eq!(c.decision.timeout_ms, 10_000);
        assert!(!c.decision.local_only);
        assert_eq!(
            c.decision.private_paths,
            vec![
                "/private".to_string(),
                "/secrets".to_string(),
                "/home/user/.ssh".to_string(),
            ]
        );
        assert!(apply(&Field::DecisionTimeout, "99", &mut c).is_err());
        assert!(apply(&Field::DecisionStrategy, "automatic", &mut c).is_err());
        assert!(apply(&Field::DecisionProtocol, "native-ish", &mut c).is_err());
    }

    #[test]
    fn provider_edits_restore_each_selected_model() {
        let mut c = cfg();
        apply(&Field::Provider, "openai", &mut c).unwrap();
        apply(&Field::Model, "openai-choice", &mut c).unwrap();
        apply(&Field::Provider, "ollama", &mut c).unwrap();
        assert_eq!(c.model, None);
        apply(&Field::Model, "local-choice", &mut c).unwrap();
        apply(&Field::Provider, "openai", &mut c).unwrap();
        assert_eq!(c.model.as_deref(), Some("openai-choice"));
        apply(&Field::Provider, "openai", &mut c).unwrap();
        assert_eq!(c.model.as_deref(), Some("openai-choice"));
        apply(&Field::Provider, "ollama", &mut c).unwrap();
        assert_eq!(c.model.as_deref(), Some("local-choice"));
    }

    #[test]
    fn ordinary_provider_picker_omits_the_decision_only_jev_wire() {
        let providers = completion_provider_names(&cfg());
        assert!(providers.iter().any(|name| name == "ollama"));
        assert!(!providers.iter().any(|name| name == "jev"));
    }

    #[test]
    fn every_field_has_a_name_and_a_label() {
        let c = cfg();
        for f in fields() {
            assert!(!name_of(&f).is_empty(), "{f:?} has no name");
            assert!(!f.label(&c).is_empty(), "{f:?} has no label");
        }
    }

    #[test]
    fn enabled_setting_can_be_paused_and_restored() {
        let mut c = cfg();
        assert!(apply(&Field::Enabled, "false", &mut c).unwrap());
        assert_eq!(current(&Field::Enabled, &c), "false");
        assert!(apply(&Field::Enabled, "true", &mut c).unwrap());
        assert_eq!(current(&Field::Enabled, &c), "true");
    }

    #[test]
    fn field_names_are_unique() {
        let mut names: Vec<&str> = fields().iter().map(name_of).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate setting name");
    }

    #[test]
    fn provider_key_actions_are_visible_and_descriptive() {
        let mut c = cfg();
        c.provider = Some("openai".into());

        assert!(fields().contains(&Field::ReplaceProviderKey));
        assert!(fields().contains(&Field::RemoveProviderKey));
        assert_eq!(
            Field::ReplaceProviderKey.label(&c),
            "Replace API key for openai"
        );
        assert_eq!(
            Field::RemoveProviderKey.label(&c),
            "Remove stored key for openai"
        );
        assert!(selected_provider(&c).is_ok());
    }

    #[test]
    fn provider_key_actions_require_a_selected_keyed_provider() {
        let mut c = cfg();
        c.provider = None;
        assert!(selected_provider(&c)
            .unwrap_err()
            .contains("Choose a provider"));

        c.provider = Some("ollama".into());
        assert!(selected_provider(&c)
            .unwrap_err()
            .contains("does not use an API key"));
    }

    #[test]
    fn terminal_exclusions_can_be_edited_saved_and_cleared() {
        let mut c: Config = toml::from_str("mode = 'aggressive'").unwrap();
        assert!(c.disabled_terminals.is_empty());
        apply(
            &Field::DisabledTerminals,
            "WarpTerminal, warp, iTerm.app",
            &mut c,
        )
        .unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(current(&Field::DisabledTerminals, &restored), "warp,iterm");
        apply(&Field::DisabledTerminals, "", &mut c).unwrap();
        assert!(c.disabled_terminals.is_empty());
    }

    #[test]
    fn mode_accepts_canonical_names_and_legacy_aliases() {
        for (input, want) in [
            ("automatic", Mode::Aggressive),
            ("on-demand", Mode::Subtle),
            ("aggressive", Mode::Aggressive),
            ("eager", Mode::Aggressive),
            ("subtle", Mode::Subtle),
            ("off", Mode::Off),
            ("EAGER", Mode::Aggressive),
        ] {
            let mut c = cfg();
            apply(&Field::Mode, input, &mut c).unwrap();
            assert_eq!(c.mode, want, "{input}");
        }
        let mut c = cfg();
        assert!(apply(&Field::Mode, "turbo", &mut c).is_err());
    }

    #[test]
    fn tab_cannot_remain_the_accept_key() {
        let mut c = cfg();
        apply(&Field::AcceptKey, "^I", &mut c).unwrap();
        assert_eq!(c.keys.accept, "^[[C");

        apply(&Field::AcceptKey, "M-y", &mut c).unwrap();
        assert_eq!(c.keys.accept, "^[y");
    }

    #[test]
    fn mode_switches_between_on_demand_and_automatic() {
        let mut c = Config {
            mode: Mode::Subtle,
            ..Config::default()
        };
        assert_eq!(c.mode, Mode::Subtle);
        apply(&Field::Mode, "automatic", &mut c).unwrap();
        assert_eq!(c.mode, Mode::Aggressive);
        apply(&Field::Mode, "off", &mut c).unwrap();
        assert_eq!(c.mode, Mode::Off);
    }

    #[test]
    fn booleans_parse_natural_spellings() {
        for (input, want) in [("on", true), ("off", false), ("yes", true), ("0", false)] {
            let mut c = cfg();
            apply(&Field::Animations, input, &mut c).unwrap();
            assert_eq!(c.style.animations, want, "{input}");
        }
        let mut c = cfg();
        assert!(apply(&Field::Spinner, "maybe", &mut c).is_err());
    }

    #[test]
    fn numbers_reject_garbage() {
        for f in [
            Field::MinChars,
            Field::Debounce,
            Field::HistoryEntries,
            Field::AnimationFrames,
        ] {
            let mut c = cfg();
            assert!(
                apply(&f, "lots", &mut c).is_err(),
                "{f:?} should reject garbage"
            );
        }
    }

    #[test]
    fn keys_are_stored_and_normalized_on_display() {
        let mut c = cfg();
        apply(&Field::AcceptKey, "M-y", &mut c).unwrap();
        assert_eq!(c.keys.accept, "^[y");
        assert!(Field::AcceptKey.label(&c).contains("Alt+Y"));
    }

    #[test]
    fn shortcut_names_are_readable_and_tab_is_reserved() {
        let mut c = cfg();
        apply(&Field::RevealKey, "Ctrl+G", &mut c).unwrap();
        apply(&Field::AcceptKey, "Right Arrow", &mut c).unwrap();
        apply(&Field::CycleKey, "Shift+Tab", &mut c).unwrap();
        assert!(Field::RevealKey.label(&c).contains("Ctrl+G"));
        assert!(Field::AcceptKey.label(&c).contains("Right Arrow"));
        assert!(Field::CycleKey.label(&c).contains("Shift+Tab"));
        assert!(apply(&Field::CycleKey, "Tab", &mut c).is_err());
        assert!(apply(&Field::RevealKey, "Tab", &mut c).is_err());
        apply(&Field::AcceptKey, "Tab", &mut c).unwrap();
        assert_eq!(c.keys.accept, "^[[C");
    }

    #[test]
    fn shortcut_reset_restores_all_defaults() {
        let mut c = cfg();
        c.keys.reveal = "x".into();
        c.keys.accept = "y".into();
        c.keys.cycle = "z".into();
        c.keys.dismiss = "d".into();
        c.keys.toggle = "t".into();
        apply(&Field::ResetShortcuts, "", &mut c).unwrap();
        assert_eq!(c.keys, KeyBindings::default());
        assert_eq!(name_of(&Field::ResetShortcuts), "reset_shortcuts");
    }

    #[test]
    fn csi_capture_keeps_complete_supported_sequences_and_rejects_others() {
        assert_eq!(csi_to_shell_spec("9;2u").unwrap(), "^[[9;2u");
        assert_eq!(csi_to_shell_spec("1;2Z").unwrap(), "^[[1;2Z");
        assert_eq!(csi_to_shell_spec("27;2;9~").unwrap(), "^[[27;2;9~");
        assert_eq!(csi_to_shell_spec("27;6;90~").unwrap(), "^[[27;6;90~");
        assert_eq!(csi_to_shell_spec("122;6u").unwrap(), "^[[122;6u");
        assert!(csi_to_shell_spec("999;6u").is_err());
        assert!(csi_to_shell_spec("8;9u").is_err());
    }

    #[test]
    fn exit_reports_no_change() {
        let mut c = cfg();
        assert!(!apply(&Field::Exit, "", &mut c).unwrap());
    }

    #[test]
    fn labels_show_current_values() {
        let c = Config {
            mode: Mode::Aggressive,
            ..Config::default()
        };
        assert!(Field::Mode.label(&c).contains("Automatic"));
        assert!(Field::AcceptKey
            .label(&c)
            .contains("Right Arrow by default"));
        assert!(Field::Animations.label(&c).contains("on"));
        let on_demand = Config {
            mode: Mode::Subtle,
            ..Config::default()
        };
        assert_eq!(Field::Mode.label(&on_demand), "Mode: On demand");
        assert_eq!(name_of(&Field::EagerColor), "automatic_color");
        let c2 = Config {
            style: crate::config::StyleConfig {
                animations: false,
                ..Default::default()
            },
            ..Config::default()
        };
        assert!(Field::Animations.label(&c2).contains("off"));
    }
}
