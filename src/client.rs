//! Client side of the daemon socket: connect, optionally autostart, send a
//! request, read the matching reply.
//!
//! Shells call this on every keystroke, so the path is kept allocation-light and
//! fully time-bounded. A daemon that is missing, slow, or wedged degrades to
//! "no suggestion" rather than freezing the prompt.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config;
use crate::protocol::{Request, Response};
use crate::Result;

/// A connected client with a buffered reader for line-delimited replies.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    /// Connect to an already-running daemon.
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self> {
        let stream = UnixStream::connect(path).map_err(|e| {
            crate::Error::Daemon(format!("could not connect to {}: {e}", path.display()))
        })?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
        })
    }

    /// Connect, spawning a daemon first if `autostart` is set.
    ///
    /// Retries briefly: `spawn` returns as soon as the child exists, so the
    /// socket may not be listening yet.
    pub fn connect_or_start(timeout: Duration, autostart: bool) -> Result<Self> {
        let path = config::socket_path();
        match Self::connect(&path, timeout) {
            Ok(c) => return Ok(c),
            Err(e) if !autostart => return Err(e),
            Err(_) => {}
        }
        let cfg = config::Config::load()?;
        ensure_autostart_enabled(&cfg)?;
        crate::daemon::spawn_background()?;
        let deadline = Instant::now() + timeout;
        let mut last = None;
        while Instant::now() < deadline {
            match Self::connect(&path, timeout) {
                Ok(c) => return Ok(c),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(last
            .unwrap_or_else(|| crate::Error::Daemon("daemon did not become ready in time".into())))
    }

    /// Send `req` and return the reply with the matching id.
    ///
    /// Out-of-band replies are skipped rather than misdelivered, so a stale reply
    /// from an abandoned request cannot corrupt a newer one.
    pub fn request(&mut self, req: &Request, id: u64) -> Result<Response> {
        let mut line = serde_json::to_string(req)
            .map_err(|e| crate::Error::Daemon(format!("could not encode request: {e}")))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;

        loop {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf)?;
            if n == 0 {
                return Err(crate::Error::Daemon("daemon closed the connection".into()));
            }
            let res: Response = serde_json::from_str(&buf)
                .map_err(|e| crate::Error::Daemon(format!("bad reply from daemon: {e}")))?;
            if res.id() == id {
                return Ok(res);
            }
        }
    }
}

fn ensure_autostart_enabled(cfg: &config::Config) -> Result<()> {
    if cfg.enabled.unwrap_or(true) {
        Ok(())
    } else {
        Err(crate::Error::Daemon(
            "TermPlete is paused; run `tp start` to resume it".into(),
        ))
    }
}

/// Convenience wrapper: one request per connection.
///
/// Shells only ever need a single round-trip per keystroke, so this avoids
/// keeping state around between calls.
pub fn ask(req: Request, timeout: Duration, autostart: bool) -> Result<Response> {
    let id = request_id(&req);
    let mut client = Client::connect_or_start(timeout, autostart)?;
    client.request(&req, id)
}

/// Extract the id from a request so the caller need not track it separately.
pub fn request_id(req: &Request) -> u64 {
    match req {
        Request::Suggest { id, .. }
        | Request::Generate { id, .. }
        | Request::ObserveInput { id, .. }
        | Request::Record { id, .. }
        | Request::Feedback { id, .. }
        | Request::Ping { id }
        | Request::Shutdown { id } => *id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_is_read_from_the_request() {
        assert_eq!(request_id(&Request::Ping { id: 42 }), 42);
        assert_eq!(
            request_id(&Request::Feedback {
                cwd: None,
                id: 7,
                prefix: "git l".into(),
                command: "git log --oneline".into(),
            }),
            7
        );
        assert_eq!(
            request_id(&Request::Suggest {
                id: 9,
                line: "x".into(),
                cursor: None,
                cwd: None,
                mode: None,
                local_only: true,
                frame: None
            }),
            9
        );
    }

    #[test]
    fn connecting_to_absent_socket_fails_fast() {
        for path in ["/nonexistent/termplete.sock", "/nonexistent/tp.sock"] {
            let err = Client::connect(Path::new(path), Duration::from_millis(50));
            assert!(matches!(err, Err(crate::Error::Daemon(_))), "{path}");
        }
    }

    #[test]
    fn stale_autostart_is_rejected_when_the_saved_global_pause_is_set() {
        let cfg = config::Config {
            enabled: Some(false),
            ..config::Config::default()
        };

        let err = ensure_autostart_enabled(&cfg).unwrap_err();
        assert!(matches!(err, crate::Error::Daemon(message) if message.contains("paused")));
    }
}
