//! Provider setup wizard. Prompts and picker chrome share stderr; dumb terminals
//! and redirected streams use numbered choices without terminal escapes.

use dialoguer::console::{truncate_str, Key, Term};
use dialoguer::Input;

use crate::auth;
use crate::config::{Config, ProviderPreset};

/// Everything the wizard learned, ready to be applied.
#[derive(Default)]
pub struct Choices {
    pub provider: Option<String>,
    /// Base URL, editable so a user can point at a gateway or self-hosted proxy.
    pub base_url: Option<String>,
    /// Model id, typed by the user: provider catalogues move faster than this
    /// binary does, so guessing here is worse than asking.
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// A user-defined OpenAI-compatible provider to add to the config.
    pub custom: Option<CustomProvider>,
}

/// A provider the user defines from scratch: any OpenAI-compatible endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct CustomProvider {
    pub name: String,
    pub base_url: String,
    pub env_key: Option<String>,
}

macro_rules! say {
    () => { eprintln!() };
    ($($arg:tt)*) => { eprintln!($($arg)*) };
}

/// Diagnostic breadcrumbs, shown only when `TERMPLETE_SETUP_DEBUG` is set.
fn debug(msg: &str) {
    if std::env::var_os("TERMPLETE_SETUP_DEBUG").is_some() {
        eprintln!("[tp-setup] {msg}");
    }
}

/// Make the cursor visible again, and reset SGR.
///
/// Writes straight to the file descriptor rather than through Rust's `stdout`.
/// That matters on the paths where we are called: `console` has the terminal in
/// raw mode and its `LineWriter` may be mid-teardown, so a buffered write can be
/// lost exactly when it is needed most. The sequence is harmless to repeat.
pub fn show_cursor() {
    restore_terminal();
    if !interactive() {
        return;
    }
    const RESTORE: &[u8] = b"\x1b[?25h\x1b[0m";
    // SAFETY: a plain `write(2)` to stderr with a static byte string. This is
    // async-signal-safe, which is why the signal handler does the same.
    unsafe {
        libc::write(libc::STDERR_FILENO, RESTORE.as_ptr().cast(), RESTORE.len());
    }
}

static ORIGINAL_TERMINAL: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

fn restore_terminal() {
    if let Some(saved) = ORIGINAL_TERMINAL.get() {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
        }
    }
}

/// Restore cursor visibility for interrupted prompts and panics.
/// The picker handles Ctrl+C and Ctrl+D as ordinary cancellation keys.
pub fn install_cursor_guard() {
    unsafe {
        let mut saved = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) == 0 {
            let _ = ORIGINAL_TERMINAL.set(saved);
        }
    }
    // Panic path first, so a bug in the wizard still restores the cursor.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        show_cursor();
        previous(info);
    }));

    // ctrlc handles SIGINT, which is the common case.
    let _ = ctrlc::set_handler(|| {
        restore_terminal();
        show_cursor();
        std::process::exit(130);
    });

    // SIGTERM and SIGHUP arrive on `kill` and terminal close.
    extern "C" fn restore_and_exit(signum: libc::c_int) {
        restore_terminal();
        show_cursor();
        // `_exit`, not `exit`: this is signal context and must not run atexit
        // handlers or flush locked buffers.
        unsafe { libc::_exit(signum) }
    }
    // Bind through a typed fn pointer first: casting the item directly to the
    // `sighandler_t` alias is a function-item cast, which is not a documented
    // conversion.
    let handler: extern "C" fn(libc::c_int) = restore_and_exit;
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            libc::signal(sig, handler as libc::sighandler_t);
        }
    }
}

fn interactive() -> bool {
    crate::menu::interactive()
}

/// Build the provider list without touching the keychain.
///
/// This runs *before* the selector is drawn, so a keychain that is locked or slow
/// would stall the very first prompt — which reads as "the wizard does nothing".
/// Credential status is therefore not consulted here; it is looked up for the one
/// provider the user actually picks, after the list is already on screen.
fn provider_rows(cfg: &Config) -> Vec<(String, ProviderPreset, String)> {
    let mut rows: Vec<(String, ProviderPreset, String)> = cfg
        .provider_table()
        .into_iter()
        .filter(|(_, preset)| preset.wire != crate::config::Wire::Jev)
        .map(|(name, p)| {
            let status = if p.wire == crate::config::Wire::OpenAiAcp {
                "sign in via Codex".to_string()
            } else if p.no_auth {
                "local, no key needed".to_string()
            } else if p
                .env_key
                .as_deref()
                .is_some_and(|v| std::env::var_os(v).is_some())
            {
                "key from environment".to_string()
            } else {
                "hosted".to_string()
            };
            (name, p, status)
        })
        .collect();

    // Current provider first so re-running the wizard is a no-op.
    if let Some(active) = cfg.provider.as_deref() {
        rows.sort_by_key(|(name, _, _)| u8::from(name.as_str() != active));
    }
    rows
}

/// Run the wizard, returning what the user chose.
pub fn run() -> crate::Result<Choices> {
    let cfg = Config::load()?;
    let mut choices = Choices::default();

    debug(&format!(
        "start: TERM={:?} interactive={} rows={}",
        std::env::var("TERM").unwrap_or_default(),
        interactive(),
        usize::from(Term::stderr().size().0)
    ));
    say!();
    say!("  termplete setup");
    say!("  --------------------------------------------");

    // --- 1. provider ---------------------------------------------------------
    let rows = provider_rows(&cfg);
    if rows.is_empty() {
        say!("  No providers are configured.");
        return Ok(choices);
    }

    let mut labels: Vec<String> = rows
        .iter()
        .map(|(_, preset, status)| format!("{:<24} {status}", preset.display_name()))
        .collect();
    labels.push("+ Custom OpenAI-compatible provider — enter base URL and API key".into());
    debug(&format!("{} providers, labels ready", labels.len()));
    say!();
    say!("  Step 1 of 4 · Which provider?");
    debug(&format!(
        "entering selector (interactive={})",
        interactive()
    ));
    let Some(index) = crate::menu::select("Provider", &labels, 0)? else {
        return Err(crate::Error::Auth("setup cancelled".into()));
    };

    if index >= rows.len() {
        return run_custom(&cfg);
    }

    let (name, preset, _) = &rows[index];
    choices.provider = Some(name.clone());
    say!("  > {name}");

    if preset.wire == crate::config::Wire::OpenAiAcp {
        say!();
        say!("  ChatGPT sign-in is managed by OpenAI Codex.");
        say!("  Setup will open the Codex login flow; the model selected in Codex is used by default.");
        choices.model = Some(cfg.effective_model(preset));
        return Ok(choices);
    }

    // --- 2. api key ----------------------------------------------------------
    say!();
    say!("  Step 2 of 4 · API key");
    if !preset.no_auth {
        explain_keychain_access(name);
    }
    if preset.no_auth {
        say!("  `{name}` runs locally and needs no key.");
        say!("  Run the server's own list command for model ids, e.g. `ollama list`.");
    } else if auth::is_logged_in(name) {
        say!("  A key for `{name}` is already in your keychain — keeping it.");
    } else {
        choices.api_key = Some(ask_key(name)?);
    }

    // --- 3. base url ---------------------------------------------------------
    say!();
    say!("  Step 3 of 4 · Base URL");
    say!(
        "  Press enter to keep {}, or type a different endpoint",
        preset.base_url
    );
    let url = ask_line("base URL", &preset.base_url)?;
    choices.base_url = Some(url);

    // --- 4. model ------------------------------------------------------------
    say!();
    say!("  Step 4 of 4 · Model");
    say!("  Type the model id exactly as your provider lists it.");
    if !preset.model.is_empty() {
        say!("  (preset suggestion: {})", preset.model);
    }
    let previous_model = cfg.effective_model(preset);
    let model = ask_line("", &previous_model)?;
    if model.is_empty() {
        return Err(crate::Error::Auth(
            "a model id is required — re-run `tp setup` and enter one".into(),
        ));
    }
    choices.model = Some(model);

    Ok(choices)
}

/// Ask a free-text question; `prompt` labels it, `default` (if any) pre-fills it.
fn ask_line(prompt: &str, default: &str) -> crate::Result<String> {
    let prompt = if prompt.is_empty() && !default.is_empty() {
        format!("[{default}]")
    } else if prompt.is_empty() {
        "model id".to_string()
    } else {
        prompt.to_string()
    };
    if interactive() {
        // Own the input mode before displaying the text prompt, so Ctrl+C
        // cannot arrive between a visible prompt and raw-mode initialization.
        let _input_mode = crate::shellutil::RawTerminal::enter(true)?;
        let term = Term::stderr();
        let answer = if default.is_empty() {
            Input::new()
                .with_prompt(format!("  {prompt}"))
                .interact_text_on(&term)
        } else {
            Input::new()
                .with_prompt(format!("  {prompt}"))
                .default(default.to_string())
                .interact_text_on(&term)
        };
        answer.map_err(|e| {
            show_cursor();
            crate::Error::Auth(e.to_string())
        })
    } else {
        if !default.is_empty() {
            say!("  [{default}]");
        }
        Ok(read_line_from_stdin().unwrap_or_else(|| default.to_string()))
    }
}

/// True when the endpoint points at this machine, so no credential is needed.
///
/// Matches on the host rather than a substring, so a remote name like
/// `notlocalhost.example.com` is not mistaken for a local server.
fn is_local_url(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    // Host runs up to the port, path or IPv6 bracket.
    let host = match rest.split_once(']') {
        Some((host, _)) => host.trim_start_matches('['),
        None => rest.split([':', '/']).next().unwrap_or(""),
    };
    let host = host.to_ascii_lowercase();
    matches!(host.as_str(), "127.0.0.1" | "0.0.0.0" | "::1" | "localhost")
}

/// Collect the fields for a user-defined OpenAI-compatible provider.
fn ask_custom_provider() -> crate::Result<CustomProvider> {
    say!();
    say!("  Step 1a · Custom provider");
    say!("  Any endpoint that speaks the OpenAI chat-completions API: a gateway,");
    say!("  a self-hosted proxy, or a vendor not on the list.");
    let base_url = ask_line_required("base URL (e.g. https://gateway.internal/v1)", "base URL")?;
    validate_custom_url(&base_url)?;
    say!();
    let name = ask_line_required("provider name (e.g. my-provider)", "provider name")?
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '/', ':'], "-");

    say!();
    say!("  Step 1b · Environment variable for the key (optional)");
    say!("  Optional fallback; the API key entered next is stored in your keychain.");
    let env_key = ask_line("environment variable (optional)", "")?;
    let env_key = env_key.trim();
    let env_key = (!env_key.is_empty()).then(|| env_key.to_ascii_uppercase());

    say!("  > {name} at {base_url}");
    Ok(CustomProvider {
        name,
        base_url,
        env_key,
    })
}

fn validate_custom_url(base_url: &str) -> crate::Result<()> {
    let valid = reqwest::Url::parse(base_url).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    });
    if !valid {
        return Err(crate::Error::Auth(
            "use an HTTP(S) base URL without embedded credentials, query parameters or fragments"
                .into(),
        ));
    }
    Ok(())
}

/// Shared custom-provider flow for setup and Settings. No config or key is
/// written here; the caller persists the completed choices.
pub fn run_custom(cfg: &Config) -> crate::Result<Choices> {
    let custom = ask_custom_provider()?;
    if cfg.provider_table().contains_key(&custom.name) {
        return Err(crate::Error::Auth(
            "that provider name already exists; choose a unique name".into(),
        ));
    }
    let mut choices = Choices {
        provider: Some(custom.name.clone()),
        base_url: Some(custom.base_url.clone()),
        custom: Some(custom),
        ..Choices::default()
    };
    choices = ask_model_and_finish(&mut choices, None)?;
    let custom = choices.custom.as_ref().expect("custom provider supplied");
    if is_local_url(&custom.base_url) {
        say!("  That endpoint is local — no API key required.");
    } else {
        choices.api_key = Some(ask_key(&custom.name)?);
    }
    Ok(choices)
}

/// Ask a question that must be answered.
fn ask_line_required(question: &str, prompt: &str) -> crate::Result<String> {
    say!("  {question}");
    let answer = ask_line(prompt, "")?;
    if answer.trim().is_empty() {
        return Err(crate::Error::Auth(format!("a {prompt} is required")));
    }
    Ok(answer.trim().to_string())
}

/// Ask for the model id, then return the collected choices.
fn ask_model_and_finish(
    choices: &mut Choices,
    preset: Option<&ProviderPreset>,
) -> crate::Result<Choices> {
    say!();
    say!("  Model");
    say!("  Type the model id exactly as your provider lists it.");
    if let Some(p) = preset.filter(|p| !p.model.is_empty()) {
        say!("  (preset suggestion: {})", p.model);
    }
    let model = ask_line_required("model id", "model id")?;
    choices.model = Some(model);
    Ok(std::mem::take(choices))
}

/// Explain the scope of the platform credential-store prompt before accessing
/// a provider's saved key.
pub fn explain_keychain_access(provider: &str) {
    say!();
    say!("  termplete stores API keys locally in your system credential store.");
    say!("  It reads `{provider}`'s key when sending requests.");
    say!("  On macOS, Keychain may ask you to allow access to this key.");
    say!("  That approval applies to this item only, not your other Keychain items.");
}

/// Ask for an API key with visible, masked feedback on a TTY.
fn ask_key(provider: &str) -> crate::Result<String> {
    say!("  Paste the key from {provider}. It is stored in your system");
    say!("  keychain, never in the config file or your shell history.");
    prompt_api_key(provider)
}

/// Shared by setup and `tp auth login`: never echo the secret itself.
pub fn prompt_api_key(provider: &str) -> crate::Result<String> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        return read_line_from_stdin()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| crate::Error::Auth("no API key given".into()));
    }
    let _input_mode = crate::shellutil::RawTerminal::enter(true)?;
    let term = if std::io::stderr().is_terminal() {
        Term::stderr()
    } else if std::io::stdout().is_terminal() {
        Term::stdout()
    } else {
        return Err(crate::Error::Auth(
            "interactive key input needs a terminal; use --stdin".into(),
        ));
    };
    term.write_line(&format!("API key for {provider}"))?;
    term.write_line("Input is masked. Enter confirms; Ctrl+C cancels.")?;
    let mut key = String::new();
    let mut previous_width: usize = 0;
    loop {
        let width = usize::from(term.size().1).saturating_sub(1).max(1);
        let prefix = if width >= 16 { "API key: " } else { "Key: " };
        // Cap the mask so long pasted keys never wrap over the prompt.
        let mask = "*".repeat(
            key.chars()
                .count()
                .min(width.saturating_sub(prefix.len()).min(24)),
        );
        let row = truncate_str(&format!("{prefix}{mask}"), width, "").into_owned();
        term.write_str(&format!(
            "\r{row}{}\r{row}",
            " ".repeat(previous_width.saturating_sub(row.len()))
        ))?;
        term.flush()?;
        previous_width = row.len();
        match term.read_key_raw()? {
            Key::Enter if !key.trim().is_empty() => {
                term.write_line("")?;
                term.write_line("API key received.")?;
                return Ok(key.trim().to_string());
            }
            Key::Enter => {}
            Key::Backspace => {
                key.pop();
            }
            Key::Char('\u{15}') => key.clear(),
            Key::Char(c) if !c.is_control() => key.push(c),
            Key::Escape | Key::CtrlC | Key::Char('\u{4}') => {
                term.write_line("")?;
                return Err(crate::Error::Auth("API key entry cancelled".into()));
            }
            _ => {}
        }
    }
}

/// Write the chosen provider, base URL and model to the config file.
pub fn apply(cfg: &mut Config, choices: &Choices) -> crate::Result<()> {
    apply_in_memory(cfg, choices);
    cfg.save()?;
    Ok(())
}

/// Apply provider choices without saving; Settings saves the complete draft.
pub fn apply_in_memory(cfg: &mut Config, choices: &Choices) {
    if let Some(p) = &choices.provider {
        cfg.select_provider(p);
    }
    if let Some(m) = &choices.model {
        cfg.model = (!m.trim().is_empty()).then(|| m.clone());
    }

    // A user-defined provider is stored as a config entry, so it appears in the
    // list next run instead of being retyped.
    if let Some(custom) = &choices.custom {
        cfg.providers.retain(|p| p.name != custom.name);
        cfg.providers.push(ProviderPreset {
            name: custom.name.clone(),
            base_url: custom.base_url.clone(),
            model: choices.model.clone().unwrap_or_default(),
            // Only a local endpoint can be reached without a credential.
            no_auth: is_local_url(&custom.base_url),
            wire: crate::config::Wire::OpenAiCompat,
            env_key: custom.env_key.clone(),
            description: "Custom OpenAI-compatible endpoint".into(),
        });
    } else if let (Some(name), Some(url)) = (&choices.provider, &choices.base_url) {
        // A customised base URL shadows the built-in preset. An empty answer
        // means "keep the default" — writing that through would blank a working
        // endpoint.
        let url = url.trim();
        if !url.is_empty() {
            let preset = cfg.provider_table().get(name).cloned();
            if let Some(mut preset) = preset {
                if preset.base_url != url {
                    preset.base_url = url.to_string();
                    cfg.providers.retain(|p| p.name != *name);
                    cfg.providers.push(preset);
                }
            }
        }
    }
}

/// Store the key the user pasted.
pub fn store_key(provider: &str, key: &str, cfg: &Config) -> crate::Result<()> {
    let preset = cfg.provider(Some(provider))?;
    auth::login(&preset, key, true)?;
    Ok(())
}

/// The line to add to an rc file.
///
/// `eval "$(tp init --print <shell>)"` rather than `source <(...)`: process
/// substitution reads the script through a pipe, and a fast producer can exit
/// before the shell has read it, leaving an empty integration and no error.
pub fn integration_snippet(shell: &str) -> String {
    format!("# termplete — {shell} integration\neval \"$(tp init --print {shell})\"")
}

/// Read one line from stdin without dialoguer, used when there is no TTY.
///
/// The buffer is `thread_local` on purpose: each `stdin().lock()` is a fresh
/// `BufReader`, so locking per call would drop whatever it had already buffered
/// and silently skip the following line.
fn read_line_from_stdin() -> Option<String> {
    use std::cell::RefCell;
    use std::io::BufRead;

    thread_local! {
        static STDIN_BUF: RefCell<std::io::BufReader<std::io::Stdin>> =
            RefCell::new(std::io::BufReader::new(std::io::stdin()));
    }

    let mut buf = String::new();
    let read = STDIN_BUF.with(|r| r.borrow_mut().read_line(&mut buf));
    match read {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf.trim_end_matches(['\n', '\r']).to_string()),
    }
}

/// Where a shell's rc file lives.
pub fn rc_path(shell: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    Some(match shell {
        "zsh" => std::env::var_os("ZDOTDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or(home)
            .join(".zshrc"),
        "bash" => home.join(".bashrc"),
        _ => return None,
    })
}

/// Enable suggestions in every new interactive session of the selected shell.
pub fn install_integration(shell: &str) -> crate::Result<std::path::PathBuf> {
    use std::io::Write;
    let path =
        rc_path(shell).ok_or_else(|| crate::Error::Auth(format!("unsupported shell: {shell}")))?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let snippet = integration_snippet(shell);
    let command = snippet.lines().last().expect("integration command");
    if !existing.lines().any(|line| line.trim() == command) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        if !existing.is_empty() && !existing.ends_with('\n') {
            writeln!(file)?;
        }
        writeln!(file, "{snippet}")?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn custom(name: &str, url: &str) -> CustomProvider {
        CustomProvider {
            name: name.into(),
            base_url: url.into(),
            env_key: None,
        }
    }

    #[test]
    fn custom_url_requires_a_plain_http_endpoint() {
        for valid in ["https://example.com/v1", "http://127.0.0.1:11434/v1"] {
            assert!(validate_custom_url(valid).is_ok());
        }
        for invalid in [
            "not-a-url",
            "file:///tmp/model",
            "https://user:password@example.com/v1",
            "https://example.com/v1?key=secret",
        ] {
            assert!(validate_custom_url(invalid).is_err());
        }
    }

    #[test]
    fn custom_draft_keeps_the_key_out_of_configuration() {
        let mut cfg = Config::default();
        let choices = Choices {
            provider: Some("my-provider".into()),
            model: Some("my-model".into()),
            api_key: Some("synthetic-key-marker".into()),
            custom: Some(custom("my-provider", "https://example.com/v1")),
            ..Choices::default()
        };
        apply_in_memory(&mut cfg, &choices);
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(!serialized.contains("synthetic-key-marker"));
        let restored: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            restored.provider(Some("my-provider")).unwrap().wire,
            crate::config::Wire::OpenAiCompat
        );
        assert_eq!(
            restored
                .require_model(&restored.provider(Some("my-provider")).unwrap())
                .unwrap(),
            "my-model"
        );
    }

    #[test]
    fn detects_local_endpoints_that_need_no_key() {
        // A false negative here makes the wizard demand a key from a server that
        // has none, so cover the schemes and spellings users actually type.
        for url in [
            "http://127.0.0.1:1234/v1",
            "https://127.0.0.1:11434/v1",
            "http://127.0.0.1:11434/v1",
            "http://localhost:1234/v1",
            "https://localhost:1234/v1",
            "http://LOCALHOST:1234/v1",
            "http://0.0.0.0:8000/v1",
            "http://[::1]:1234/v1",
            "http://127.0.0.1",
        ] {
            assert!(is_local_url(url), "{url} should be local");
        }
        for url in [
            "https://api.openai.com/v1",
            "https://gateway.internal/v1",
            // A hostname that merely contains a local-ish word is remote.
            "https://notlocalhost.example.com/v1",
            "https://127.0.0.1.example.com/v1",
        ] {
            assert!(!is_local_url(url), "{url} should not be local");
        }
    }

    #[test]
    fn custom_provider_is_persisted_and_reusable() {
        let mut cfg = Config::default();
        let choices = Choices {
            provider: Some("my-gateway".into()),
            model: Some("some-model".into()),
            custom: Some(custom("my-gateway", "https://gateway.internal/v1")),
            ..Choices::default()
        };
        apply_in_memory(&mut cfg, &choices);

        let table = cfg.provider_table();
        let stored = table.get("my-gateway").expect("saved");
        assert_eq!(stored.base_url, "https://gateway.internal/v1");
        assert_eq!(cfg.model.as_deref(), Some("some-model"));
        assert!(!stored.no_auth, "a remote gateway still needs a key");
    }

    #[test]
    fn local_custom_provider_is_marked_no_auth() {
        let mut cfg = Config::default();
        let choices = Choices {
            provider: Some("my-local".into()),
            custom: Some(custom("my-local", "http://127.0.0.1:5000/v1")),
            ..Choices::default()
        };
        apply_in_memory(&mut cfg, &choices);
        assert!(cfg.provider_table()["my-local"].no_auth);
    }

    #[test]
    fn rerunning_the_wizard_replaces_rather_than_duplicates() {
        let mut cfg = Config::default();
        for url in ["https://a.internal/v1", "https://b.internal/v1"] {
            let choices = Choices {
                provider: Some("test-endpoint".into()),
                custom: Some(custom("test-endpoint", url)),
                ..Choices::default()
            };
            apply_in_memory(&mut cfg, &choices);
        }
        assert_eq!(cfg.providers.len(), 1, "redefining must not duplicate");
        assert_eq!(cfg.providers[0].base_url, "https://b.internal/v1");
    }

    #[test]
    fn custom_provider_does_not_shadow_a_builtin() {
        let mut cfg = Config::default();
        let choices = Choices {
            provider: Some("openai".into()),
            custom: Some(custom("openai", "https://proxy.internal/v1")),
            ..Choices::default()
        };
        apply_in_memory(&mut cfg, &choices);
        // The user's override wins, but it is one entry, not a duplicate.
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(
            cfg.provider_table()["openai"].base_url,
            "https://proxy.internal/v1"
        );
    }

    #[test]
    fn switching_setup_providers_keeps_models_and_custom_endpoints() {
        let mut cfg = Config::default();
        apply_in_memory(
            &mut cfg,
            &Choices {
                provider: Some("saved-gateway".into()),
                model: Some("gateway-model".into()),
                custom: Some(custom("saved-gateway", "https://gateway.internal/v1")),
                ..Choices::default()
            },
        );
        apply_in_memory(
            &mut cfg,
            &Choices {
                provider: Some("ollama".into()),
                model: Some("local-model".into()),
                ..Choices::default()
            },
        );
        apply_in_memory(
            &mut cfg,
            &Choices {
                provider: Some("saved-gateway".into()),
                ..Choices::default()
            },
        );
        assert_eq!(cfg.model.as_deref(), Some("gateway-model"));
        assert_eq!(
            cfg.provider(None).unwrap().base_url,
            "https://gateway.internal/v1"
        );
        assert_eq!(cfg.providers.len(), 1);
    }

    #[test]
    fn keeping_the_default_base_url_writes_no_override() {
        let mut cfg = Config::default();
        let choices = Choices {
            provider: Some("mistral".into()),
            base_url: Some(String::new()), // user pressed enter
            model: Some("m".into()),
            ..Choices::default()
        };
        apply_in_memory(&mut cfg, &choices);
        assert!(
            cfg.providers.is_empty(),
            "an empty URL must not blank the preset"
        );
        assert_eq!(
            cfg.provider_table()["mistral"].base_url,
            "https://api.mistral.ai/v1"
        );
    }
}
