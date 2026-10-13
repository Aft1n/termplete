//! `tp` — the termplete command line.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use termplete::client;
use termplete::config::{self, Config, KeyBindings, Mode, ProviderPreset};
use termplete::context::Store;
use termplete::protocol::{Request, Response};
use termplete::{auth, daemon, pty, shellutil, Error};
mod lifecycle_commands;

use lifecycle_commands::{is_live_daemon, service, ServiceCmd};

#[derive(Parser, Debug)]
#[command(
    name = "tp",
    version,
    about = "LLM-powered terminal autocomplete",
    long_about = "termplete suggests shell completions from your command history and \
                  output, refined by an LLM. Keys live in your system keychain."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

impl Cmd {
    /// Whether this command can hide the cursor and so needs it restored.
    fn renders_menu(&self) -> bool {
        matches!(self, Cmd::Setup | Cmd::Settings)
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Initialize a config file at ~/.termplete/config.toml
    Init(InitArgs),
    /// Manage provider API keys and sign-in
    Auth {
        #[command(subcommand)]
        command: AuthCmd,
    },
    /// Connect your AI provider
    Setup,
    /// Change mode, shortcuts and preferences
    Settings,
    /// Pause terminal integration and helper activity
    Stop,
    /// Resume terminal integration and helper activity
    Start,
    /// Start, stop or inspect the background daemon
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Print a completion for a line without touching the shell
    Suggest(SuggestArgs),
    /// Start the daemon in the background or foreground
    Daemon {
        #[arg(long)]
        foreground: bool,
    },
    /// Internal PTY wrapper for output capture
    Shell(ShellArgs),
    /// Show the recorded command context
    Context {
        /// Import an existing zsh/bash history file into command memory.
        #[arg(long)]
        import: Option<PathBuf>,
        /// Search command memory for a prefix without contacting the model.
        #[arg(long)]
        search: Option<String>,
    },
    /// Report daemon and configuration status
    Status,
    /// Record a command (and optionally its output) into the context store
    Record(RecordArgs),
    /// Learn an explicitly accepted completion without running it
    Feedback(FeedbackArgs),
    /// Emit a private command boundary for the terminal output observer.
    Frame {
        /// Random per-session marker, unavailable to child commands.
        #[arg(long)]
        token: String,
        #[arg(long, allow_hyphen_values = true)]
        command: String,
        #[arg(long)]
        cwd: String,
    },
    /// Read or change configuration values (see `tp settings` for a menu)
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Test the configured decision helper using synthetic context only
    #[command(subcommand)]
    Decision(DecisionCmd),
}

#[derive(Subcommand, Debug)]
enum DecisionCmd {
    /// Check connectivity, validated decisions and latency without reading history
    Check {
        /// Override the latency budget for this check (100–10000 ms)
        #[arg(long)]
        timeout_ms: Option<u64>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args, Debug)]
struct InitArgs {
    /// Overwrite an existing config file.
    #[arg(long)]
    force: bool,
    /// Print the shell integration script to stdout instead of writing config.
    #[arg(long)]
    print: Option<String>,
}

#[derive(clap::Args, Debug)]
struct RecordArgs {
    /// Observe pasted input without claiming execution or learning a usage.
    #[arg(long, conflicts_with_all = ["exit_code", "output"])]
    pending: bool,
    /// The command that was run.
    #[arg(last = true, required = true)]
    command: Vec<String>,
    /// Exit status of the command.
    #[arg(long)]
    exit_code: Option<i32>,
    /// Output the command produced, if captured.
    #[arg(long)]
    output: Option<String>,
    /// Directory in which the command started.
    #[arg(long)]
    cwd: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct FeedbackArgs {
    #[arg(long, allow_hyphen_values = true)]
    prefix: String,
    #[arg(last = true, required = true)]
    command: Vec<String>,
    /// Directory in which the suggestion was accepted.
    #[arg(long)]
    cwd: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum AuthCmd {
    /// Sign in to a provider or store its API key
    Login {
        /// Provider name, e.g. openai, groq, ollama, or openai-acp (ChatGPT subscription)
        #[arg(long)]
        provider: String,
        /// The key. Prefer --stdin or the interactive prompt to avoid shell history.
        #[arg(long, conflicts_with = "stdin")]
        key: Option<String>,
        /// Read the key from stdin.
        #[arg(long)]
        stdin: bool,
        /// Replace an existing key.
        #[arg(long)]
        force: bool,
        /// Make this the active provider.
        #[arg(long)]
        use_provider: bool,
    },
    /// Remove a stored key
    #[command(visible_alias = "remove")]
    Logout {
        #[arg(long)]
        provider: String,
    },
    /// List built-in and configured providers
    Providers,
    /// Show which providers have credentials and where they come from
    Status,
}

#[derive(clap::Args, Debug)]
struct SuggestArgs {
    /// The line to complete.
    line: String,
    /// Cursor byte offset into the line; defaults to the end.
    #[arg(long)]
    cursor: Option<usize>,
    /// Override the configured mode for this call.
    #[arg(long)]
    mode: Option<String>,
    /// Use local history only, never the model.
    #[arg(long)]
    local_only: bool,
    /// Print diagnostics alongside the suggestion.
    #[arg(long)]
    explain: bool,
    /// Animation frame index the shell is currently on; selects the shimmer
    /// phase for the reply.
    #[arg(long)]
    frame: Option<usize>,
    /// Machine-readable output.
    #[arg(long)]
    json: bool,
    /// Shell quoting: print only the ghost text, with no trailing newline.
    #[arg(long)]
    raw: bool,
    /// Print only the ghost text, unstyled, with no trailing newline.
    #[arg(long)]
    plain: bool,
    /// Print one candidate command per line, for shell completion functions.
    #[arg(long)]
    candidates: bool,
    /// Interpret the line as a human description of a command to generate.
    #[arg(long, conflicts_with = "local_only")]
    intent: bool,
    /// One-line reply for the native shell integration.
    #[arg(long)]
    shell_reply: bool,
}

#[derive(clap::Args, Debug)]
struct ShellArgs {
    /// Shell to run; defaults to $SHELL.
    #[arg(long)]
    shell: Option<String>,
    /// Run this command instead of an interactive shell.
    #[arg(long, short = 'c')]
    command: Option<String>,
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Print the effective configuration
    Show,
    /// Print one value, e.g. `tp config get mode`
    Get {
        #[arg(long)]
        key: String,
    },
    /// Set one value, e.g. `tp config set --key mode --value automatic`
    Set {
        #[arg(long)]
        key: String,
        #[arg(long)]
        value: String,
    },
    /// Print the config file path
    Path,
}

fn main() -> ExitCode {
    // Restore the default SIGPIPE disposition. Rust ignores SIGPIPE, so a closed
    // pipe turns into an `EPIPE` error and `println!` panics with "failed
    // printing to stdout". Restoring the default makes the process die quietly
    // like every other Unix tool, so `tp auth providers | head` just works.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    init_tracing();
    let cli = Cli::parse();
    // Only the interactive menus hide the cursor. Restoring it unconditionally
    // would append the escape bytes to every other command's stdout — which
    // corrupts `tp init --print`, and makes ordinary output look like it was
    // erased from the terminal.
    let drew_menu = cli.command.renders_menu();
    if drew_menu {
        termplete::setup::install_cursor_guard();
    }
    let result = run(cli.command);
    if drew_menu {
        termplete::setup::show_cursor();
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e}", termplete::BIN_NAME);
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    // Logs go to a file so they never corrupt the prompt; RUST_LOG still wins.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn,termplete=info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn run(cmd: Cmd) -> termplete::Result<()> {
    match cmd {
        Cmd::Init(args) => init(args),
        Cmd::Record(args) => record(args),
        Cmd::Feedback(args) => feedback(args),
        Cmd::Frame {
            command,
            cwd,
            token,
        } => {
            print!("{}", pty::frame_start(&command, &cwd, &token));
            Ok(())
        }
        Cmd::Auth { command } => auth_cmd(command),
        Cmd::Setup => setup_wizard(),
        Cmd::Settings => settings_menu(),
        Cmd::Stop => lifecycle_commands::stop(),
        Cmd::Start => lifecycle_commands::start(),
        Cmd::Suggest(args) => suggest(args),
        Cmd::Daemon { foreground } => {
            if foreground {
                daemon::serve()
            } else {
                daemon::spawn_background()?;
                println!("daemon started");
                Ok(())
            }
        }
        Cmd::Shell(args) => shell(args),
        Cmd::Context { import, search } => context(import, search),
        Cmd::Status => status(),
        Cmd::Service(cmd) => lifecycle_commands::service(cmd),
        Cmd::Config(cmd) => config_cmd(cmd),
        Cmd::Decision(DecisionCmd::Check { timeout_ms, json }) => decision_check(timeout_ms, json),
    }
}

// --- init ---------------------------------------------------------------------

fn init(args: InitArgs) -> termplete::Result<()> {
    if let Some(shell) = args.print {
        return print_shell_integration(&shell);
    }
    let path = config::config_path()?;
    if path.exists() && !args.force {
        return Err(Error::Config(config::ConfigError::Io {
            path: path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "{} already exists — pass --force to overwrite",
                    path.display()
                ),
            ),
        }));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, config::default_config_toml())?;
    println!("wrote {}", path.display());
    println!("next: {} auth login --provider openai", termplete::BIN_NAME);
    Ok(())
}

/// Print the shell integration snippet for `shell`.
fn print_shell_integration(shell: &str) -> termplete::Result<()> {
    let name = shell.trim().to_ascii_lowercase();
    if !matches!(name.as_str(), "zsh" | "bash") {
        return Err(Error::Config(config::ConfigError::Io {
            path: PathBuf::from(shell.to_string()),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("no integration for `{shell}` — supported: zsh, bash"),
            ),
        }));
    }
    if !Config::load()?.terminal_enabled() {
        return Ok(());
    }
    // Prefer a script on disk, so a user can tweak their own copy; fall back to
    // the copy embedded at compile time.
    if let Some(script) = shell_script_path(&name)
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        print!("{script}");
        return Ok(());
    }
    match embedded_shell_script(&name) {
        Some(script) => {
            print!("{script}");
            Ok(())
        }
        None => Err(Error::Config(config::ConfigError::Io {
            path: PathBuf::from(shell.to_string()),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no embedded integration for `{name}`"),
            ),
        })),
    }
}

/// The integration scripts, embedded at compile time so `tp init --print` works
/// from any install location without depending on a data directory.
fn embedded_shell_script(shell: &str) -> Option<&'static str> {
    match shell {
        "zsh" => Some(include_str!("../shells/termplete.plugin.zsh")),
        "bash" => Some(include_str!("../shells/termplete.plugin.bash")),
        _ => None,
    }
}

/// Locate an on-disk integration script, if the user ships one.
fn shell_script_path(shell: &str) -> Option<PathBuf> {
    // Must match the shipped filenames (`termplete.plugin.zsh`), or the on-disk
    // lookup silently misses and falls back to the embedded copy — so an edited
    // script appears to have no effect.
    let name = format!("termplete.plugin.{shell}");
    if let Some(from_env) = std::env::var_os("TERMPLETE_SHELLS") {
        let p = PathBuf::from(from_env).join(&name);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("shells").join(&name),
        dir.join("../share/termplete/shells").join(&name),
        dir.join("../../share/termplete/shells").join(&name),
    ]
    .into_iter()
    .find(|p| p.exists())
}

/// Record a command into the context store.
fn record(args: RecordArgs) -> termplete::Result<()> {
    let cfg = Config::load()?;
    if !cfg.terminal_enabled() {
        return Ok(());
    }
    let command = args.command.join(" ");
    if args.pending {
        // Native paste must never wait for a model or an unbounded connection.
        let _ = client::ask(
            Request::ObserveInput {
                id: 1,
                input: command,
                cwd: args
                    .cwd
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            },
            Duration::from_millis(200),
            true,
        );
        return Ok(());
    }
    let req = Request::Record {
        id: 1,
        command: command.clone(),
        cwd: args
            .cwd
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        exit_code: args.exit_code,
        output: args.output,
    };
    // A shell records in the background; never make the prompt wait on it.
    match client::ask(req, Duration::from_millis(500), true) {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::debug!(error = %e, "could not record via the daemon");
            // Fall back to writing the history file directly so context is not
            // lost when the daemon is unavailable.
            let cfg = Config::load()?;
            let path = config::history_path()?;
            let mut store = Store::load_with(&path, &cfg)?;
            store.record_with_cwd(&command, args.exit_code, args.cwd.as_deref());
            store.save(&path)?;
            Ok(())
        }
    }
}

fn feedback(args: FeedbackArgs) -> termplete::Result<()> {
    let cfg = Config::load()?;
    if !cfg.terminal_enabled() {
        return Ok(());
    }
    let request = Request::Feedback {
        id: 1,
        prefix: args.prefix,
        command: args.command.join(" "),
        cwd: args.cwd.map(|path| path.to_string_lossy().into_owned()),
    };
    client::ask(request, Duration::from_secs(2), cfg.daemon.autostart)?;
    Ok(())
}

// --- auth ---------------------------------------------------------------------

fn auth_cmd(cmd: AuthCmd) -> termplete::Result<()> {
    match cmd {
        AuthCmd::Login {
            provider,
            key,
            stdin,
            force,
            use_provider,
        } => login(&provider, key.as_deref(), stdin, force, use_provider),
        AuthCmd::Logout { provider } => {
            let preset = Config::load()?.provider(Some(&provider))?;
            if auth::uses_external_auth(&preset) {
                termplete::acp::logout()?;
                println!("Signed out of the ChatGPT account managed by Codex ACP.");
            } else {
                auth::delete(&provider)?;
                println!("Removed the stored key for `{provider}`.");
            }
            let stopped = auth::stop_daemon_if_running()?;
            if stopped {
                println!("Stopped the running daemon to discard cached provider state.");
            }
            Ok(())
        }
        AuthCmd::Providers => {
            print_providers();
            Ok(())
        }
        AuthCmd::Status => auth_status(),
    }
}

#[cfg(test)]
mod auth_cli_tests {
    use super::*;

    #[test]
    fn auth_remove_alias_parses_without_accessing_credentials() {
        let cli = Cli::try_parse_from(["tp", "auth", "remove", "--provider", "openai"])
            .expect("auth remove should be accepted");
        let Cmd::Auth {
            command: AuthCmd::Logout { provider },
        } = cli.command
        else {
            panic!("auth remove should select the stored-key removal command");
        };
        assert_eq!(provider, "openai");
    }
}

fn login(
    provider: &str,
    key: Option<&str>,
    from_stdin: bool,
    force: bool,
    use_provider: bool,
) -> termplete::Result<()> {
    let cfg = Config::load()?;
    let preset = cfg.provider(Some(provider))?;

    if auth::uses_external_auth(&preset) {
        if key.is_some() || from_stdin {
            return Err(Error::Auth(
                "OpenAI ACP uses ChatGPT authentication managed by Codex; do not pass an API key"
                    .into(),
            ));
        }
        termplete::acp::login()?;
        println!("signed in to ChatGPT through OpenAI Codex ACP");
        if use_provider {
            set_active(&cfg, provider)?;
        } else if cfg.provider.as_deref() != Some(provider) {
            println!(
                "hint: make it active with `{} auth login --provider {provider} --use-provider`",
                termplete::BIN_NAME
            );
        }
        if is_live_daemon() {
            restart_daemon()?;
        }
        return Ok(());
    }

    let key = match (key, from_stdin) {
        (Some(k), _) => k.to_string(),
        (None, true) => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf.trim().to_string()
        }
        (None, false) => {
            if preset.no_auth {
                println!("`{provider}` is local and needs no API key.");
                if use_provider {
                    set_active(&cfg, provider)?;
                }
                return Ok(());
            }
            if !std::io::stdin().is_terminal() {
                return Err(Error::Auth(
                    "no key supplied — pipe it in with --stdin, or pass --key".into(),
                ));
            }
            termplete::setup::prompt_api_key(&preset.name)?
        }
    };

    if key.is_empty() {
        return Err(Error::Auth("empty API key".into()));
    }

    termplete::setup::explain_keychain_access(provider);
    auth::login(&preset, &key, force)?;
    println!(
        "stored a key for `{provider}` in the system keychain ({})",
        auth::SERVICE
    );

    if use_provider {
        set_active(&cfg, provider)?;
    } else if cfg.provider.as_deref() != Some(provider) {
        println!(
            "hint: make it active with `{} auth login --provider {provider} --use-provider`",
            termplete::BIN_NAME
        );
    }

    // A running daemon cached the previous credential at startup.
    if is_live_daemon() {
        restart_daemon()?;
    }

    // Verify immediately: a wrong key discovered now beats a mystery later.
    match verify(&preset) {
        Ok(()) => println!("verified: `{provider}` accepted the key"),
        Err(e) => {
            eprintln!("warning: the key was saved but a test request failed: {e}");
        }
    }
    Ok(())
}

fn set_active(cfg: &Config, provider: &str) -> termplete::Result<()> {
    let mut cfg = cfg.clone();
    auth::set_active_provider(&mut cfg, provider)?;
    println!("active provider: {provider}");
    Ok(())
}

/// Send a one-token completion to confirm the credential works.
fn verify(preset: &ProviderPreset) -> termplete::Result<()> {
    let cfg = Config::load()?;
    let credential = auth::require(preset, None)?;
    let client = termplete::provider::Client::with_timeout(Duration::from_secs(20));
    let req = termplete::provider::ChatRequest::new(
        cfg.effective_model(preset),
        vec![
            termplete::provider::Message::system("Reply with a single character."),
            termplete::provider::Message::user("ping"),
        ],
    );
    client.complete(preset, &req.model, credential.as_deref(), &req)?;
    Ok(())
}

fn print_providers() {
    let cfg = Config::load().unwrap_or_default();
    let table = cfg.provider_table();
    let active = cfg.provider.as_deref().unwrap_or("");
    println!("{:<12} {:<46} {:<30} AUTH", "NAME", "BASE URL", "MODEL");
    for (name, p) in table {
        let auth_label = if auth::uses_external_auth(&p) {
            "codex-acp"
        } else if p.no_auth {
            "none (local)"
        } else if auth::is_logged_in(&name) {
            "keychain"
        } else if p
            .env_key
            .as_deref()
            .is_some_and(|v| std::env::var_os(v).is_some())
        {
            "env"
        } else {
            "not set"
        };
        let marker = if name == active { "*" } else { " " };
        // The model column applies to the active provider only: a global `model`
        // is meaningless for every other row.
        let model = if name == active {
            let m = cfg.effective_model(&p);
            if m.is_empty() {
                "—".to_string()
            } else {
                m
            }
        } else {
            "—".to_string()
        };
        println!(
            "{marker}{:<12} {:<46} {:<30} {}",
            name,
            if auth::uses_external_auth(&p) {
                "codex-acp (stdio)"
            } else {
                p.base_url.as_str()
            },
            model,
            auth_label
        );
    }
    println!("\n* active provider");
}

fn auth_status() -> termplete::Result<()> {
    let cfg = Config::load()?;
    let table = cfg.provider_table();
    println!("{:<12} {:<10} {:<8} DETAIL", "PROVIDER", "AUTH", "ACTIVE");
    for (name, preset) in table {
        let source = auth::source(&preset)?;
        let active = if cfg.provider.as_deref() == Some(name.as_str()) {
            "yes"
        } else {
            ""
        };
        let detail = match &source {
            auth::KeySource::Environment(v) => format!("key from ${v}"),
            auth::KeySource::CodexAcp => {
                "ChatGPT sign-in is managed by Codex ACP; run `tp auth login --provider openai-acp` to sign in".into()
            }
            _ => format!("{source}"),
        };
        println!(
            "{:<12} {:<10} {:<8} {}",
            name,
            source.to_string(),
            active,
            detail
        );
    }
    Ok(())
}

/// Run the interactive setup wizard and apply everything it collected.
fn setup_wizard() -> termplete::Result<()> {
    let choices = termplete::setup::run()?;

    let mut cfg = Config::load()?;

    // Register the provider in config *before* storing its key: a custom provider
    // does not exist in the preset table yet, and the keychain entry is keyed by
    // provider name, so the lookup needs it to be present first.
    termplete::setup::apply(&mut cfg, &choices)?;

    if let (Some(provider), Some(key)) = (&choices.provider, &choices.api_key) {
        let fresh = Config::load()?;
        match termplete::setup::store_key(provider, key, &fresh) {
            Ok(()) => {
                println!();
                println!("  Key for `{provider}` saved to your system keychain.");
            }
            Err(e) => {
                // Do not leave the user with a provider configured but no
                // credential, silently asking for a key every time.
                println!();
                println!("  Config written, but the key was NOT stored: {e}");
                println!("  Add it later with: tp auth login --provider {provider}");
            }
        }
    }

    if let Some(provider) = choices.provider.as_deref() {
        if cfg
            .provider(Some(provider))
            .is_ok_and(|preset| auth::uses_external_auth(&preset))
        {
            println!();
            println!("  Starting the ChatGPT sign-in flow through Codex ACP...");
            if let Err(error) = termplete::acp::login() {
                eprintln!("  {error}");
                eprintln!("  Setup is saved. Run `tp auth login --provider openai-acp` to retry.");
            }
        }
    }

    println!();
    println!("  Config written to {}", config::config_path()?.display());
    println!();
    println!(
        "  provider  {}",
        cfg.provider.as_deref().unwrap_or("<unset>")
    );
    println!(
        "  model     {}",
        cfg.model.as_deref().unwrap_or("<preset default>")
    );
    // Report the URL actually in force, which may come from the built-in preset.
    match cfg.provider(None) {
        Ok(p) => println!("  base url  {}", p.base_url),
        Err(_) => println!("  base url  <unset>"),
    }

    let shell = termplete::shellutil::shell_name(&termplete::shellutil::user_shell());
    println!();
    if matches!(shell.as_str(), "zsh" | "bash") {
        let rc = termplete::setup::install_integration(&shell)?;
        println!(
            "  Autocomplete enabled for new {shell} sessions in {}",
            rc.display()
        );
        println!("  Reload this session with `exec {shell}`.");
    } else {
        println!(
            "  Shell integration supports zsh and bash. Add the integration to their rc file."
        );
    }

    if is_live_daemon() {
        println!();
        restart_daemon()?;
    }
    Ok(())
}

/// Open the settings menu.
fn settings_menu() -> termplete::Result<()> {
    termplete::settings::run().map_err(termplete::Error::Auth)?;
    if Config::load()?.enabled == Some(false) {
        lifecycle_commands::stop()?;
    } else if is_live_daemon() {
        restart_daemon()?;
    }
    Ok(())
}

// --- suggest ------------------------------------------------------------------

fn suggest(args: SuggestArgs) -> termplete::Result<()> {
    let cfg = Config::load()?;
    let req = if args.intent {
        Request::Generate {
            id: 1,
            instruction: args.line.clone(),
            cwd: std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
        }
    } else {
        Request::Suggest {
            id: 1,
            line: args.line.clone(),
            cursor: args.cursor,
            mode: args.mode.clone(),
            local_only: args.local_only,
            frame: args.frame.or(Some(0)),
            cwd: std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
        }
    };
    let timeout = Duration::from_millis(cfg.daemon.connect_timeout_ms.max(100))
        + Duration::from_secs(cfg.daemon.request_timeout_secs.max(1))
        + Duration::from_millis(if cfg.decision.active() && !args.local_only {
            cfg.decision.timeout_ms
        } else {
            0
        });
    let res = if cfg.terminal_enabled() {
        client::ask(req, timeout, cfg.daemon.autostart)?
    } else {
        Response::Suggest {
            id: 1,
            ghost: String::new(),
            ghost_plain: String::new(),
            candidates: Vec::new(),
            mode: cfg.mode.as_str().into(),
            source: "disabled".into(),
            diagnostic: Some("TermPlete is disabled in this terminal. Change Preferences > Terminal apps in `tp settings`.".into()),
        }
    };

    if let Response::Error { message, .. } = &res {
        if args.shell_reply {
            println!("E\t{}", shell_message(message));
            return Ok(());
        }
        return Err(Error::Daemon(message.clone()));
    }
    let Response::Suggest {
        ghost,
        candidates,
        mode,
        source,
        ghost_plain,
        diagnostic,
        ..
    } = res
    else {
        return Err(Error::Daemon(
            "unexpected reply to a suggest request".into(),
        ));
    };

    if args.shell_reply {
        if candidates.is_empty() {
            if let Some(issue) = diagnostic {
                println!("E\t{}", shell_message(&issue));
            } else {
                println!("N");
            }
        } else {
            let commands: Vec<_> = candidates
                .iter()
                .filter(|c| !c.chars().any(char::is_control))
                .collect();
            print!("{}", if source == "context-refine" { "R" } else { "C" });
            for command in commands {
                print!("\t{command}");
            }
            println!();
        }
        return Ok(());
    }
    if args.json {
        let payload = serde_json::json!({
            "ghost": ghost_plain,
            "styled_ghost": ghost,
            "candidates": candidates,
            "mode": mode,
            "source": source,
            "diagnostic": diagnostic,
        });
        println!("{payload}");
        return Ok(());
    }

    // Newline-separated candidates, so a shell completion function can consume
    // them with a plain `while read` instead of parsing JSON.
    if args.candidates {
        for c in candidates {
            if !c.contains('\n') {
                println!("{c}");
            }
        }
        return Ok(());
    }

    if args.explain {
        eprintln!(
            "mode={mode} source={source} candidates={}",
            candidates.len()
        );
        if let Some(issue) = &diagnostic {
            eprintln!("reason: {issue}");
        }
    }

    if args.raw || args.plain {
        use std::io::Write;
        // `--plain` exists for shells that render their own styling and must not
        // receive escape codes inside their edit buffer.
        let out = if args.plain { &ghost_plain } else { &ghost };
        print!("{out}");
        let _ = std::io::stdout().flush();
    } else if ghost_plain.is_empty() {
        println!("(no suggestion)");
    } else {
        println!("{ghost_plain}");
    }
    Ok(())
}

fn shell_message(message: &str) -> String {
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect()
}

// --- shell / context / status -------------------------------------------------

fn shell(args: ShellArgs) -> termplete::Result<()> {
    let cfg = Config::load()?;
    if !cfg.terminal_enabled() {
        use std::os::unix::process::CommandExt;
        let shell = args
            .shell
            .map(PathBuf::from)
            .unwrap_or_else(shellutil::user_shell);
        let mut command = std::process::Command::new(shell);
        if let Some(script) = args.command {
            command.arg("-c").arg(script);
        } else {
            command.arg("-i");
        }
        return Err(command.exec().into());
    }
    let store = Store::load_with(&config::history_path()?, &cfg)?;
    let store = std::sync::Arc::new(parking_lot::Mutex::new(store));
    let opts = pty::Opts {
        shell: args.shell,
        command: args.command,
        rows: env_size("LINES", 24),
        cols: env_size("COLUMNS", 80),
        forward_to_daemon: true,
        ..Default::default()
    };
    pty::run(opts, store)
}

fn env_size(name: &str, fallback: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn context(import: Option<PathBuf>, search: Option<String>) -> termplete::Result<()> {
    let cfg = Config::load()?;
    let mut store = Store::load_with(&config::history_path()?, &cfg)?;
    if let Some(path) = import {
        // Pause a running daemon so its older snapshot cannot overwrite the import.
        let running = is_live_daemon();
        if running {
            service(ServiceCmd::Stop)?;
            if !lifecycle_commands::wait_for_ready(false, Duration::from_secs(10)) {
                return Err(Error::Daemon(
                    "daemon did not stop before history import".into(),
                ));
            }
            store = Store::load_with(&config::history_path()?, &cfg)?;
        }
        let count = store.import_history(&path)?;
        store.save(&config::history_path()?)?;
        println!(
            "Imported {count} historical commands; {} commands in memory.",
            store.memory().len()
        );
        if running {
            service(ServiceCmd::Start)?;
        }
        if search.is_none() {
            return Ok(());
        }
    }
    if let Some(prefix) = search {
        for (command, _) in store.memory().prefix(&prefix, 20) {
            println!("{command}");
        }
        return Ok(());
    }
    println!(
        "{} commands in memory ({})",
        store.memory().len(),
        config::config_dir()?.join("commands.json").display()
    );
    if store.is_empty() {
        println!("no context recorded yet — run some commands");
        return Ok(());
    }
    println!("{} entries (most recent first):", store.len());
    for entry in store.entries().rev().take(20) {
        println!("  {}", entry.summary());
    }
    Ok(())
}

fn status() -> termplete::Result<()> {
    let cfg = Config::load()?;
    println!(
        "config   {}",
        config::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    println!("socket   {}", daemon::socket_display().display());
    println!("mode     {}", cfg.mode.as_str());
    println!("strategy {}", cfg.decision.strategy.label());
    println!(
        "helper   {}",
        if cfg.decision.active() { "on" } else { "off" }
    );
    println!("privacy  {}", cfg.privacy.sensitive_context.label());
    if cfg.decision.active() {
        println!(
            "decision {} / {} — {} ms budget",
            cfg.decision.provider, cfg.decision.model, cfg.decision.timeout_ms
        );
        println!(
            "privacy  {}",
            if cfg.decision.local_only {
                "local decision helper; fixed secret and path filters"
            } else {
                "remote decision helper receives filtered context"
            }
        );
        if let Err(error) = cfg.decision_provider() {
            println!("issue    {error}");
        }
    }
    println!(
        "terminal {} — {}",
        termplete::terminal::current()
            .as_deref()
            .unwrap_or("unknown"),
        if cfg.terminal_enabled() {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("excluded {}", cfg.disabled_terminals.join(", "));
    let store = Store::load_with(&config::history_path()?, &cfg)?;
    println!(
        "memory   {} commands — {}",
        store.memory().len(),
        config::config_dir()?.join("commands.json").display()
    );
    println!("provider {}", cfg.provider.as_deref().unwrap_or("<unset>"));
    if let Ok(provider) = cfg.provider(None) {
        println!(
            "model    {}",
            cfg.require_model(&provider)
                .unwrap_or_else(|error| error.to_string())
        );
        match auth::source(&provider) {
            Ok(source) => {
                println!("auth     {source}");
                if !provider.no_auth && source == auth::KeySource::None {
                    println!(
                        "issue    no API key available; run `tp auth login --provider {}`",
                        provider.name
                    );
                }
            }
            Err(error) => println!("issue    {error}"),
        }
    }
    println!(
        "log      {}",
        config::config_dir()?.join("daemon.log").display()
    );
    match client::ask(Request::Ping { id: 1 }, Duration::from_millis(500), false) {
        Ok(Response::Ok { note, .. }) => {
            println!(
                "daemon   running{}",
                note.map(|n| format!(" — {n}")).unwrap_or_default()
            );
        }
        Ok(other) => println!("daemon   unexpected reply: {other:?}"),
        Err(_) => println!("daemon   not running"),
    }
    Ok(())
}

fn decision_check(timeout_ms: Option<u64>, json: bool) -> termplete::Result<()> {
    let mut cfg = Config::load()?;
    if let Some(timeout_ms) = timeout_ms {
        cfg.decision.timeout_ms = timeout_ms;
    }
    cfg.validate()?;
    let provider = cfg.decision_provider()?;
    let commands = vec!["git status --short".to_string(), "git log -5".to_string()];
    let request = termplete::provider::ChatRequest::new(
        "",
        vec![termplete::provider::Message::user(
            "The user wants to check which working tree files have changed. Typed prefix: git . No credentials or private paths are present.",
        )],
    );
    let runtime = termplete::decision_runtime::Runtime::new(&cfg);
    let started = std::time::Instant::now();
    let decision = runtime.evaluate(&request, &commands)?;
    let elapsed_ms = started.elapsed().as_millis();
    let selected: Vec<_> = decision
        .ranking
        .iter()
        .filter_map(|index| commands.get(*index))
        .collect();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "provider": provider.name, "model": cfg.decision.model,
                "elapsed_ms": elapsed_ms, "allow_remote": decision.allow_remote,
                "omit_context": decision.omit_context, "candidates": selected,
                "synthetic_context": true,
            })
        );
    } else {
        println!(
            "Decision helper ready: {} / {} ({} ms)",
            provider.name, cfg.decision.model, elapsed_ms
        );
        println!("Synthetic check only; no command history or directory contents were read.");
        println!(
            "Remote context: {}; omit extra context: {}",
            decision.allow_remote, decision.omit_context
        );
        for command in selected {
            println!("  {command}");
        }
    }
    Ok(())
}

/// Stop a running daemon and start a fresh one so it picks up new config.
fn restart_daemon() -> termplete::Result<()> {
    lifecycle_commands::shutdown();
    // The old process needs a moment to release the socket before the new one
    // can bind it.
    lifecycle_commands::wait_for_ready(false, Duration::from_secs(1));
    daemon::spawn_background()?;
    // Credential stores can take several seconds to unlock on startup.
    if lifecycle_commands::wait_for_ready(true, Duration::from_secs(10)) {
        println!("daemon restarted with the new config");
    } else {
        println!("config saved; restart the daemon with `tp service start` to apply it");
    }
    Ok(())
}

// --- config -------------------------------------------------------------------

fn config_cmd(cmd: ConfigCmd) -> termplete::Result<()> {
    match cmd {
        ConfigCmd::Path => {
            println!("{}", config::config_path()?.display());
            Ok(())
        }
        ConfigCmd::Show => {
            let cfg = Config::load()?;
            print!("{}", config::redacted_summary(&cfg));
            println!(
                "keys      reveal={} accept={} cycle={} dismiss={} toggle={}",
                KeyBindings::describe(&cfg.keys.reveal),
                KeyBindings::describe(&cfg.keys.accept),
                KeyBindings::describe(&cfg.keys.cycle),
                KeyBindings::describe(&cfg.keys.dismiss),
                KeyBindings::describe(&cfg.keys.toggle),
            );
            Ok(())
        }
        ConfigCmd::Get { key } => {
            let cfg = Config::load()?;
            println!("{}", get_value(&cfg, &key)?);
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            let mut cfg = Config::load()?;
            set_value(&mut cfg, &key, &value)?;
            cfg.save()?;
            println!("{key} = {value}");
            // The daemon reads config once at startup, so a change would
            // otherwise not take effect until it is restarted. Restarting here
            // is what makes `tp config set` do what the user just asked.
            if key == "enabled" && cfg.enabled == Some(false) {
                lifecycle_commands::stop()?;
            } else if key != "disabled_terminals" && is_live_daemon() {
                restart_daemon()?;
            }
            Ok(())
        }
    }
}

/// Resolve a dotted config key for `tp config get`.
fn get_value(cfg: &Config, key: &str) -> termplete::Result<String> {
    Ok(match key {
        "enabled" => cfg.enabled.unwrap_or(true).to_string(),
        "disabled_terminals" => cfg.disabled_terminals.join(","),
        "terminal_enabled" => cfg.terminal_enabled().to_string(),
        "provider" => cfg.provider.clone().unwrap_or_default(),
        "model" => cfg.model.clone().unwrap_or_default(),
        "mode" => cfg.mode.as_str().to_string(),
        "decision.strategy" => cfg.decision.strategy.as_str().into(),
        "decision.enabled" => cfg.decision.enabled.to_string(),
        "decision.active" => cfg.decision.active().to_string(),
        "privacy.sensitive_context" => cfg.privacy.sensitive_context.as_str().into(),
        "decision.protocol" => cfg.decision.protocol.as_str().into(),
        "decision.provider" => cfg.decision.provider.clone(),
        "decision.model" => cfg.decision.model.clone(),
        "decision.base_url" => cfg.decision.base_url.clone().unwrap_or_default(),
        "decision.timeout_ms" => cfg.decision.timeout_ms.to_string(),
        "decision.local_only" => cfg.decision.local_only.to_string(),
        "decision.private_paths" => {
            serde_json::to_string(&cfg.decision.private_paths).expect("path serialization")
        }
        "animations" => cfg.style.animations.to_string(),
        "spinner" => cfg.style.spinner.to_string(),
        "animation_frames" => cfg.style.animation_frames.to_string(),
        "zle_palette" => termplete::anim::zle_palette(&cfg.style)
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(" "),
        "ghost_color" => cfg.style.ghost_color.clone(),
        "automatic_color" | "aggressive_color" | "eager_color" => cfg.style.eager_color.clone(),
        "show_hint" => cfg.style.show_hint.to_string(),
        "min_chars" => cfg.modes.min_chars.to_string(),
        "on_demand_debounce_ms" | "subtle_debounce_ms" => cfg.modes.subtle_debounce_ms.to_string(),
        "automatic_debounce_ms" | "aggressive_debounce_ms" | "eager_debounce_ms" => {
            cfg.modes.eager_debounce_ms.to_string()
        }
        "max_suggestions" => cfg.modes.max_suggestions.to_string(),
        "history_entries" => cfg.context.history_entries.to_string(),
        "command_memory_entries" => cfg.context.command_memory_entries.to_string(),
        "history_candidates" => cfg.context.history_candidates.to_string(),
        "import_shell_history" => cfg.context.import_shell_history.to_string(),
        "ai_select_history" => cfg.context.ai_select_history.to_string(),
        "capture_output" => cfg.context.capture_output.to_string(),
        "shell_history_file" => cfg.context.shell_history_file.clone().unwrap_or_default(),
        "max_output_bytes" => cfg.context.max_output_bytes.to_string(),
        "autostart" => cfg.daemon.autostart.to_string(),
        "keys.reveal" => KeyBindings::normalize(&cfg.keys.reveal),
        "keys.reveal_label" => KeyBindings::describe(&cfg.keys.reveal),
        "keys.accept" => KeyBindings::normalize(&cfg.keys.accept),
        "keys.cycle" => KeyBindings::normalize(&cfg.keys.cycle),
        "keys.dismiss" => KeyBindings::normalize(&cfg.keys.dismiss),
        "keys.toggle" => KeyBindings::normalize(&cfg.keys.toggle),
        other => {
            return Err(Error::Config(config::ConfigError::Io {
                path: PathBuf::from(other.to_string()),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "unknown config key — try `tp config show`",
                ),
            }));
        }
    })
}

/// Keys accepting `true/false` (plus `on/off`, `yes/no`, `1/0`).
const BOOL_KEYS: &[&str] = &[
    "enabled",
    "decision.enabled",
    "decision.local_only",
    "animations",
    "spinner",
    "show_hint",
    "import_shell_history",
    "ai_select_history",
    "capture_output",
    "autostart",
];
fn set_value(cfg: &mut Config, key: &str, value: &str) -> termplete::Result<()> {
    if BOOL_KEYS.contains(&key) {
        let v = parse_bool(key, value)?;
        match key {
            "enabled" => cfg.enabled = Some(v),
            "decision.enabled" => cfg.decision.enabled = v,
            "decision.local_only" => cfg.decision.local_only = v,
            "animations" => cfg.style.animations = v,
            "spinner" => cfg.style.spinner = v,
            "show_hint" => cfg.style.show_hint = v,
            "import_shell_history" => cfg.context.import_shell_history = v,
            "ai_select_history" => cfg.context.ai_select_history = v,
            "capture_output" => cfg.context.capture_output = v,
            _ => cfg.daemon.autostart = v, // "autostart"
        }
        cfg.validate()?;
        return Ok(());
    }
    match key {
        "disabled_terminals" => cfg.disabled_terminals = termplete::terminal::parse_list(value),
        "provider" => cfg.select_provider(value),
        "model" => cfg.model = Some(value.to_string()),
        "decision.strategy" => cfg.decision.strategy = config::DecisionStrategy::parse(value)
            .ok_or_else(|| bad(key, "llm, decision or hybrid"))?,
        "privacy.sensitive_context" => cfg.privacy.sensitive_context = config::SensitiveContext::parse(value)
            .ok_or_else(|| bad(key, "block or decision"))?,
        "decision.protocol" => cfg.decision.protocol = config::DecisionProtocol::parse(value)
            .ok_or_else(|| bad(key, "auto, system-one or chat"))?,
        "decision.provider" => cfg.decision.provider = value.trim().to_string(),
        "decision.model" => cfg.decision.model = value.trim().to_string(),
        "decision.base_url" => cfg.decision.base_url = if value.trim().is_empty() { None } else { Some(value.trim().to_string()) },
        "decision.timeout_ms" => cfg.decision.timeout_ms = value.parse().map_err(|_| bad(key, "100 to 10000 milliseconds"))?,
        "decision.private_paths" => cfg.decision.private_paths = serde_json::from_str(value)
            .map_err(|_| bad(key, "a JSON array of private path substrings"))?,
        "mode" => {
            cfg.mode = Mode::parse(value).ok_or_else(|| {
                Error::Config(config::ConfigError::Io {
                    path: PathBuf::from("mode"),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("`{value}` is not one of automatic, on-demand (legacy: aggressive, eager, subtle, off)"),
                    ),
                })
            })?
        }
        "animation_frames" => cfg.style.animation_frames = parse_int(key, value)?,
        "ghost_color" => cfg.style.ghost_color = value.to_string(),
        "automatic_color" | "aggressive_color" | "eager_color" => cfg.style.eager_color = value.to_string(),
        "min_chars" => cfg.modes.min_chars = parse_int(key, value)?,
        "on_demand_debounce_ms" | "subtle_debounce_ms" => {
            cfg.modes.subtle_debounce_ms = parse_int(key, value)?
        }
        "automatic_debounce_ms" | "aggressive_debounce_ms" | "eager_debounce_ms" => {
            cfg.modes.eager_debounce_ms = parse_int(key, value)?
        }
        "max_suggestions" => cfg.modes.max_suggestions = parse_int(key, value)?,
        "history_entries" => cfg.context.history_entries = parse_int(key, value)?,
        "command_memory_entries" => cfg.context.command_memory_entries = parse_int(key, value)?,
        "history_candidates" => cfg.context.history_candidates = parse_int(key, value)?,
        "shell_history_file" => {
            cfg.context.shell_history_file = if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }
        "max_output_bytes" => cfg.context.max_output_bytes = parse_int(key, value)?,
        "keys.reveal" => cfg.keys.reveal = value.to_string(),
        "keys.accept" => cfg.keys.accept = value.to_string(),
        "keys.cycle" => cfg.keys.cycle = value.to_string(),
        "keys.dismiss" => cfg.keys.dismiss = value.to_string(),
        "keys.toggle" => cfg.keys.toggle = value.to_string(),
        other => {
            return Err(Error::Config(config::ConfigError::Io {
                path: PathBuf::from(other.to_string()),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "unknown config key — try `tp config show`",
                ),
            }))
        }
    }
    cfg.validate()?;
    Ok(())
}

fn bad(key: &str, expected: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("`{key}` expects {expected}"),
    )
}
fn parse_bool(key: &str, value: &str) -> termplete::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        _ => Err(Error::Config(config::ConfigError::Io {
            path: PathBuf::from(key.to_string()),
            source: bad(key, "true or false"),
        })),
    }
}

fn parse_int<T: std::str::FromStr>(key: &str, value: &str) -> termplete::Result<T> {
    value.parse().map_err(|_| {
        Error::Config(config::ConfigError::Io {
            path: PathBuf::from(key.to_string()),
            source: bad(key, "an integer"),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_cli_preserves_instruction_and_rejects_local_only() {
        let cli = Cli::try_parse_from([
            "tp",
            "suggest",
            "--intent",
            "--shell-reply",
            "--",
            "find large video files",
        ])
        .unwrap();
        let Cmd::Suggest(args) = cli.command else {
            panic!("suggest expected")
        };
        assert!(args.intent && args.shell_reply);
        assert_eq!(args.line, "find large video files");
        assert!(
            Cli::try_parse_from(["tp", "suggest", "--intent", "--local-only", "anything"]).is_err()
        );
    }

    #[test]
    fn shell_diagnostics_are_bounded_and_cannot_inject_reply_fields() {
        assert_eq!(shell_message("failure\tbad\nline\r\0"), "failurebadline");
        assert_eq!(shell_message(&"é".repeat(300)).chars().count(), 240);
    }

    #[test]
    fn feedback_cli_preserves_prefix_spacing_and_full_command() {
        let cli = Cli::try_parse_from([
            "tp",
            "feedback",
            "--prefix",
            "  git l",
            "--",
            "  git log --oneline",
        ])
        .unwrap();
        let Cmd::Feedback(args) = cli.command else {
            panic!("feedback command expected");
        };
        assert_eq!(args.prefix, "  git l");
        assert_eq!(args.command, vec!["  git log --oneline"]);
    }

    #[test]
    fn provider_config_edits_restore_saved_models() {
        let mut cfg = Config::default();
        set_value(&mut cfg, "provider", "openai").unwrap();
        set_value(&mut cfg, "model", "first-model").unwrap();
        set_value(&mut cfg, "provider", "ollama").unwrap();
        assert_eq!(get_value(&cfg, "model").unwrap(), "");
        set_value(&mut cfg, "model", "second-model").unwrap();
        set_value(&mut cfg, "provider", "openai").unwrap();
        assert_eq!(get_value(&cfg, "model").unwrap(), "first-model");
        set_value(&mut cfg, "provider", "ollama").unwrap();
        assert_eq!(get_value(&cfg, "model").unwrap(), "second-model");
    }

    #[test]
    fn decision_configuration_keys_round_trip_and_reject_invalid_choices() {
        let mut cfg = Config::default();
        for (key, value) in [
            ("decision.strategy", "hybrid"),
            ("decision.protocol", "chat"),
            ("decision.provider", "ollama"),
            ("decision.model", "local-classifier"),
            ("decision.base_url", "http://127.0.0.1:11434/v1"),
            ("decision.timeout_ms", "700"),
            ("decision.local_only", "true"),
            ("decision.private_paths", "[\"/work/private\"]"),
        ] {
            set_value(&mut cfg, key, value).unwrap();
            assert_eq!(get_value(&cfg, key).unwrap(), value);
        }
        assert_eq!(
            cfg.decision_provider().unwrap().wire,
            config::Wire::OpenAiCompat
        );
        assert!(set_value(&mut cfg.clone(), "decision.strategy", "unknown").is_err());
        assert!(set_value(&mut cfg.clone(), "decision.protocol", "unknown").is_err());
        assert!(set_value(&mut cfg.clone(), "decision.timeout_ms", "99").is_err());
        assert!(set_value(&mut cfg.clone(), "decision.private_paths", "not-json").is_err());
    }

    #[test]
    fn enabled_configuration_key_defaults_on_and_can_be_toggled() {
        let mut cfg = Config::default();
        assert_eq!(get_value(&cfg, "enabled").unwrap(), "true");
        set_value(&mut cfg, "enabled", "false").unwrap();
        assert_eq!(get_value(&cfg, "enabled").unwrap(), "false");
        assert!(!cfg.terminal_enabled());
        set_value(&mut cfg, "enabled", "true").unwrap();
        assert_eq!(get_value(&cfg, "enabled").unwrap(), "true");
    }

    #[test]
    fn decision_check_accepts_an_explicit_cold_start_budget() {
        let cli =
            Cli::try_parse_from(["tp", "decision", "check", "--timeout-ms", "10000", "--json"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Cmd::Decision(DecisionCmd::Check {
                timeout_ms: Some(10000),
                json: true
            })
        ));
    }

    #[test]
    fn on_disk_script_lookup_uses_the_shipped_filenames() {
        // Regression: the lookup once built `zsh.plugin.zsh`, which never matched
        // a real file, so the on-disk path silently fell through to the embedded
        // copy and an edited script appeared to have no effect.
        for shell in ["zsh", "bash"] {
            let name = format!("termplete.plugin.{shell}");
            let dir = PathBuf::from("shells");
            assert!(
                dir.join(&name).exists(),
                "shells/{name} must exist for the on-disk lookup to find it"
            );
        }
    }

    #[test]
    fn embedded_scripts_are_present_for_both_shells() {
        assert!(embedded_shell_script("zsh").is_some());
        assert!(embedded_shell_script("bash").is_some());
        assert!(embedded_shell_script("fish").is_none());
    }

    #[test]
    fn embedded_and_on_disk_scripts_agree() {
        // A shipped script that disagrees with the embedded copy would mean the
        // plugin behaves differently depending on install path.
        for shell in ["zsh", "bash"] {
            let on_disk = std::fs::read_to_string(format!("shells/termplete.plugin.{shell}"))
                .expect("shipped script");
            assert_eq!(
                embedded_shell_script(shell),
                Some(on_disk.as_str()),
                "embedded {shell} script is stale — rebuild"
            );
        }
    }

    #[test]
    fn init_print_rejects_unknown_shells() {
        let err = print_shell_integration("fish").unwrap_err();
        assert!(err.to_string().contains("zsh, bash"), "{err}");
    }
}
