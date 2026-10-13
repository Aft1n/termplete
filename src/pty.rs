//! Batteries-included mode: run a real shell inside a PTY and capture what it
//! prints.
//!
//! The shell plugins see only the input line. This wrapper sees everything —
//! the prompt, the command, and its output — which is what lets the model reason
//! about *results*, not just history. It is the closest thing to Warp's command
//! block capture that a drop-in can offer without owning your terminal.
//!
//! Two details make this work in practice:
//!
//! - The shell is launched with the termplete integration already sourced, so
//!   ghost text still renders through the normal plugin path.
//! - Output is echoed to the terminal verbatim and *also* parsed to recover the
//!   last command line and its exit status. Parsing is best-effort: the PTY
//!   stream has no framing, so we anchor on the prompt we asked the shell to
//!   emit via `PROMPT_COMMAND` / `precmd`.

use parking_lot::Mutex;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::context::Store;
use crate::shellutil::strip_ansi;

#[path = "pty_publish.rs"]
mod publication;
#[path = "pty_stream.rs"]
mod stream;

/// Options for the wrapper.
#[derive(Debug, Clone)]
pub struct Opts {
    /// Shell to run; defaults to `$SHELL`.
    pub shell: Option<String>,
    /// Extra arguments passed to the shell.
    pub args: Vec<String>,
    /// Rows, when the caller does not detect them.
    pub rows: u16,
    pub cols: u16,
    /// Run this instead of the login shell.
    pub command: Option<String>,
    /// Forward captured commands to the daemon instead of writing history directly.
    pub forward_to_daemon: bool,
}

/// Marker the wrapped shell prints after each command, framing the PTY stream so
/// commands and their output can be told apart.
pub const PROMPT_MARKER: &str = "\u{1}tp\u{1}";

/// Prefix for private OSC frames written by the zsh integration.
pub const OSC_FRAME_PREFIX: &str = "\u{1b}]777;termplete;";

/// Hard maximum amount of command output retained from one PTY frame.
pub const MAX_CAPTURE_OUTPUT_BYTES: usize = 1024 * 1024;

const CAPTURE_PUBLISH_ACK_GRACE: Duration = Duration::from_millis(100);

fn capture_publish_timeout() -> Duration {
    let timeout_ms = crate::config::Config::load()
        .ok()
        .map_or(500, |config| config.daemon.connect_timeout_ms)
        .max(500);
    Duration::from_millis(timeout_ms)
}

/// `PROMPT_MARKER` as an ANSI-C-quoted shell word. `echo $'\001tp\001'` emits
/// the two raw control bytes, which is what the reader thread splits on; a plain
/// `printf '\\001'` would print the backslashes literally instead.
impl Default for Opts {
    fn default() -> Self {
        Self {
            shell: None,
            args: Vec::new(),
            rows: 24,
            cols: 80,
            command: None,
            forward_to_daemon: true,
        }
    }
}

/// The shell prologue that makes the PTY stream parseable.
///
/// It installs a `precmd`-style hook which, after every command, prints the exit
/// status followed by [`PROMPT_MARKER`]. The reader thread uses the marker as a
/// frame boundary. Nothing is visible to the user: the marker is a pair of
/// control characters the terminal renders as nothing.
fn marker_for_token(token: &str) -> String {
    format!("\u{1}tp-{token}\u{1}")
}

fn instrumentation(shell: &str, token: &str) -> String {
    let define = format!(
        "__tp_capture_marker=$(printf '\\001tp-{token}\\001')\ntypeset +x __tp_capture_marker\n"
    );
    let print = "printf '[exit %d]\\n%s\\n' \"$?\" \"$__tp_capture_marker\"";
    if shell_name_is_zsh(shell) {
        format!(
            "{define}__tp_frame() {{ {print}; }}\nprecmd_functions=(__tp_frame $precmd_functions)"
        )
    } else {
        format!("{define}__tp_frame() {{ {print}; }}\nPROMPT_COMMAND=__tp_frame\"${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}\"")
    }
}

/// Single-quote `s` for safe interpolation into a shell script.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            // Close the quoted run, emit an escaped quote, reopen.
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn shell_name_is_zsh(shell: &str) -> bool {
    std::path::Path::new(shell)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .map(|n| n == "zsh" || n.starts_with("zsh-5"))
        .unwrap_or(false)
}

/// What the reader thread recovered from the PTY stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Captured {
    /// The command line most recently submitted.
    pub command: String,
    /// Working directory when the command started, when available.
    pub cwd: Option<String>,
    /// Exit status when the shell exposed one.
    pub exit_code: Option<i32>,
    /// The newest bounded output printed after the command was submitted.
    pub output: String,
}

/// Encode a hidden OSC frame announcing the exact command and its directory.
///
/// The shell integration writes this immediately before command execution;
/// `StreamParser` removes it from the live terminal and uses it as the capture
/// boundary.
pub fn frame_start(command: &str, cwd: &str, token: &str) -> String {
    let payload = serde_json::json!({
        "event": "start",
        "token": token,
        "command": command,
        "cwd": cwd,
    });
    format!("{OSC_FRAME_PREFIX}{payload}\u{7}")
}

/// Encode a hidden OSC frame announcing the command's exit status.
pub fn frame_end(code: i32, token: &str) -> String {
    let payload = serde_json::json!({ "event": "end", "code": code, "token": token });
    format!("{OSC_FRAME_PREFIX}{payload}\u{7}")
}

fn terminal_size() -> Option<PtySize> {
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) != 0
            || size.ws_row == 0
            || size.ws_col == 0
        {
            return None;
        }
        Some(PtySize {
            rows: size.ws_row,
            cols: size.ws_col,
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        })
    }
}

/// Install hooks in the shell that actually runs, after its normal rc file.
fn startup_file(
    shell: &str,
    dir: &std::path::Path,
    token: &str,
) -> std::io::Result<std::path::PathBuf> {
    let original = if shell_name_is_zsh(shell) {
        std::env::var_os("ZDOTDIR")
            .map(std::path::PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_default()
    } else {
        dirs::home_dir().unwrap_or_default()
    };
    let mut startup = String::new();
    if shell_name_is_zsh(shell) {
        startup.push_str(&format!(
            "typeset -g _TP_FRAME_TOKEN={}\ntypeset +x _TP_FRAME_TOKEN\nexport TERMPLETE_CAPTURE_ACTIVE=0\n",
            shell_single_quote(token)
        ));
        let env_file = original.join(".zshenv");
        std::fs::write(
            dir.join(".zshenv"),
            format!(
                "typeset +x TERMPLETE_CAPTURE_WRAPPED\n[[ -f {0} ]] && source {0}\nexport ZDOTDIR={1}\n",
                shell_single_quote(&env_file.to_string_lossy()),
                shell_single_quote(&dir.to_string_lossy())
            ),
        )?;
        // macOS /etc/zshrc sets HISTFILE from ZDOTDIR before our rc runs.
        // Rebase that default before sourcing the user's rc: Oh My Zsh keeps
        // an existing HISTFILE, which would otherwise isolate history in the
        // disposable capture directory. Explicit rc overrides still win.
        startup.push_str(&format!(
            "export ZDOTDIR={0}\nif [[ $HISTFILE == {1} ]]; then\n  HISTFILE={2}\nfi\n",
            shell_single_quote(&original.to_string_lossy()),
            shell_single_quote(&dir.join(".zsh_history").to_string_lossy()),
            shell_single_quote(&original.join(".zsh_history").to_string_lossy())
        ));
    }
    let rc = original.join(if shell_name_is_zsh(shell) {
        ".zshrc"
    } else {
        ".bashrc"
    });
    let quoted = shell_single_quote(&rc.to_string_lossy());
    startup.push_str(&format!("[ ! -f {quoted} ] || . {quoted}\n"));
    if shell_name_is_zsh(shell) {
        startup.push_str(&format!(
            "typeset -g _TP_FRAME_TOKEN={}\ntypeset +x _TP_FRAME_TOKEN\nexport TERMPLETE_CAPTURE_ACTIVE=1\n",
            shell_single_quote(token)
        ));
    }
    let plugin = if shell_name_is_zsh(shell) {
        include_str!("../shells/termplete.plugin.zsh")
    } else {
        include_str!("../shells/termplete.plugin.bash")
    };
    let loaded = if shell_name_is_zsh(shell) {
        "_TERMPLETE_ZSH_LOADED"
    } else {
        "_TERMPLETE_BASH_LOADED"
    };
    startup.push_str(&format!(
        "if [ -z \"${{{loaded}:-}}\" ]; then\n{plugin}\nfi\n"
    ));
    // The native zsh hooks frame commands with hidden OSC records. Keeping the
    // older prompt marker here would print a visible exit-status line and make
    // prompt text part of the capture. Bash still uses the legacy parser.
    if !shell_name_is_zsh(shell) {
        startup.push_str(&instrumentation(shell, token));
    }
    let path = dir.join(if shell_name_is_zsh(shell) {
        ".zshrc"
    } else {
        "bashrc"
    });
    std::fs::write(&path, startup)?;
    Ok(path)
}

/// Run the wrapper, blocking until the shell exits.
pub fn run(opts: Opts, store: Arc<Mutex<Store>>) -> crate::Result<()> {
    let output_limit = publication::output_capture_limit(store.lock().config());
    let publish_ack_timeout = capture_publish_timeout()
        .saturating_mul(2)
        .saturating_add(CAPTURE_PUBLISH_ACK_GRACE);
    let system = native_pty_system();
    let pair = system
        .openpty(terminal_size().unwrap_or(PtySize {
            rows: opts.rows,
            cols: opts.cols,
            pixel_width: 0,
            pixel_height: 0,
        }))
        .map_err(|e| crate::Error::Io(std::io::Error::other(e.to_string())))?;

    let shell = opts.shell.unwrap_or_else(shellutil_path);
    let mut cmd = CommandBuilder::new(shell.clone());
    // portable-pty defaults to HOME. A transparent observer must keep the
    // directory selected by the terminal or the shell that launched it.
    cmd.cwd(std::env::current_dir()?);
    let mut token_bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut token_bytes)?;
    let frame_token = token_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let prompt_marker = marker_for_token(&frame_token);
    let prologue = instrumentation(&shell, &frame_token);
    let startup_dir = tempfile::tempdir()?;

    match &opts.command {
        // A one-shot command never becomes an interactive session, so nothing
        // would install the prompt hook and no marker would ever be emitted.
        // Install the hook and then frame the command explicitly, so the reader
        // thread still sees a delimited command and its output.
        Some(script) => {
            let script_echo = shell_single_quote(script);
            let print_marker = format!("printf '\\001tp-{frame_token}\\001\\n'");
            // The marker must reach the stream as two real 0x01 bytes. `printf`
            // without %b would print the backslash escape literally, and a bare
            // `printf '\001'` only works as a command, not as shell source, so
            // use the shell's own $'...' quoting to emit the raw bytes.
            // The reader recovers the command from the segment's first line, so
            // one-shot mode has to actually emit it. An interactive shell echoes
            // the typed line for free; `bash -c` never does.
            let framed =
                format!("{prologue}\n{print_marker}\necho {script_echo}\n{script}\n__tp_frame\n");
            cmd.args(["-c", &framed]);
        }
        // Interactive: the shell sources its rc, which runs PROMPT_COMMAND (or
        // precmd) after every command, emitting the marker for us.
        None => {
            let name = std::path::Path::new(&shell).file_name().unwrap_or_default();
            if shell_name_is_zsh(&shell) || name == "bash" {
                let rc = startup_file(&shell, startup_dir.path(), &frame_token)?;
                cmd.env("_TP_BIN", std::env::current_exe()?);
                if shell_name_is_zsh(&shell) {
                    // Prevent the user's rc from recursively wrapping this
                    // child while capture is deliberately disabled at startup.
                    cmd.env("TERMPLETE_CAPTURE_WRAPPED", "1");
                    cmd.env("TERMPLETE_CAPTURE_ACTIVE", "0");
                    cmd.env("ZDOTDIR", startup_dir.path());
                    cmd.arg("-i");
                } else {
                    cmd.args(["--rcfile", &rc.to_string_lossy(), "-i"]);
                }
            } else {
                cmd.arg("-i");
            }
        }
    }
    for a in &opts.args {
        cmd.arg(a);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| crate::Error::Io(std::io::Error::other(e.to_string())))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| crate::Error::Io(std::io::Error::other(e.to_string())))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| crate::Error::Io(std::io::Error::other(e.to_string())))?;

    let _terminal = crate::shellutil::RawTerminal::enter(false)?;
    let done = Arc::new(AtomicBool::new(false));
    let reader_done = Arc::clone(&done);
    let (capture_tx, capture_rx) = mpsc::channel::<(Captured, Option<mpsc::Sender<()>>)>();
    let store_for_recorder = Arc::clone(&store);
    let daemon_forward = opts.forward_to_daemon;
    let recorder = thread::spawn(move || {
        for (cap, published) in capture_rx {
            publication::publish(&store_for_recorder, cap, daemon_forward);
            if let Some(published) = published {
                let _ = published.send(());
            }
        }
    });
    // Output always flows independently of stdin, context parsing and IPC.
    let native_zsh_frames = opts.command.is_none() && shell_name_is_zsh(&shell);
    let parser_token = frame_token.clone();
    let marker_for_reader = prompt_marker.clone();
    let pump = thread::spawn(move || {
        let mut pending: Vec<u8> = Vec::with_capacity(8192);
        let mut parser = native_zsh_frames
            .then(|| stream::StreamParser::with_output_limit(parser_token, output_limit));
        let mut stdout = std::io::stdout();
        let mut buf = [0u8; 4096];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let display = if let Some(parser) = &mut parser {
                let update = parser.feed(&buf[..n]);
                for cap in update.captured {
                    let (published, wait_for_publish) = if native_zsh_frames {
                        let (published, wait_for_publish) = mpsc::channel();
                        (Some(published), Some(wait_for_publish))
                    } else {
                        (None, None)
                    };
                    if capture_tx.send((cap, published)).is_ok() {
                        if let Some(wait_for_publish) = wait_for_publish {
                            // The command's earlier output chunks have already
                            // streamed. Hold this boundary read until its
                            // captured context reaches the shared store, so a
                            // user typing at the newly shown prompt sees it.
                            let _ = wait_for_publish.recv_timeout(publish_ack_timeout);
                        }
                    }
                }
                update.display
            } else {
                pending.extend_from_slice(&buf[..n]);
                if let Some(caps) = extract(&mut pending, &marker_for_reader) {
                    for cap in caps {
                        let _ = capture_tx.send((cap, None));
                    }
                }
                // An unfinished command or a long-running program must not
                // grow the legacy capture buffer indefinitely. Displayed
                // output is never cut.
                if pending.len() > MAX_CAPTURE_OUTPUT_BYTES {
                    pending.clear();
                }
                buf[..n].to_vec()
            };
            let _ = stdout.write_all(&display);
            let _ = stdout.flush();
        }
        if let Some(parser) = &mut parser {
            let update = parser.finish();
            let _ = stdout.write_all(&update.display);
            let _ = stdout.flush();
            for cap in update.captured {
                let _ = capture_tx.send((cap, None));
            }
        }
        reader_done.store(true, Ordering::Release);
    });

    let mut writer = writer;
    let mut input_open = true;
    let mut last_size = terminal_size();
    while !done.load(Ordering::Acquire) {
        if let Some(size) = terminal_size() {
            if last_size != Some(size) {
                let _ = pair.master.resize(size);
                last_size = Some(size);
            }
        }
        let mut poll = libc::pollfd {
            fd: if input_open { libc::STDIN_FILENO } else { -1 },
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 50) };
        if ready > 0 && poll.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut bytes = [0u8; 4096];
            let n =
                unsafe { libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len()) };
            if n > 0 {
                if writer.write_all(&bytes[..n as usize]).is_err() {
                    break;
                }
                let _ = writer.flush();
            } else {
                input_open = false;
                // Tell a child waiting on input about pipe EOF.
                let _ = writer.write_all(&[4]);
            }
        } else if ready < 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            let _ = child.kill();
            break;
        }
    }

    drop(writer);
    let _ = pump.join();
    let _ = recorder.join();
    let status = child
        .wait()
        .map_err(|e| crate::Error::Io(std::io::Error::other(e.to_string())))?;

    // A paused wrapper may hold an old in-memory snapshot; don't ping or save it.
    let may_persist = if opts.forward_to_daemon {
        match crate::config::Config::load() {
            Ok(config) => config.terminal_enabled(),
            Err(error) => {
                tracing::warn!(%error, "could not load configuration before saving captured context");
                false
            }
        }
    } else {
        true
    };
    if may_persist {
        // A live daemon owns shared persistence. Saving this shell's older
        // snapshot over it would erase commands captured by other sessions.
        let daemon_owns_history = opts.forward_to_daemon
            && crate::client::ask(
                crate::protocol::Request::Ping { id: 0 },
                Duration::from_millis(200),
                false,
            )
            .is_ok();
        if let Some(path) = crate::config::history_path()
            .ok()
            .filter(|_| !daemon_owns_history)
        {
            let snapshot = store.lock().clone();
            if let Err(e) = snapshot.save(&path) {
                tracing::warn!(error = %e, "could not save captured context");
            }
        }
    }
    tracing::info!(exit = ?status.exit_code(), "wrapped shell exited");
    Ok(())
}

fn shellutil_path() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// Recover complete commands from the PTY stream, draining `pending`.
///
/// Framing comes from the prompt marker the wrapper installs, which is the only
/// unambiguous boundary in an otherwise unframed terminal stream. Everything
/// between two markers is one command and its output.
fn extract(pending: &mut Vec<u8>, marker: &str) -> Option<Vec<Captured>> {
    let text = strip_ansi(&String::from_utf8_lossy(pending));
    let mut out = Vec::new();
    let mut rest = strip_leading(text.as_str(), marker);

    while let Some(end) = rest.find(marker) {
        let segment = &rest[..end];
        rest = &rest[end + marker.len()..];
        if let Some(cap) = parse_segment(segment, marker) {
            out.push(cap);
        }
        rest = strip_leading(rest, marker);
    }

    // Retain the unterminated tail; it may complete on a later chunk.
    let consumed_chars = text.len() - rest.len();
    let consumed_bytes = text
        .get(..consumed_chars)
        .map_or(pending.len(), |s| s.len());
    pending.drain(..consumed_bytes.min(pending.len()));
    (!out.is_empty()).then_some(out)
}

/// Parse one marker-delimited segment into a command and its output.
///
/// The first line is the echoed command; everything after it is its output.
fn parse_segment(segment: &str, marker: &str) -> Option<Captured> {
    // A PTY echoes CRLF, so a segment commonly begins with a bare "\r" and the
    // real command is on the following line. Normalise carriage returns before
    // splitting, otherwise the first line is empty and the command is lost.
    let normalized = normalize_lines(segment);
    let (command_line, body) = match normalized.find('\n') {
        Some(nl) => (&normalized[..nl], &normalized[nl + 1..]),
        None => (normalized.as_str(), ""),
    };
    let command = strip_prompt(command_line.trim(), marker);
    let (exit_code, body) = split_exit_status(body);
    let output = body
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if command.is_empty() && output.is_empty() {
        return None;
    }
    Some(Captured {
        command,
        cwd: None,
        exit_code,
        output,
    })
}

/// Normalize a PTY segment into clean `\n`-separated lines.
///
/// A pseudo-terminal turns every `\n` the shell writes into `\r\n`, and the
/// line discipline echoes a second `\r`, so lines arrive as `\r\r\n`. Collapsing
/// to single `\n` and dropping the blank lines that leaves would otherwise create
/// is what makes the first line of a segment actually be the command.
fn normalize_lines(segment: &str) -> String {
    // Each real newline arrives as CR CR LF (the line discipline echoes a second
    // CR). Replace that whole run with a single LF, then treat any stray CR as
    // its own line break too. Blanks produced by that are then collapsed.
    let mut collapsed = String::with_capacity(segment.len());
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            // Swallow the entire run of CRs, then consume a following LF.
            while i < bytes.len() && bytes[i] == b'\r' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'\n' {
                i += 1;
            }
            collapsed.push('\n');
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'\r' {
            i += 1;
        }
        collapsed.push_str(&segment[start..i]);
    }

    // Drop empty lines, keeping at most one blank between real lines.
    let mut out = String::with_capacity(collapsed.len());
    let mut pending_break = false;
    for line in collapsed.split('\n') {
        if line.trim().is_empty() {
            pending_break = !out.is_empty();
            continue;
        }
        if pending_break {
            out.push('\n');
            pending_break = false;
        } else if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// Detect and remove a trailing `[exit NNN]` marker on its own line.
fn split_exit_status(segment: &str) -> (Option<i32>, &str) {
    let trimmed = segment.trim_end();
    if !trimmed.ends_with(']') {
        return (None, segment);
    }
    let Some(open) = trimmed.rfind("[exit ") else {
        return (None, segment);
    };
    let Ok(code) = trimmed[open + 6..trimmed.len() - 1].trim().parse::<i32>() else {
        return (None, segment);
    };
    (Some(code), &segment[..open])
}

/// Remove the marker if `s` starts with it.
fn strip_leading<'a>(s: &'a str, marker: &str) -> &'a str {
    s.strip_prefix(marker).unwrap_or(s)
}

/// Remove a leading prompt. The marker is handled by the caller; this strips the
/// decorations a shell draws before the command, such as a fancy arrow or a
/// classic `%` prompt.
fn strip_prompt(line: &str, marker: &str) -> String {
    let mut s = line.strip_prefix(marker).unwrap_or(line).trim_start();
    for sym in ['❯', '➜', '»', '›'] {
        if let Some(rest) = s.strip_prefix(sym) {
            s = rest.trim_start();
            break;
        }
    }
    for sym in ['$', '%', '#', '>'] {
        if let Some(rest) = s.strip_prefix(sym) {
            s = rest.trim_start();
            break;
        }
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(stream: &str) -> Vec<Captured> {
        let mut pending = stream.as_bytes().to_vec();
        extract(&mut pending, PROMPT_MARKER).unwrap_or_default()
    }

    #[test]
    fn extract_recovers_command_and_output() {
        let caps = feed("\u{1}tp\u{1}echo hi\nhi\n\u{1}tp\u{1}");
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].command, "echo hi");
        assert!(caps[0].output.contains("hi"));
    }

    #[test]
    fn extract_consumes_parsed_prefix() {
        let mut pending = "\u{1}tp\u{1}ls\nfile\n\u{1}tp\u{1}".as_bytes().to_vec();
        extract(&mut pending, PROMPT_MARKER).unwrap();
        assert!(
            pending.is_empty(),
            "leftover: {:?}",
            String::from_utf8_lossy(&pending)
        );
    }

    #[test]
    fn extract_waits_for_the_next_marker() {
        let caps = feed("\u{1}tp\u{1}echo partial");
        assert!(
            caps.is_empty(),
            "an unterminated segment must not be published"
        );
    }

    #[test]
    fn extract_keeps_unterminated_tail_for_next_chunk() {
        let mut pending = "\u{1}tp\u{1}echo hi\nhi\n\u{1}tp\u{1}ls -l"
            .as_bytes()
            .to_vec();
        let caps = extract(&mut pending, PROMPT_MARKER).unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].command, "echo hi");
        let tail = String::from_utf8_lossy(&pending).to_string();
        assert!(tail.contains("ls -l"), "tail must survive: {tail:?}");
    }

    #[test]
    fn extract_ignores_a_lone_leading_marker() {
        assert!(feed("\u{1}tp\u{1}").is_empty());
    }

    #[test]
    fn extract_handles_multiline_output() {
        let caps = feed("\u{1}tp\u{1}wc -l f\n1\n2\n3\n\u{1}tp\u{1}");
        assert_eq!(caps[0].command, "wc -l f");
        assert!(caps[0].output.contains('3'));
    }

    #[test]
    fn extract_parses_exit_status() {
        let caps = feed("\u{1}tp\u{1}false\n[exit 1]\n\u{1}tp\u{1}");
        assert_eq!(caps[0].command, "false");
        assert_eq!(caps[0].exit_code, Some(1));
        assert!(
            !caps[0].output.contains("exit 1"),
            "marker must not count as output"
        );
    }

    #[test]
    fn exit_status_marker_is_absent_on_success() {
        let caps = feed("\u{1}tp\u{1}true\nok\n\u{1}tp\u{1}");
        assert_eq!(caps[0].exit_code, None);
    }

    #[test]
    fn multiple_commands_in_one_chunk() {
        let caps = feed("\u{1}tp\u{1}one\n1\n\u{1}tp\u{1}two\n2\n\u{1}tp\u{1}");
        let cmds: Vec<_> = caps.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(cmds, vec!["one", "two"]);
    }

    #[test]
    fn normalizes_pty_doubled_carriage_returns() {
        // A PTY emits CR CR LF for every newline the shell writes.
        let got = normalize_lines("\r\r\ncmd\r\r\nout\r\r\n");
        assert_eq!(got, "cmd\nout");
    }

    #[test]
    fn normalizes_lone_carriage_returns() {
        assert_eq!(normalize_lines("a\rb"), "a\nb");
    }

    #[test]
    fn normalizes_plain_newlines_unchanged() {
        assert_eq!(normalize_lines("a\nb\n"), "a\nb");
    }

    #[test]
    fn extract_handles_real_pty_framing() {
        // Exactly what the PTY produced for `tp shell -c 'echo CAPTURED_OK'`.
        let mut pending =
            "\u{1}tp\u{1}\r\r\necho CAPTURED_OK\r\r\nCAPTURED_OK\r\r\n[exit 0]\r\r\n\u{1}tp\u{1}\r\r\n"
                .as_bytes()
                .to_vec();
        let caps = extract(&mut pending, PROMPT_MARKER).expect("a capture");
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].command, "echo CAPTURED_OK");
        assert_eq!(caps[0].exit_code, Some(0));
        assert_eq!(caps[0].output, "CAPTURED_OK");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn strip_prompt_removes_shell_markers() {
        assert_eq!(strip_prompt("% git status", ""), "git status");
        assert_eq!(strip_prompt("$ ls -la", ""), "ls -la");
        assert_eq!(strip_prompt("❯ cargo build", ""), "cargo build");
        assert_eq!(strip_prompt("git status", ""), "git status");
    }

    #[test]
    fn split_exit_status_ignores_malformed_marker() {
        let (code, _) = split_exit_status("ls\n[exit abc]");
        assert_eq!(code, None);
    }

    #[test]
    fn split_exit_status_ignores_unclosed_marker() {
        let (code, _) = split_exit_status("ls\n[exit 1");
        assert_eq!(code, None);
    }

    #[test]
    fn instrumentation_uses_precmd_for_zsh() {
        let script = instrumentation("/bin/zsh", "deadbeef");
        assert!(script.contains("precmd_functions"), "{script}");
        assert!(script.contains("__tp_frame"));
        assert!(script.contains("[exit %d]"), "{script}");
    }

    #[test]
    fn instrumentation_uses_prompt_command_for_bash() {
        let script = instrumentation("/bin/bash", "deadbeef");
        assert!(script.contains("PROMPT_COMMAND"), "{script}");
        assert!(!script.contains("precmd_functions"), "{script}");
    }

    #[test]
    fn instrumentation_generates_a_session_specific_marker() {
        let script = instrumentation("/bin/bash", "deadbeef");
        assert!(script.contains("\\001tp-deadbeef\\001"), "{script}");
        assert!(
            !script.contains('\u{1}'),
            "marker is generated by the shell"
        );
    }

    #[test]
    fn instrumentation_preserves_existing_prompt_command() {
        let script = instrumentation("/bin/bash", "deadbeef");
        assert!(script.contains("$PROMPT_COMMAND"), "{script}");
    }

    #[test]
    fn detects_zsh_by_name() {
        assert!(shell_name_is_zsh("/bin/zsh"));
        assert!(shell_name_is_zsh("/usr/local/bin/zsh"));
        assert!(shell_name_is_zsh("/bin/zsh-5.9"));
        assert!(!shell_name_is_zsh("/bin/bash"));
        // A different program that merely starts with the same letters.
        assert!(!shell_name_is_zsh("/usr/bin/zsh-not-really"));
        assert!(!shell_name_is_zsh("/bin/sh"));
    }
}
