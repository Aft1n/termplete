use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use clap::Subcommand;
use termplete::config::Config;
use termplete::protocol::Request;
use termplete::{client, daemon, Error};

const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Subcommand, Debug)]
pub enum ServiceCmd {
    /// Start the daemon in the background.
    Start,
    /// Ask a running daemon to exit.
    Stop,
    /// Report whether a daemon is listening.
    Status,
}

/// Persist the integration toggle without touching the daemon.
fn set_enabled(enabled: bool) -> termplete::Result<()> {
    let mut cfg = Config::load()?;
    cfg.enabled = Some(enabled);
    Ok(cfg.save()?)
}

/// Ask a running daemon to exit; fire-and-forget, the caller waits as needed.
pub fn shutdown() {
    let _ = client::ask(
        Request::Shutdown { id: 1 },
        Duration::from_millis(500),
        false,
    );
}

/// Pause integration requests, persistently, and stop the daemon if it is live.
pub fn stop() -> termplete::Result<()> {
    set_enabled(false)?;

    report("TermPlete paused for new and refreshed shell integrations.");
    report("Wrappers stay until their terminal tabs close.");
    report("Older wrappers may still record or autostart; close those tabs to stop them.");

    if !is_live_daemon() {
        report("daemon   was not running");
        return Ok(());
    }

    shutdown();
    if wait_for_ready(false, STOP_TIMEOUT) {
        report("daemon   stopped");
        Ok(())
    } else {
        Err(Error::Daemon(
            "the pause was saved, but the daemon did not exit within 2 seconds".into(),
        ))
    }
}

/// Re-enable shell integration and ensure the background daemon is available.
pub fn start() -> termplete::Result<()> {
    set_enabled(true)?;

    report("TermPlete resumed; existing integrations can use it now.");
    report("New shells use the saved setting.");
    if is_live_daemon() {
        report("daemon   already running");
        return Ok(());
    }

    daemon::spawn_background()?;
    if wait_for_ready(true, START_TIMEOUT) {
        report("daemon   started");
        Ok(())
    } else {
        Err(Error::Daemon(
            "TermPlete is enabled, but the daemon did not become ready; check `tp status` and its daemon log".into(),
        ))
    }
}

/// Run the existing low-level daemon controls without changing configuration.
pub fn service(cmd: ServiceCmd) -> termplete::Result<()> {
    match cmd {
        ServiceCmd::Start => {
            if Config::load()?.enabled == Some(false) {
                return Err(Error::Daemon("TermPlete is paused; run `tp start`".into()));
            }
            if is_live_daemon() {
                report("daemon   already running");
            } else {
                daemon::spawn_background()?;
                if wait_for_ready(true, START_TIMEOUT) {
                    report("daemon   started");
                } else {
                    report("daemon   not ready yet — check `tp status` and its daemon log");
                }
            }
        }
        ServiceCmd::Stop => {
            if is_live_daemon() {
                shutdown();
                report("daemon   stopping");
            } else {
                report("daemon   was not running");
            }
        }
        ServiceCmd::Status => report(&format!(
            "daemon   {}",
            if is_live_daemon() {
                "running"
            } else {
                "not running"
            }
        )),
    }
    Ok(())
}

/// Whether a daemon is currently accepting connections.
pub fn is_live_daemon() -> bool {
    UnixStream::connect(daemon::socket_display()).is_ok()
}

/// Poll until liveness matches `live` or `timeout` elapses.
pub fn wait_for_ready(live: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_live_daemon() == live {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    is_live_daemon() == live
}

/// Print and flush a status line so a shell repaint cannot swallow it.
fn report(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
