//! The background daemon: one process per user owning the context store, the
//! HTTP client and the animation tables, shared by every shell.
//!
//! Why a daemon at all? Because the alternative is re-reading the history file,
//! re-opening a TLS connection and re-resolving a keychain entry on every
//! keystroke. Here, a suggestion is a unix-socket round-trip against warm state.
//!
//! Concurrency is deliberately simple: connections are handled on threads, but
//! the context store lives behind a mutex. A shell sends at most one suggestion
//! request at a time, so contention is not a real concern, and avoiding an async
//! runtime keeps the failure modes obvious.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::anim::Anim;
use crate::client::request_id;
use crate::config::{self, Config, DecisionStrategy, Mode, ProviderPreset};
use crate::context::Store;
use crate::protocol::{line_prefix, Request, Response};
use crate::provider::Client as HttpClient;
use crate::suggest::{self, PromptBuilder, Suggestion};
use crate::{auth, Error, Result};
use parking_lot::Mutex;

/// Shared daemon state.
struct State {
    cfg: Config,
    history_path: PathBuf,
    store: Mutex<Store>,
    /// Resolved once at startup so a keychain hit is not on the keystroke path.
    credential: Mutex<Option<String>>,
    auth_loaded: AtomicBool,
    provider: Option<ProviderPreset>,
    http: HttpClient,
    decision: crate::decision_runtime::Runtime,
    anim: Anim,
    last_issue: Mutex<Option<String>>,
    persist_lock: Mutex<()>,
}

/// Run the daemon in the foreground until it is told to stop or goes idle.
///
/// Returns when the idle timeout elapses, or immediately after a `Shutdown`.
pub fn serve() -> Result<()> {
    config::secure_config_dir()?;
    let path = config::socket_path();
    let cfg = Config::load()?;
    if !global_enabled(&cfg) {
        return Ok(());
    }
    let history_path = config::history_path()?;
    let mut store = Store::load_with(&history_path, &cfg)?;
    if cfg.context.import_shell_history {
        if let Some(history) = crate::memory::shell_history_path(&cfg.context) {
            match store.import_history(&history) {
                Ok(count) if count > 0 => {
                    tracing::info!(commands = count, "imported native shell history");
                    store.save(&history_path)?;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "could not import native shell history")
                }
            }
        }
    }

    // History is immediately usable without an API key or a Keychain dialog.
    // Resolve credentials lazily only when a model request is actually needed.
    let (provider, initial_issue) = match cfg.provider(None) {
        Ok(p) => (Some(p), None),
        Err(_) if cfg.decision.active() && cfg.decision.strategy == DecisionStrategy::Decision => {
            (None, None)
        }
        Err(error) => (None, Some(error.to_string())),
    };

    let state = Arc::new(State {
        cfg: cfg.clone(),
        history_path,
        store: Mutex::new(store),
        credential: Mutex::new(None),
        auth_loaded: AtomicBool::new(false),
        provider,
        http: HttpClient::with_timeout(Duration::from_secs(cfg.daemon.request_timeout_secs.max(1))),
        decision: crate::decision_runtime::Runtime::new(&cfg),
        anim: Anim::from_style(&cfg.style),
        last_issue: Mutex::new(initial_issue),
        persist_lock: Mutex::new(()),
    });

    let Some(listener) = bind_if_enabled(&path)? else {
        return Ok(());
    };
    tracing::info!(socket = %path.display(), "termplete daemon listening");

    let shutdown = Arc::new(AtomicBool::new(false));
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    listener.set_nonblocking(true)?;

    // Poll for shutdown rather than joining a channel: the accept loop is the
    // only thread that must exit, and this keeps the idle timer simple.
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                let shutdown = Arc::clone(&shutdown);
                let last_activity = Arc::clone(&last_activity);
                std::thread::spawn(move || handle(stream, state, shutdown, last_activity));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
                let idle = *last_activity.lock();
                let limit = state.cfg.daemon.idle_shutdown_secs;
                if limit > 0 && idle.elapsed() > Duration::from_secs(limit) {
                    tracing::info!(idle_secs = limit, "shutting down after idle period");
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    persist(&state);
    let _ = std::fs::remove_file(&path);
    tracing::info!("termplete daemon stopped");
    Ok(())
}

fn global_enabled(cfg: &Config) -> bool {
    cfg.enabled.unwrap_or(true)
}

fn bind_if_enabled(path: &Path) -> Result<Option<UnixListener>> {
    let listener = bind(path)?;
    match Config::load().map(|cfg| global_enabled(&cfg)) {
        Ok(true) => Ok(Some(listener)),
        Ok(false) => {
            let _ = std::fs::remove_file(path);
            drop(listener);
            Ok(None)
        }
        Err(error) => {
            let _ = std::fs::remove_file(path);
            drop(listener);
            Err(error.into())
        }
    }
}

/// Write the context store back to disk.
fn persist(state: &State) {
    let _persist = state.persist_lock.lock();
    let snapshot = state.store.lock().clone();
    if let Err(e) = snapshot.save(&state.history_path) {
        tracing::warn!(error = %e, "could not save history");
    }
}

/// Bind the socket, clearing a stale socket file left by a crashed daemon.
fn bind(path: &Path) -> Result<UnixListener> {
    if std::env::var_os("TERMPLETE_SOCKET").is_none() {
        if let Some(parent) = path.parent() {
            config::prepare_socket_dir(parent)?;
        }
    }
    if path.exists() {
        // A socket that still accepts connections means a daemon is live; one
        // that refuses is stale and safe to remove.
        if UnixStream::connect(path).is_ok() {
            return Err(Error::Daemon(format!(
                "a daemon is already running at {}",
                path.display()
            )));
        }
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)
        .map_err(|e| Error::Daemon(format!("could not bind {}: {e}", path.display())))?;
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(listener)
}

/// Spawn a detached daemon process with no terminal attached.
pub fn spawn_background() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::Daemon(format!("could not locate the termplete binary: {e}")))?;
    let log_path = config::secure_config_dir()?.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    use std::os::unix::fs::PermissionsExt;
    log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("--foreground")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::Daemon(format!("could not start the daemon: {e}")))
}

/// Serve one client connection until it closes.
fn handle(
    stream: UnixStream,
    state: Arc<State>,
    shutdown: Arc<AtomicBool>,
    last_activity: Arc<Mutex<Instant>>,
) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = write_half;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        *last_activity.lock() = Instant::now();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let _ = send(&mut writer, &Response::err(0, format!("bad request: {e}")));
                continue;
            }
        };
        let should_stop = matches!(req, Request::Shutdown { .. });
        let res = dispatch(&state, &req);
        if send(&mut writer, &res).is_err() {
            break;
        }
        if should_stop {
            shutdown.store(true, Ordering::Relaxed);
            break;
        }
    }
    let _ = writer.shutdown(Shutdown::Both);
}

fn send(writer: &mut UnixStream, res: &Response) -> std::io::Result<()> {
    let mut json = serde_json::to_string(res)?;
    json.push('\n');
    writer.write_all(json.as_bytes())?;
    writer.flush()
}

fn dispatch(state: &State, req: &Request) -> Response {
    if !global_enabled(&state.cfg)
        && !matches!(req, Request::Ping { .. } | Request::Shutdown { .. })
    {
        return Response::err(request_id(req), "TermPlete is paused");
    }

    match req {
        Request::Ping { id } => Response::Ok {
            id: *id,
            note: Some(format!(
                "provider={} entries={} mode={} auth={} strategy={} decision_provider={}{}",
                state
                    .provider
                    .as_ref()
                    .map(|p| p.name.as_str())
                    .unwrap_or("<none>"),
                state.store.lock().len(),
                state.cfg.mode.as_str(),
                if state.cfg.decision.active()
                    && state.cfg.decision.strategy == DecisionStrategy::Decision
                {
                    "not-needed"
                } else if state.credential.lock().is_some()
                    || state.provider.as_ref().is_some_and(|p| p.no_auth)
                {
                    "ready"
                } else if !state.auth_loaded.load(Ordering::Relaxed) {
                    "pending"
                } else {
                    "missing"
                },
                state.cfg.decision.strategy.as_str(),
                state.cfg.decision.provider,
                state
                    .last_issue
                    .lock()
                    .as_ref()
                    .map(|issue| format!(" issue={issue}"))
                    .unwrap_or_default()
            )),
        },
        Request::Shutdown { id } => Response::ok(*id),
        Request::ObserveInput { id, input, cwd } => {
            let observed = state
                .store
                .lock()
                .observe_input(input, cwd.as_deref().map(Path::new));
            if observed {
                persist(state);
            }
            Response::ok(*id)
        }
        Request::Record {
            id,
            command,
            exit_code,
            output,
            cwd,
        } => {
            record(
                state,
                command,
                *exit_code,
                output.as_deref(),
                cwd.as_deref().map(Path::new),
            );
            Response::ok(*id)
        }
        Request::Feedback {
            id,
            prefix,
            command,
            cwd,
        } => {
            feedback(state, prefix, command, cwd.as_deref().map(Path::new));
            Response::Ack { id: *id }
        }
        Request::Suggest {
            id,
            line,
            cursor,
            cwd,
            mode,
            local_only,
            frame,
        } => {
            let requested_cwd = cwd.as_deref().map(PathBuf::from);
            let requested_cwd = requested_cwd.as_deref().filter(|path| path.is_dir());
            suggest_reply_with_cwd(
                state,
                *id,
                line,
                *cursor,
                mode.as_deref(),
                *local_only,
                frame.unwrap_or(0),
                requested_cwd,
            )
        }
        Request::Generate {
            id,
            instruction,
            cwd,
        } => {
            let cwd = cwd.as_deref().map(Path::new).filter(|path| path.is_dir());
            generate_reply(state, *id, instruction, cwd)
        }
    }
}

fn record(
    state: &State,
    command: &str,
    exit_code: Option<i32>,
    output: Option<&str>,
    cwd: Option<&Path>,
) {
    let recorded = {
        let mut store = state.store.lock();
        store.record_with_cwd(command, exit_code, cwd).map(|entry| {
            if let Some(out) = output {
                store.attach_output(command, out);
            }
            entry.seq
        })
    };
    // Persist immediately. Saving only on shutdown meant `tp context` and a fresh
    // daemon reported nothing at all for the whole session, which is most of it.
    // Writes are cheap at this size and happen once per command, not per keystroke.
    if recorded.is_some() {
        persist(state);
    }
}

fn feedback(state: &State, prefix: &str, command: &str, cwd: Option<&Path>) {
    let accepted = state
        .store
        .lock()
        .accept_suggestion_with_cwd(prefix, command, cwd);
    if accepted {
        // Acceptance is a deliberate user decision. Make it available to the
        // next suggestion and preserve it across daemon restarts immediately.
        persist(state);
    }
}

/// Resolve the provider, credential and model for one model request.
fn model_credential(
    state: &State,
) -> std::result::Result<(ProviderPreset, Option<String>, String), String> {
    let Some(provider) = state.provider.clone() else {
        return Err("No AI provider configured. Run `tp setup`.".into());
    };
    if !provider.no_auth
        && !auth::uses_external_auth(&provider)
        && !state.auth_loaded.load(Ordering::Relaxed)
    {
        match auth::require(&provider, None) {
            Ok(value) => *state.credential.lock() = value,
            Err(error) => *state.last_issue.lock() = Some(error.to_string()),
        }
        state.auth_loaded.store(true, Ordering::Relaxed);
    }
    let credential = state.credential.lock().clone();
    if credential.is_none() && !provider.no_auth && !auth::uses_external_auth(&provider) {
        let issue = state.last_issue.lock().clone().unwrap_or_else(|| {
            format!(
                "No API key loaded for {}. Run `tp auth login --provider {}` and restart the daemon.",
                provider.name, provider.name
            )
        });
        return Err(issue);
    }
    let model = state
        .cfg
        .require_model(&provider)
        .map_err(|error| error.to_string())?;
    Ok((provider, credential, model))
}

#[cfg(test)]
fn suggest_reply(
    state: &State,
    id: u64,
    line: &str,
    cursor: Option<usize>,
    mode_override: Option<&str>,
    local_only: bool,
    frame: usize,
) -> Response {
    suggest_reply_with_cwd(
        state,
        id,
        line,
        cursor,
        mode_override,
        local_only,
        frame,
        None,
    )
}

// Keep the fields of the wire request explicit at this protocol boundary.
#[allow(clippy::too_many_arguments)]
fn suggest_reply_with_cwd(
    state: &State,
    id: u64,
    line: &str,
    cursor: Option<usize>,
    mode_override: Option<&str>,
    local_only: bool,
    frame: usize,
    cwd: Option<&Path>,
) -> Response {
    let prefix = line_prefix(line, cursor);
    let modes = &state.cfg.modes;
    let mode = resolve_mode(state.cfg.mode, mode_override);
    let directory_command = crate::fs_context::is_directory_command(prefix);
    let decision_enabled = state.cfg.decision.active();
    let local_limit = if decision_enabled && directory_command {
        suggest::suggestion_limit(modes.max_suggestions).max(8)
    } else {
        suggest::suggestion_limit(modes.max_suggestions)
    };

    let current_local = || {
        let store = state.store.lock();
        if !directory_command {
            return suggest::complete_local_with_cwd(&store, prefix, local_limit, cwd);
        }
        cwd.map(|cwd| crate::fs_context::candidates_with_context(prefix, cwd, local_limit, &store))
            .unwrap_or_default()
            .into_iter()
            .map(|text| {
                let evidence = suggest::candidate_context_evidence(&store, &text, cwd)
                    .unwrap_or_else(|| {
                        store.memory().candidate_evidence_with_scope(
                            &text,
                            cwd,
                            state.cfg.context.suggestion_scope,
                        )
                    });
                Suggestion {
                    source: if evidence.context_match {
                        suggest::Source::TerminalContext
                    } else {
                        suggest::Source::Filesystem
                    },
                    score: crate::memory::rank_evidence(&evidence).score,
                    text,
                }
            })
            .collect::<Vec<_>>()
    };
    let local = current_local();
    if directory_command && local.is_empty() {
        return empty(id, mode, Some("No matching local directory."));
    }

    // A blank or too-short buffer never warrants work.
    if prefix.trim().chars().count() < modes.min_chars {
        return empty(id, mode, None);
    }

    let relevant_context = !directory_command
        && suggest::has_relevant_terminal_context(&state.store.lock(), prefix, cwd);
    let can_refine_context = relevant_context
        && mode != Mode::Off
        && (decision_enabled
            || state
                .provider
                .as_ref()
                .is_some_and(|provider| state.cfg.require_model(provider).is_ok()));
    let history = || from_local(id, &current_local(), prefix, state, frame, mode);
    if local
        .iter()
        .any(|candidate| candidate.source == suggest::Source::TerminalContext)
        && suggest::private_prompt_cwd(
            cwd,
            &state.cfg.decision.private_paths,
            state.decision.reviews_private_metadata(),
        )
    {
        return history();
    }
    // The shell's local prepass must not make fresh context unreachable. In
    // on-demand mode this also marks a deferred model request for reveal.
    let deferred_context = || {
        let mut response = history();
        if can_refine_context {
            if let Response::Suggest { source, .. } = &mut response {
                *source = "context-refine".into();
            }
        }
        response
    };

    // Repeated choices are direct evidence of what this user wants. Let that
    // preference win even when AI history selection is enabled.
    if !relevant_context
        && state.store.lock().memory().has_trusted_choice_with_scope(
            prefix,
            cwd,
            state.cfg.context.suggestion_scope,
        )
    {
        return history();
    }

    if mode == Mode::Off {
        return history();
    }
    if local_only {
        return deferred_context();
    }
    // Directory targets always come from the validated local candidate pool.
    if directory_command && !decision_enabled {
        return history();
    }
    let has_history = local.iter().any(|candidate| {
        matches!(
            candidate.source,
            suggest::Source::History
                | suggest::Source::HistoryPrefix
                | suggest::Source::TerminalContext
        )
    });
    if has_history && !relevant_context && !state.cfg.context.ai_select_history && !decision_enabled
    {
        return history();
    }
    let contextual_tool = state.provider.is_some()
        && mode == Mode::Aggressive
        && (prefix.trim_start() == "docker"
            || prefix.trim_start() == "ffmpeg"
            || prefix.trim_start().starts_with("ffmpeg "));
    let skip_common_command = modes.skip_for_common_commands
        && !contextual_tool
        && !relevant_context
        && !decision_enabled
        && !suggest::common_commands(prefix).is_empty();
    let has_ai_selectable_history = has_history && state.cfg.context.ai_select_history;
    if skip_common_command && !has_ai_selectable_history {
        return history();
    }

    // Hold the model back until the buffer looks like a command. This is the
    // difference between `subtle` and firing on every keystroke.
    let last_is_space = prefix.ends_with(' ');
    if mode != Mode::Aggressive && !suggest::worth_llm(prefix, modes, last_is_space) {
        return deferred_context();
    }

    if !judge_input_allowed(state, prefix) {
        return diagnose(
            state,
            history(),
            "Privacy rules kept this command local; sensitive text is not sent to models.".into(),
        );
    }
    let builder = PromptBuilder::new(modes, &state.cfg.context)
        .with_private_paths(&state.cfg.decision.private_paths)
        .with_local_review(state.decision.reviews_private_metadata());
    let mut request = {
        let store = state.store.lock();
        builder.build_with_cwd(&store, prefix, mode == Mode::Aggressive, cwd)
    };
    let judge_request = local_judge_request(state, &request);
    let mut prompt_only_fallback = false;
    request =
        crate::privacy::sanitize_request_with_paths(&request, &state.cfg.decision.private_paths);
    if decision_enabled {
        let judged: Vec<Suggestion> = local
            .iter()
            .filter(|candidate| judge_input_allowed(state, &candidate.text))
            .take(8)
            .cloned()
            .collect();
        let commands: Vec<_> = judged
            .iter()
            .map(|candidate| candidate.text.clone())
            .collect();
        let decision = match evaluate_decision(state, &judge_request, &commands, prefix, cwd) {
            Ok(decision) => decision,
            Err(error) => return diagnose(state, history(), error.to_string()),
        };
        prompt_only_fallback = decision.is_none();
        if let Some(decision) = &decision {
            let latest = current_local();
            let mut ranked: Vec<_> = latest
                .iter()
                .filter(|candidate| {
                    candidate.source == suggest::Source::TerminalContext
                        && judge_input_allowed(state, &candidate.text)
                })
                .cloned()
                .collect();
            ranked.extend(
                decision
                    .ranking
                    .iter()
                    .filter_map(|index| judged.get(*index))
                    .filter(|candidate| candidate.source != suggest::Source::TerminalContext)
                    .filter(|candidate| {
                        !directory_command
                            || latest.iter().any(|valid| valid.text == candidate.text)
                    })
                    .filter(|candidate| {
                        !suggest::superseded_context_command(
                            &state.store.lock(),
                            &candidate.text,
                            cwd,
                        )
                    })
                    .cloned(),
            );
            if !ranked.is_empty() {
                *state.last_issue.lock() = None;
                let mut response = from_local(id, &ranked, prefix, state, frame, mode);
                if let Response::Suggest { source, .. } = &mut response {
                    *source = "decision".into();
                }
                return response;
            }
            if directory_command {
                return history();
            }
            if state.cfg.decision.strategy == DecisionStrategy::Decision {
                return diagnose(state, history(), "Decision only found no suitable known command. Use Decision + AI for new commands.".into());
            }
            if !decision.allow_remote {
                return diagnose(
                    state,
                    history(),
                    "The decision helper kept this request local for privacy.".into(),
                );
            }
            if crate::privacy::sensitive_with_paths(prefix, &state.cfg.decision.private_paths) {
                return diagnose(state, history(), "Private command metadata stays out of the generator; local review can select known commands.".into());
            }
        }
        if directory_command {
            return history();
        }
        if decision
            .as_ref()
            .is_none_or(|decision| decision.omit_context)
        {
            request = builder.build(
                &Store::new(state.cfg.context.clone()),
                prefix,
                mode == Mode::Aggressive,
            );
            request = crate::privacy::sanitize_request_with_paths(
                &request,
                &state.cfg.decision.private_paths,
            );
        }
    }

    // Without a usable provider or credential, history completions are the whole
    // story. This is not an error: the user may not have logged in yet.
    let (provider, credential, model) = match model_credential(state) {
        Ok(resolved) => resolved,
        Err(issue) => return diagnose(state, history(), issue),
    };
    let selection_options = {
        // The store lock is held only across prompt construction; the network
        // call must not block other shells from recording commands.
        let store = state.store.lock();
        if has_history
            && !relevant_context
            && state.cfg.context.ai_select_history
            && !prompt_only_fallback
        {
            suggest::complete_local_with_cwd(
                &store,
                prefix,
                state.cfg.context.history_candidates.clamp(1, 20),
                cwd,
            )
            .into_iter()
            .filter(|candidate| {
                matches!(
                    candidate.source,
                    suggest::Source::History
                        | suggest::Source::HistoryPrefix
                        | suggest::Source::TerminalContext
                )
            })
            .map(|candidate| candidate.text)
            .collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    };
    request.model = model.clone();

    let reply = match state
        .http
        .complete(&provider, &model, credential.as_deref(), &request)
    {
        Ok(text) => text,
        Err(e) => {
            return diagnose(state, history(), e.to_string());
        }
    };

    let local = current_local();
    let mut ai_candidates = suggest::sanitize_candidates(
        &reply,
        prefix,
        suggest::suggestion_limit(modes.max_suggestions),
    );
    {
        let store = state.store.lock();
        ai_candidates.retain(|command| !suggest::superseded_context_command(&store, command, cwd));
    }
    if !selection_options.is_empty() {
        // AI selection is a reranker, never permission to invent another target.
        ai_candidates.retain(|command| selection_options.contains(command));
    }
    // Exact current-context continuations remain authoritative through model reranking.
    for candidate in local
        .iter()
        .rev()
        .filter(|candidate| candidate.source == suggest::Source::TerminalContext)
    {
        ai_candidates.retain(|command| command != &candidate.text);
        ai_candidates.insert(0, candidate.text.clone());
    }
    let Some(clean) = ai_candidates.first().cloned() else {
        if !selection_options.is_empty()
            || local
                .iter()
                .any(|candidate| candidate.source == suggest::Source::TerminalContext)
        {
            return history();
        }
        return diagnose(
            state,
            history(),
            "The model returned no usable command continuation.".into(),
        );
    };
    // The model may restate the whole command; only the tail is renderable.
    let Some(ghost_plain) = suggest::ghost_suffix(&clean, prefix) else {
        return diagnose(
            state,
            history(),
            "The model response did not extend the typed command.".into(),
        );
    };

    let ghost = state
        .anim
        .ghost(&ghost_plain, frame, mode == Mode::Aggressive)
        .into_owned();
    let mut candidates = ai_candidates;
    candidates.extend(local.into_iter().map(|c| c.text));
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|command| seen.insert(command.clone()));
    candidates.truncate(suggest::suggestion_limit(modes.max_suggestions));

    *state.last_issue.lock() = None;
    Response::Suggest {
        id,
        ghost,
        candidates,
        mode: mode.as_str().to_string(),
        source: suggest::Source::Llm.as_str().to_string(),
        ghost_plain,
        diagnostic: None,
    }
}

fn generate_reply(state: &State, id: u64, instruction: &str, cwd: Option<&Path>) -> Response {
    let mode = state.cfg.mode;
    let Some(instruction) = normalize_intent(instruction) else {
        return Response::Suggest {
            id,
            ghost: String::new(),
            candidates: Vec::new(),
            mode: mode.as_str().to_string(),
            source: "none".into(),
            ghost_plain: String::new(),
            diagnostic: Some("Enter a non-empty task description after `#`.".into()),
        };
    };

    let fallback = || Response::Suggest {
        id,
        ghost: String::new(),
        candidates: Vec::new(),
        mode: mode.as_str().to_string(),
        source: "none".into(),
        ghost_plain: String::new(),
        diagnostic: None,
    };
    if !judge_input_allowed(state, instruction) {
        return diagnose(
            state,
            fallback(),
            "Privacy rules kept this task local; sensitive text is not sent to models.".into(),
        );
    }
    let mut request = {
        let store = state.store.lock();
        suggest::build_intent_request_with_privacy(
            instruction,
            state.cfg.modes.max_suggestions,
            &store,
            &state.cfg.context,
            cwd,
            &state.cfg.decision.private_paths,
            state.decision.reviews_private_metadata(),
        )
    };
    let judge_request = local_judge_request(state, &request);
    request =
        crate::privacy::sanitize_request_with_paths(&request, &state.cfg.decision.private_paths);
    if state.cfg.decision.active() {
        let known = if state.cfg.decision.strategy == DecisionStrategy::Decision {
            let store = state.store.lock();
            let mut commands: Vec<String> = store
                .memory()
                .related(instruction, 8)
                .into_iter()
                .map(|(command, _)| command.to_string())
                .collect();
            commands.extend(
                store
                    .recent(8)
                    .into_iter()
                    .map(|entry| entry.command.clone()),
            );
            let mut seen = std::collections::HashSet::new();
            commands.retain(|command| {
                seen.insert(command.clone()) && judge_input_allowed(state, command)
            });
            commands.truncate(8);
            commands
        } else {
            Vec::new()
        };
        let decision = match evaluate_decision(state, &judge_request, &known, instruction, cwd) {
            Ok(decision) => decision,
            Err(error) => return diagnose(state, fallback(), error.to_string()),
        };
        if let Some(decision) = &decision {
            if state.cfg.decision.strategy == DecisionStrategy::Decision {
                let selected: Vec<_> = decision
                    .ranking
                    .iter()
                    .filter_map(|index| known.get(*index))
                    .map(|command| Suggestion {
                        text: command.clone(),
                        source: suggest::Source::History,
                        score: 0,
                    })
                    .collect();
                if selected.is_empty() {
                    return diagnose(state, fallback(), "Decision only found no suitable known command. Use Decision + AI to generate commands for a task.".into());
                }
                let mut response = from_local(id, &selected, "", state, 0, mode);
                if let Response::Suggest {
                    source, candidates, ..
                } = &mut response
                {
                    *source = "decision".into();
                    candidates.truncate(suggest::suggestion_limit(state.cfg.modes.max_suggestions));
                }
                *state.last_issue.lock() = None;
                return response;
            }
            if !decision.allow_remote {
                return diagnose(
                    state,
                    fallback(),
                    "The decision helper kept this task local for privacy.".into(),
                );
            }
            if crate::privacy::sensitive_with_paths(instruction, &state.cfg.decision.private_paths)
            {
                return diagnose(state, fallback(), "Private task metadata stays out of the generator; local review can select known commands.".into());
            }
        }
        if decision
            .as_ref()
            .is_none_or(|decision| decision.omit_context)
        {
            request = suggest::build_intent_request(instruction, state.cfg.modes.max_suggestions);
            request = crate::privacy::sanitize_request_with_paths(
                &request,
                &state.cfg.decision.private_paths,
            );
        }
    }
    let (provider, credential, model) = match model_credential(state) {
        Ok(resolved) => resolved,
        Err(issue) => return diagnose(state, fallback(), issue),
    };
    request.model = model.clone();
    let reply = match state
        .http
        .complete(&provider, &model, credential.as_deref(), &request)
    {
        Ok(reply) => reply,
        Err(error) => return diagnose(state, fallback(), error.to_string()),
    };
    let candidates = suggest::sanitize_intent_candidates(
        &reply,
        suggest::suggestion_limit(state.cfg.modes.max_suggestions),
    );
    let Some(first) = candidates.first() else {
        return diagnose(
            state,
            fallback(),
            "The model returned no usable shell command.".into(),
        );
    };

    let ghost_plain = first.clone();
    let ghost = state
        .anim
        .ghost(&ghost_plain, 0, mode == Mode::Aggressive)
        .into_owned();
    *state.last_issue.lock() = None;
    Response::Suggest {
        id,
        ghost,
        candidates,
        mode: mode.as_str().to_string(),
        source: suggest::Source::Llm.as_str().to_string(),
        ghost_plain,
        diagnostic: None,
    }
}

/// Hybrid may degrade to Standard AI for a safe active prompt under Block.
/// No history, output, directory metadata or learned preferences accompanies
/// that request. Required private-context review and explicit denials stay closed.
fn evaluate_decision(
    state: &State,
    request: &crate::provider::ChatRequest,
    candidates: &[String],
    active_text: &str,
    cwd: Option<&Path>,
) -> crate::Result<Option<crate::decision::Decision>> {
    let evidence = {
        let store = state.store.lock();
        let timestamp = crate::memory::now();
        candidates
            .iter()
            .map(|command| {
                let private_cwd = suggest::private_prompt_cwd(
                    cwd,
                    &state.cfg.decision.private_paths,
                    state.decision.reviews_private_metadata(),
                );
                let scope = if private_cwd {
                    crate::config::SuggestionScope::Global
                } else {
                    state.cfg.context.suggestion_scope
                };
                let evidence = (!private_cwd)
                    .then(|| suggest::candidate_context_evidence(&store, command, cwd))
                    .flatten()
                    .unwrap_or_else(|| {
                        store
                            .memory()
                            .candidate_evidence_with_scope(command, cwd, scope)
                    });
                let ranked = crate::memory::rank_evidence_at(&evidence, timestamp);
                crate::decision::CandidateEvidence::from_ranked(&evidence, &ranked, timestamp)
            })
            .collect::<Vec<_>>()
    };
    match state
        .decision
        .evaluate_with_evidence(request, candidates, &evidence)
    {
        Ok(decision) => Ok(Some(decision)),
        Err(error)
            if state.cfg.decision.strategy == DecisionStrategy::Hybrid
                && state.cfg.privacy.sensitive_context
                    == crate::config::SensitiveContext::Block
                && !crate::privacy::sensitive_with_paths(
                    active_text,
                    &state.cfg.decision.private_paths,
                ) =>
        {
            tracing::warn!(error = %error, "decision helper unavailable; generating from safe active prompt only");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn judge_input_allowed(state: &State, text: &str) -> bool {
    if state.decision.reviews_private_metadata() {
        !crate::privacy::hard_sensitive_with_paths(text, &state.cfg.decision.private_paths)
    } else {
        !crate::privacy::sensitive_with_paths(text, &state.cfg.decision.private_paths)
    }
}

fn local_judge_request(
    state: &State,
    request: &crate::provider::ChatRequest,
) -> crate::provider::ChatRequest {
    if state.decision.reviews_private_metadata() {
        crate::privacy::sanitize_for_review_with_paths(request, &state.cfg.decision.private_paths)
    } else {
        crate::privacy::sanitize_request_with_paths(request, &state.cfg.decision.private_paths)
    }
}

fn normalize_intent(instruction: &str) -> Option<&str> {
    let instruction = instruction.trim();
    let instruction = instruction.strip_prefix('#').unwrap_or(instruction).trim();
    if instruction.is_empty()
        || instruction.chars().count() > 1000
        || instruction.chars().any(char::is_control)
    {
        None
    } else {
        Some(instruction)
    }
}

fn from_local(
    id: u64,
    local: &[Suggestion],
    prefix: &str,
    state: &State,
    frame: usize,
    mode: Mode,
) -> Response {
    let Some(best) = local.first() else {
        return empty(id, mode, None);
    };
    let ghost_plain = best.suffix_for(prefix).to_string();
    if ghost_plain.is_empty() {
        return empty(id, mode, None);
    }
    let ghost = state
        .anim
        .ghost(&ghost_plain, frame, mode == Mode::Aggressive)
        .into_owned();
    Response::Suggest {
        id,
        ghost,
        candidates: local
            .iter()
            .take(suggest::suggestion_limit(state.cfg.modes.max_suggestions))
            .map(|c| c.text.clone())
            .collect(),
        mode: mode.as_str().to_string(),
        source: best.source.as_str().to_string(),
        ghost_plain,
        diagnostic: None,
    }
}

fn empty(id: u64, mode: Mode, diagnostic: Option<&str>) -> Response {
    Response::Suggest {
        id,
        ghost: String::new(),
        candidates: Vec::new(),
        mode: mode.as_str().to_string(),
        source: "none".into(),
        ghost_plain: String::new(),
        diagnostic: diagnostic.map(str::to_string),
    }
}

fn diagnose(state: &State, mut response: Response, issue: String) -> Response {
    let issue = if let Some(key) = state.credential.lock().as_deref() {
        issue.replace(key, "[REDACTED]")
    } else {
        issue
    };
    let issue = crate::redact::scrub(&issue);
    let mut previous = state.last_issue.lock();
    if previous.as_ref() != Some(&issue) {
        tracing::warn!(error = %issue, "AI suggestions unavailable");
    }
    *previous = Some(issue.clone());
    if let Response::Suggest { diagnostic, .. } = &mut response {
        *diagnostic = Some(issue);
    }
    response
}

fn resolve_mode(configured: Mode, override_str: Option<&str>) -> Mode {
    override_str.and_then(Mode::parse).unwrap_or(configured)
}

/// The path the daemon listens on, for diagnostics.
pub fn socket_display() -> PathBuf {
    config::socket_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Modes;

    static NEXT_TEST_STATE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn test_history_path() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "termplete-daemon-test-{}-{}",
            std::process::id(),
            NEXT_TEST_STATE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        directory.join("history.json")
    }

    fn test_state(mode: Mode) -> State {
        State {
            cfg: Config {
                mode,
                modes: Modes {
                    min_chars: 1,
                    ..Modes::default()
                },
                ..Config::default()
            },
            history_path: test_history_path(),
            store: Mutex::new(Store::new(crate::config::ContextConfig::default())),
            credential: Mutex::new(None),
            auth_loaded: AtomicBool::new(true),
            provider: None,
            http: HttpClient::default(),
            decision: crate::decision_runtime::Runtime::new(&Config::default()),
            anim: Anim::disabled(),
            last_issue: Mutex::new(None),
            persist_lock: Mutex::new(()),
        }
    }

    fn seed(state: &State, commands: &[&str]) {
        let mut store = state.store.lock();
        for c in commands {
            store.record(c, Some(0));
        }
    }

    fn read_body(reader: &mut impl std::io::BufRead) -> serde_json::Value {
        let mut size = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                size = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut bytes = vec![0; size];
        reader.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn mock_preset(address: std::net::SocketAddr) -> ProviderPreset {
        ProviderPreset {
            base_url: format!("http://{address}/v1"),
            no_auth: true,
            ..Config::default().provider(Some("openai")).unwrap()
        }
    }

    fn serve(
        status: &'static str,
        body: String,
    ) -> (ProviderPreset, std::thread::JoinHandle<serde_json::Value>) {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let request = read_body(&mut reader);
            write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            request
        });
        (mock_preset(address), worker)
    }

    fn mock_model(reply: &str) -> (ProviderPreset, std::thread::JoinHandle<serde_json::Value>) {
        let body = serde_json::json!({"choices": [{"message": {"content": reply}}]}).to_string();
        serve("200 OK", body)
    }

    fn mock_provider_error() -> (ProviderPreset, std::thread::JoinHandle<serde_json::Value>) {
        serve(
            "500 Internal Server Error",
            r#"{"error":{"message":"mock provider failure"}}"#.into(),
        )
    }

    fn optional_mock_model(reply: &str) -> (ProviderPreset, std::thread::JoinHandle<bool>) {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        server.set_nonblocking(true).unwrap();
        let body = serde_json::json!({"choices": [{"message": {"content": reply}}]}).to_string();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(400);
            let (mut socket, _) = loop {
                match server.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock listener failed: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let _ = read_body(&mut reader);
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            true
        });
        (mock_preset(address), worker)
    }

    fn directory_suggest_request(id: u64, cwd: &Path, line: &str, local_only: bool) -> Request {
        Request::Suggest {
            id,
            line: line.into(),
            cursor: None,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            mode: None,
            local_only,
            frame: None,
        }
    }

    #[test]
    fn newly_created_directory_wins_before_the_display_limit() {
        let directory = tempfile::Builder::new()
            .prefix("termplete-created-directory-")
            .tempdir()
            .unwrap();
        for name in ["nuxtmodulefork", "orch-test", "other"] {
            std::fs::create_dir(directory.path().join(name)).unwrap();
        }
        let mut state = test_state(Mode::Off);
        state.cfg.modes.max_suggestions = 1;
        {
            let mut store = state.store.lock();
            for _ in 0..20 {
                store.record_with_cwd("cd nuxtmodulefork", Some(0), Some(directory.path()));
                store.accept_suggestion_with_cwd(
                    "cd ",
                    "cd nuxtmodulefork/",
                    Some(directory.path()),
                );
            }
            store.record_with_cwd("mkdir orch-test     ", Some(0), Some(directory.path()));
        }
        for prefix in ["cd ", "cd ./", "pushd "] {
            let Response::Suggest {
                candidates, source, ..
            } = dispatch(
                &state,
                &directory_suggest_request(1, directory.path(), prefix, true),
            )
            else {
                panic!("suggestion expected");
            };
            assert_eq!(candidates, vec![format!("{prefix}orch-test/")]);
            assert_eq!(source, "terminal-context");
        }
        let Response::Suggest { candidates, .. } = dispatch(
            &state,
            &directory_suggest_request(2, directory.path(), "cd nu", true),
        ) else {
            panic!("typed target should win");
        };
        assert_eq!(candidates, vec!["cd nuxtmodulefork/"]);
        std::fs::remove_dir(directory.path().join("orch-test")).unwrap();
        let Response::Suggest { candidates, .. } = dispatch(
            &state,
            &directory_suggest_request(3, directory.path(), "cd ", true),
        ) else {
            panic!("fallback expected");
        };
        assert_eq!(candidates, vec!["cd nuxtmodulefork/"]);
    }

    #[test]
    fn directory_ranking_uses_recorded_destinations_before_the_display_limit() {
        let directory = tempfile::Builder::new()
            .prefix("termplete-directory-ranking-")
            .tempdir()
            .unwrap();
        for name in ["a-old", "b-old", "c-old", "z-active"] {
            std::fs::create_dir(directory.path().join(name)).unwrap();
        }
        let mut state = test_state(Mode::Off);
        state.cfg.modes.max_suggestions = 1;
        let cwd = directory.path().to_string_lossy().into_owned();
        for id in [1, 2] {
            dispatch(
                &state,
                &Request::Record {
                    id,
                    command: "cd z-active".into(),
                    cwd: Some(cwd.clone()),
                    exit_code: Some(0),
                    output: None,
                },
            );
        }
        let response = dispatch(
            &state,
            &directory_suggest_request(3, directory.path(), "cd ", true),
        );
        let Response::Suggest {
            candidates, source, ..
        } = response
        else {
            panic!("suggestions expected")
        };
        assert_eq!(candidates, vec!["cd z-active/"]);
        assert_eq!(source, "filesystem");
        let restored = Store::load_with(&state.history_path, &state.cfg).unwrap();
        assert_eq!(
            restored
                .memory()
                .candidate_evidence("cd ./z-active/", Some(directory.path()))
                .uses,
            2
        );
    }

    #[test]
    fn directory_candidates_precede_history_selection_and_local_only() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join("nuxt-local-model")).unwrap();
        let other_cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(other_cwd.path().join("nuxt-local-other")).unwrap();
        let mut state = test_state(Mode::Aggressive);
        state.cfg.context.ai_select_history = true;
        seed(&state, &["cd nuxt-local-history/"]);

        let response = dispatch(
            &state,
            &directory_suggest_request(1, cwd.path(), "cd nuxt-lo", false),
        );
        let Response::Suggest {
            candidates, source, ..
        } = response
        else {
            panic!("expected suggestions");
        };
        assert_eq!(
            candidates.iter().map(String::as_str).collect::<Vec<_>>(),
            ["cd nuxt-local-model/"]
        );
        assert_eq!(source, "filesystem");
        let response = dispatch(
            &state,
            &directory_suggest_request(2, other_cwd.path(), "cd nuxt-lo", true),
        );
        assert_eq!(source_of(&response), "filesystem");
        let Response::Suggest { candidates, .. } = &response else {
            panic!("expected suggestions");
        };
        assert_eq!(
            candidates.iter().map(String::as_str).collect::<Vec<_>>(),
            ["cd nuxt-local-other/"]
        );
    }

    #[test]
    fn unmatched_directory_prefix_returns_empty_without_model_fallback() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join("existing-folder")).unwrap();
        let state = test_state(Mode::Aggressive);

        let response = dispatch(
            &state,
            &directory_suggest_request(2, cwd.path(), "cd missing", false),
        );
        let Response::Suggest {
            candidates,
            source,
            diagnostic,
            ..
        } = response
        else {
            panic!("expected suggestions");
        };
        assert!(candidates.is_empty());
        assert_eq!(source, "none");
        assert_eq!(diagnostic.as_deref(), Some("No matching local directory."));
    }

    #[test]
    fn history_first_uses_usual_target_without_credentials_or_model() {
        let mut state = test_state(Mode::Aggressive);
        let mut provider = state.cfg.provider(Some("openai")).unwrap();
        provider.name = "history-only-test".into();
        state.provider = Some(provider);
        state.auth_loaded.store(false, Ordering::Relaxed);
        seed(
            &state,
            &[
                "ssh root@203.0.113.7",
                "ssh root@203.0.113.7",
                "ssh test@203.0.113.9",
            ],
        );
        let response = suggest_reply(&state, 1, "ssh ", None, None, false, 0);
        assert_eq!(ghost_of(&response), "root@203.0.113.7");
        assert_eq!(source_of(&response), "history");
        assert!(!state.auth_loaded.load(Ordering::Relaxed));
        assert!(state.credential.lock().is_none());
        assert!(
            matches!(response, Response::Suggest { diagnostic: None, candidates, .. } if candidates.len() == 2)
        );
    }

    #[test]
    fn model_retrieves_old_ssh_targets_beyond_recent_context() {
        let (provider, worker) = mock_model("ssh -o ConnectTimeout=10 root@203.0.113.7");
        let mut state = test_state(Mode::Aggressive);
        state.cfg.context.history_entries = 2;
        state.store = Mutex::new(Store::new(state.cfg.context.clone()));
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());
        seed(&state, &["ssh root@203.0.113.7"]);
        for index in 0..10 {
            seed(&state, &[&format!("echo {index}")]);
        }
        let response = suggest_reply(&state, 1, "ssh -o", None, None, false, 0);
        assert_eq!(ghost_of(&response), " ConnectTimeout=10 root@203.0.113.7");
        let request = worker.join().unwrap();
        let user = request["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("Relevant command memory"));
        assert!(user.contains("ssh root@203.0.113.7"));
        assert!(
            !user.contains("$ ssh root@203.0.113.7"),
            "target should come from retrieval, not recent output context"
        );
    }

    #[test]
    fn optional_ai_selection_can_choose_another_known_target_but_never_invent_one() {
        for (reply, limit) in [
            ("ssh test@203.0.113.9", 8),
            ("ssh invented@203.0.113.99", 8),
            ("ssh invented@203.0.113.99", 0),
        ] {
            let (provider, worker) = mock_model(reply);
            let mut state = test_state(Mode::Aggressive);
            state.cfg.context.ai_select_history = true;
            state.cfg.context.history_candidates = limit;
            state.cfg.model = Some("test-model".into());
            state.provider = Some(provider);
            seed(
                &state,
                &[
                    "ssh root@203.0.113.7",
                    "ssh root@203.0.113.7",
                    "ssh test@203.0.113.9",
                ],
            );
            let response = suggest_reply(&state, 1, "ssh ", None, None, false, 0);
            assert_eq!(
                ghost_of(&response),
                if reply.contains("invented") {
                    "root@203.0.113.7"
                } else {
                    "test@203.0.113.9"
                }
            );
            if let Response::Suggest { candidates, .. } = response {
                assert_eq!(
                    candidates
                        .iter()
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    candidates.len()
                );
                assert!(!candidates
                    .iter()
                    .any(|candidate| candidate.contains("invented")));
            }
            let request = worker.join().unwrap();
            assert!(request["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("Choose EXACTLY one"));
        }
    }

    fn ghost_of(res: &Response) -> &str {
        match res {
            Response::Suggest { ghost_plain, .. } => ghost_plain,
            other => panic!("expected a suggest reply, got {other:?}"),
        }
    }

    fn source_of(res: &Response) -> &str {
        match res {
            Response::Suggest { source, .. } => source,
            _ => "",
        }
    }

    fn mode_of(res: &Response) -> &str {
        match res {
            Response::Suggest { mode, .. } => mode,
            _ => "",
        }
    }

    #[test]
    fn mode_override_wins_over_config() {
        assert_eq!(
            resolve_mode(Mode::Subtle, Some("aggressive")),
            Mode::Aggressive
        );
        assert_eq!(resolve_mode(Mode::Subtle, Some("nonsense")), Mode::Subtle);
        assert_eq!(resolve_mode(Mode::Aggressive, None), Mode::Aggressive);
    }

    #[test]
    fn too_short_input_yields_empty_ghost() {
        let mut state = test_state(Mode::Aggressive);
        state.cfg.modes = Modes {
            min_chars: 5,
            ..Modes::default()
        };
        seed(&state, &["git status"]);
        let res = suggest_reply(&state, 1, "gi", None, None, false, 0);
        assert!(ghost_of(&res).is_empty());
    }

    #[test]
    fn off_mode_never_calls_the_model() {
        let state = test_state(Mode::Off);
        seed(&state, &["git status"]);
        let res = suggest_reply(&state, 1, "git st", None, None, false, 0);
        assert_eq!(ghost_of(&res), "atus");
        assert_eq!(source_of(&res), "history");
    }

    #[test]
    fn local_only_forces_history_even_in_eager_mode() {
        let state = test_state(Mode::Aggressive);
        seed(&state, &["git status"]);
        let res = suggest_reply(&state, 1, "git st", None, None, true, 0);
        assert_eq!(ghost_of(&res), "atus");
        assert_eq!(mode_of(&res), "automatic");
    }

    #[test]
    fn no_provider_falls_back_to_history() {
        let state = test_state(Mode::Aggressive);
        seed(&state, &["docker ps"]);
        let res = suggest_reply(&state, 1, "docker p", None, None, false, 0);
        assert_eq!(ghost_of(&res), "s");
        assert_eq!(source_of(&res), "history");
    }

    #[test]
    fn git_log_is_available_without_history_or_a_provider() {
        let state = test_state(Mode::Aggressive);
        let res = suggest_reply(&state, 1, "git l", None, None, false, 0);
        assert_eq!(ghost_of(&res), "og");
        assert_eq!(source_of(&res), "builtin");
        assert!(matches!(
            res,
            Response::Suggest {
                diagnostic: None,
                ..
            }
        ));
    }

    #[test]
    fn bare_ffmpeg_gets_a_continuation_without_model_setup() {
        let state = test_state(Mode::Aggressive);
        let res = suggest_reply(&state, 1, "ffmpeg", None, None, false, 0);
        assert_eq!(ghost_of(&res), " -i input.mp4 output.mp4");
        assert_eq!(source_of(&res), "builtin");
        assert!(matches!(
            res,
            Response::Suggest {
                diagnostic: None,
                ..
            }
        ));
    }

    #[test]
    fn missing_credentials_report_why_the_model_is_unavailable() {
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(state.cfg.provider(Some("mistral")).unwrap());
        let res = suggest_reply(&state, 1, "docker p", None, None, false, 0);
        match res {
            Response::Suggest {
                diagnostic: Some(issue),
                ..
            } => assert!(issue.contains("No API key")),
            other => panic!("missing diagnostic: {other:?}"),
        }
        let status = dispatch(&state, &Request::Ping { id: 2 });
        assert!(matches!(status, Response::Ok { note: Some(note), .. } if note.contains("issue=")));
    }

    #[test]
    fn provider_failures_are_reported_and_credentials_are_redacted() {
        let (provider, worker) = serve(
            "401 Unauthorized",
            r#"{"error":{"message":"invalid key test-secret-value"}}"#.into(),
        );
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());
        *state.credential.lock() = Some("test-secret-value".into());
        state.cfg.context.ai_select_history = true;
        seed(&state, &["docker ps"]);
        let res = suggest_reply(&state, 1, "docker p", None, None, false, 0);
        assert_eq!(ghost_of(&res), "s");
        match res {
            Response::Suggest {
                diagnostic: Some(issue),
                ..
            } => {
                assert!(issue.contains("401"), "{issue}");
                assert!(!issue.contains("test-secret-value"));
            }
            other => panic!("missing diagnostic: {other:?}"),
        }
        worker.join().unwrap();
    }

    #[test]
    fn model_contract_returns_inline_suffix_and_preserves_prefix_spacing() {
        let (provider, worker) = serve(
            "200 OK",
            r#"{"choices":[{"message":{"content":"docker  ps"}}]}"#.into(),
        );
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());
        *state.last_issue.lock() = Some("previous error".into());
        let res = suggest_reply(&state, 1, "docker  p", None, None, false, 0);
        assert_eq!(ghost_of(&res), "s");
        assert_eq!(source_of(&res), "llm");
        assert!(matches!(
            res,
            Response::Suggest {
                diagnostic: None,
                ..
            }
        ));
        assert!(state.last_issue.lock().is_none());
        let request = worker.join().unwrap();
        let system = request["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("full completed command"));
        assert!(!system.contains("Never repeat"));
        assert!(system.contains("Always propose one useful continuation"));
        assert!(system.contains("Never explain your guess and never return an empty reply"));
        let user = request["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("$ docker  p"));
    }

    #[test]
    fn model_response_returns_distinct_valid_alternatives_from_one_call() {
        let (provider, worker) = mock_model(
            "not a docker command\ndocker ps\ndocker ps\ndocker images\ndocker compose up",
        );
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());

        let response = suggest_reply(&state, 1, "docker ", None, None, false, 0);
        assert_eq!(ghost_of(&response), "ps");
        assert_eq!(source_of(&response), "llm");
        assert!(matches!(
            response,
            Response::Suggest {
                candidates,
                diagnostic: None,
                ..
            } if candidates == ["docker ps", "docker images", "docker compose up"]
        ));

        let request = worker.join().unwrap();
        let user = request["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("up to 3 distinct full commands"));
        assert_eq!(request["stop"], serde_json::json!(["\r"]));
        assert_eq!(request["max_tokens"], 192);
    }

    #[test]
    fn ai_history_selection_discards_every_nonmember_alternative() {
        let (provider, worker) =
            mock_model("ssh invented@203.0.113.99\nssh test@203.0.113.9\nssh root@203.0.113.7");
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());
        state.cfg.context.ai_select_history = true;
        seed(
            &state,
            &[
                "ssh root@203.0.113.7",
                "ssh root@203.0.113.7",
                "ssh test@203.0.113.9",
            ],
        );

        let response = suggest_reply(&state, 1, "ssh ", None, None, false, 0);
        assert_eq!(ghost_of(&response), "test@203.0.113.9");
        assert!(matches!(
            response,
            Response::Suggest { candidates, .. }
                if candidates == ["ssh test@203.0.113.9", "ssh root@203.0.113.7"]
        ));
        let request = worker.join().unwrap();
        assert!(request["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Choose EXACTLY one"));
    }

    #[test]
    fn generate_uses_task_description_and_recent_terminal_context_without_execution() {
        let (provider, worker) =
            mock_model("$ find . -type f -size +100M\n$ du -ah . | sort -h | tail -n 20");
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());
        let cwd = tempfile::Builder::new()
            .prefix("termplete-public-test-")
            .tempdir()
            .unwrap();
        std::fs::create_dir(cwd.path().join("actual-project")).unwrap();
        state.store.lock().record_with_output_and_cwd(
            "tool check",
            Some(1),
            "previous-result-sentinel",
            Some(cwd.path()),
        );

        let response = dispatch(
            &state,
            &Request::Generate {
                id: 27,
                cwd: Some(cwd.path().to_string_lossy().into_owned()),
                instruction: "# find large files".into(),
            },
        );
        assert!(matches!(
            response,
            Response::Suggest {
                id: 27,
                ref candidates,
                ref ghost_plain,
                ref source,
                diagnostic: None,
                ..
            } if candidates == &["find . -type f -size +100M", "du -ah . | sort -h | tail -n 20"]
                && ghost_plain == "find . -type f -size +100M"
                && source == "llm"
        ));
        assert_eq!(
            state.store.lock().len(),
            1,
            "generation must not execute or record commands"
        );

        let request = worker.join().unwrap();
        let system = request["messages"][0]["content"].as_str().unwrap();
        let user = request["messages"][1]["content"].as_str().unwrap();
        assert!(system.contains("Use the task description as the goal"));
        assert!(user.contains("find large files"));
        assert!(user.contains("previous-result-sentinel"));
        assert!(user.contains("actual-project"));
        assert!(user.contains("[exit 1]"));
        assert!(user.contains("Recent session"));
    }

    #[test]
    fn bare_tools_use_contextual_ai_and_keep_three_fallback_options() {
        for (tool, filename, reply) in [
            (
                "docker",
                "project-compose.yml",
                "docker compose -f project-compose.yml ps",
            ),
            ("ffmpeg", "clip.webm", "ffmpeg -i clip.webm preview.mp4"),
        ] {
            let cwd = tempfile::Builder::new()
                .prefix("termplete-public-test-")
                .tempdir()
                .unwrap();
            std::fs::write(cwd.path().join(filename), "not read").unwrap();
            let (provider, worker) = mock_model(reply);
            let mut state = test_state(Mode::Aggressive);
            state.provider = Some(provider);
            state.cfg.model = Some("test-model".into());
            assert!(state.cfg.modes.skip_for_common_commands);
            let response = dispatch(
                &state,
                &directory_suggest_request(8, cwd.path(), tool, false),
            );
            let Response::Suggest {
                candidates, source, ..
            } = response
            else {
                panic!("expected suggestions");
            };
            assert_eq!(source, "llm");
            assert_eq!(candidates.first().map(String::as_str), Some(reply));
            assert_eq!(candidates.len(), 3);
            let request = worker.join().unwrap();
            assert!(request["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains(filename));
        }
    }

    #[test]
    fn generate_rejects_empty_task_without_calling_provider() {
        let (provider, worker) = optional_mock_model("$ ls");
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());
        let response = dispatch(
            &state,
            &Request::Generate {
                id: 28,
                cwd: None,
                instruction: "#   ".into(),
            },
        );
        assert!(matches!(
            response,
            Response::Suggest {
                id: 28,
                candidates,
                source,
                diagnostic: Some(message),
                ..
            } if candidates.is_empty()
                && source == "none"
                && message.contains("task description")
        ));
        assert!(
            !worker.join().unwrap(),
            "invalid tasks must not reach the provider"
        );
    }

    #[test]
    fn generate_reports_provider_errors_as_suggestion_diagnostics() {
        let (provider, worker) = mock_provider_error();
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());
        let response = dispatch(
            &state,
            &Request::Generate {
                id: 29,
                cwd: None,
                instruction: "show disk usage".into(),
            },
        );
        assert!(matches!(
            response,
            Response::Suggest {
                id: 29,
                candidates,
                diagnostic: Some(message),
                ..
            } if candidates.is_empty() && message.contains("500")
        ));
        let request = worker.join().unwrap();
        assert!(request["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("show disk usage"));
    }

    #[test]
    fn cursor_mid_line_completes_the_prefix() {
        let state = test_state(Mode::Off);
        seed(&state, &["git status --short"]);
        let res = suggest_reply(&state, 1, "git status --short", Some(3), None, false, 0);
        assert_eq!(ghost_of(&res), " status --short");
    }

    #[test]
    fn unmatched_prefix_yields_empty_ghost() {
        let state = test_state(Mode::Off);
        seed(&state, &["git status"]);
        let res = suggest_reply(&state, 1, "zzz", None, None, false, 0);
        assert!(ghost_of(&res).is_empty());
        assert!(matches!(&res, Response::Suggest { candidates, .. } if candidates.is_empty()));
    }

    #[test]
    fn mode_override_is_reported_in_the_reply() {
        let state = test_state(Mode::Off);
        seed(&state, &["ls"]);
        let res = suggest_reply(&state, 1, "l", None, Some("aggressive"), false, 0);
        assert_eq!(mode_of(&res), "automatic");
    }

    #[test]
    fn record_updates_the_store() {
        let state = test_state(Mode::Off);
        let res = dispatch(
            &state,
            &Request::Record {
                cwd: None,
                id: 1,
                command: "terraform apply".into(),
                exit_code: Some(0),
                output: Some("Apply complete!".into()),
            },
        );
        assert!(matches!(res, Response::Ok { .. }));
        let store = state.store.lock();
        let e = store.entries().next().unwrap();
        assert_eq!(e.command, "terraform apply");
        assert_eq!(e.output, vec!["Apply complete!"]);
    }

    #[test]
    fn feedback_is_acknowledged_ranked_and_persisted_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        let mut state = test_state(Mode::Off);
        state.history_path = path.clone();
        let request = Request::Feedback {
            cwd: None,
            id: 7,
            prefix: "docker p".into(),
            command: "docker ps --all".into(),
        };
        assert_eq!(dispatch(&state, &request), Response::Ack { id: 7 });

        let response = suggest_reply(&state, 8, "docker p", None, None, false, 0);
        assert_eq!(ghost_of(&response), "s --all");
        assert_eq!(source_of(&response), "history");

        let restored = Store::load_with(&path, &state.cfg).unwrap();
        assert_eq!(
            restored.memory().prefix("docker p", 1)[0].0,
            "docker ps --all"
        );
    }

    #[test]
    fn failed_record_invalidates_a_choice_and_success_restores_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        let mut state = test_state(Mode::Off);
        state.history_path = path.clone();
        for id in [1, 2] {
            let response = dispatch(
                &state,
                &Request::Feedback {
                    cwd: None,
                    id,
                    prefix: "docker p".into(),
                    command: "docker ps --all".into(),
                },
            );
            assert_eq!(response, Response::Ack { id });
        }
        assert!(state.store.lock().memory().has_trusted_choice("docker p"));

        dispatch(
            &state,
            &Request::Record {
                cwd: None,
                id: 3,
                command: "docker ps --all".into(),
                exit_code: Some(1),
                output: None,
            },
        );
        let restored = Store::load_with(&path, &state.cfg).unwrap();
        assert!(!restored.memory().has_trusted_choice("docker p"));
        assert!(restored.memory().prefix("docker p", 3).is_empty());

        dispatch(
            &state,
            &Request::Record {
                cwd: None,
                id: 4,
                command: "docker ps --all".into(),
                exit_code: Some(0),
                output: None,
            },
        );
        let restored = Store::load_with(&path, &state.cfg).unwrap();
        assert!(restored.memory().has_trusted_choice("docker p"));
        assert_eq!(
            restored.memory().prefix("docker p", 1)[0].0,
            "docker ps --all"
        );
    }

    #[test]
    fn repeated_user_choice_skips_the_optional_ai_history_model_call() {
        let (provider, worker) = optional_mock_model("docker images");
        let mut state = test_state(Mode::Aggressive);
        state.provider = Some(provider);
        state.cfg.model = Some("test-model".into());
        state.cfg.context.ai_select_history = true;
        {
            let mut store = state.store.lock();
            store.accept_suggestion("docker p", "docker ps");
            store.accept_suggestion("docker p", "docker ps");
        }

        let response = suggest_reply(&state, 9, "docker p", None, None, false, 0);
        assert_eq!(ghost_of(&response), "s");
        assert_eq!(source_of(&response), "history");
        assert!(matches!(
            response,
            Response::Suggest {
                diagnostic: None,
                ..
            }
        ));
        assert!(
            !worker.join().unwrap(),
            "trusted preference should avoid HTTP"
        );
    }

    #[test]
    fn ping_reports_diagnostics() {
        let state = test_state(Mode::Subtle);
        seed(&state, &["ls"]);
        let res = dispatch(&state, &Request::Ping { id: 3 });
        match res {
            Response::Ok { id, note } => {
                assert_eq!(id, 3);
                let note = note.unwrap();
                assert!(note.contains("entries=1"), "{note}");
                assert!(note.contains("mode=on-demand"), "{note}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn global_pause_blocks_requests_but_allows_ping_and_shutdown() {
        let mut state = test_state(Mode::Subtle);
        state.cfg.enabled = Some(false);

        let suggest = dispatch(
            &state,
            &Request::Suggest {
                id: 4,
                line: "git ".into(),
                cursor: None,
                cwd: None,
                mode: None,
                local_only: false,
                frame: None,
            },
        );
        assert!(matches!(suggest, Response::Error { id: 4, .. }));

        let generate = dispatch(
            &state,
            &Request::Generate {
                id: 5,
                instruction: "list files".into(),
                cwd: None,
            },
        );
        assert!(matches!(generate, Response::Error { id: 5, .. }));
        assert!(matches!(
            dispatch(&state, &Request::Ping { id: 6 }),
            Response::Ok { id: 6, .. }
        ));
        assert!(matches!(
            dispatch(&state, &Request::Shutdown { id: 7 }),
            Response::Ok { id: 7, .. }
        ));
    }

    #[test]
    fn output_is_redacted_on_record() {
        let state = test_state(Mode::Off);
        dispatch(
            &state,
            &Request::Record {
                cwd: None,
                id: 1,
                command: "deploy".into(),
                exit_code: Some(0),
                output: Some("AWS_SECRET_ACCESS_KEY=wompus".into()),
            },
        );
        let store = state.store.lock();
        assert!(!store
            .entries()
            .next()
            .unwrap()
            .output
            .join(" ")
            .contains("wompus"));
    }

    #[test]
    fn empty_replies_carry_a_none_source() {
        let res = empty(1, Mode::Off, None);
        assert!(
            matches!(&res, Response::Suggest { source, ghost, .. } if source == "none" && ghost.is_empty())
        );
    }
}
