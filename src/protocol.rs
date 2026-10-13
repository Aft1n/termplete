//! The newline-delimited JSON protocol spoken between shell plugins and the
//! daemon over a per-user unix socket.
//!
//! One JSON object per line in each direction. Requests carry an `id` that the
//! daemon echoes back, so a plugin with several requests in flight can match
//! replies. The design is deliberately tiny: the plugin needs a suggestion, it
//! asks for one, and it renders whatever ghost text comes back.

use serde::{Deserialize, Serialize};

/// A request from a shell plugin (or from `tp suggest`) to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Ask for completions of the current input line.
    Suggest {
        id: u64,
        line: String,
        /// Cursor byte offset into `line`. Absent means end of buffer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<usize>,
        /// The requesting shell's working directory. It is supplied per request
        /// because one daemon may serve shells in different directories.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Overrides the daemon's configured mode for this request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        /// Skip the model entirely; history only.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        local_only: bool,
        /// Animation frame index the shell is currently on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<usize>,
    },
    /// Turn a natural-language shell task into command suggestions.
    Generate {
        id: u64,
        instruction: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Preserve unexecuted pasted input as context, without learning usage.
    ObserveInput {
        id: u64,
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Report a command the user just ran, with its output and exit code.
    Record {
        id: u64,
        command: String,
        /// Directory in which execution started, needed for relative paths.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// Report a suggestion the user accepted while typing `prefix`.
    Feedback {
        id: u64,
        prefix: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Liveness and a little diagnostics for `tp status`.
    Ping { id: u64 },
    /// Ask the daemon to exit.
    Shutdown { id: u64 },
}

/// A reply from the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    /// Suggestions for a [`Request::Suggest`]. `ghost` is pre-rendered text,
    /// including any color escapes, so the shell does no formatting work.
    Suggest {
        id: u64,
        /// Text that extends the current buffer; already styled.
        ghost: String,
        /// Full candidate commands, most confident first.
        candidates: Vec<String>,
        /// Which mode produced this reply.
        mode: String,
        /// How the suggestion was produced.
        source: String,
        /// The ghost text with no styling applied.
        ghost_plain: String,
        /// Why a provider was unavailable or produced no usable continuation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
    },
    /// Acknowledgement for `Record`, `Ping` and `Shutdown`.
    Ok {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Acknowledgement for `Request::Feedback`.
    Ack { id: u64 },
    /// The request could not be served. Never fatal — the shell just shows
    /// nothing.
    Error { id: u64, message: String },
}

impl Response {
    pub fn id(&self) -> u64 {
        match self {
            Self::Suggest { id, .. }
            | Self::Ok { id, .. }
            | Self::Ack { id }
            | Self::Error { id, .. } => *id,
        }
    }

    pub fn ok(id: u64) -> Self {
        Self::Ok { id, note: None }
    }

    pub fn err(id: u64, message: impl Into<String>) -> Self {
        Self::Error {
            id,
            message: message.into(),
        }
    }
}

/// Clamp a client-supplied cursor offset into `line`.
///
/// A plugin bug must not be able to panic the daemon, so every byte offset is
/// validated and snapped to a char boundary.
pub fn clamp_cursor(line: &str, cursor: Option<usize>) -> usize {
    match cursor {
        None => line.len(),
        Some(c) => {
            let mut c = c.min(line.len());
            while c > 0 && !line.is_char_boundary(c) {
                c -= 1;
            }
            c
        }
    }
}

/// The text up to the cursor, which is what a completion should extend.
pub fn line_prefix(line: &str, cursor: Option<usize>) -> &str {
    &line[..clamp_cursor(line, cursor)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_roundtrip() {
        let requests = [
            Request::Suggest {
                id: 7,
                line: "git st".into(),
                cursor: Some(6),
                cwd: Some("/tmp/project".into()),
                mode: Some("eager".into()),
                local_only: false,
                frame: Some(3),
            },
            Request::Generate {
                cwd: None,
                id: 12,
                instruction: "find files larger than 100 MB".into(),
            },
            Request::Record {
                cwd: Some("/tmp/project".into()),
                id: 2,
                command: "cargo test".into(),
                exit_code: Some(101),
                output: Some("FAILED".into()),
            },
            Request::Feedback {
                cwd: Some("/tmp/project".into()),
                id: 4,
                prefix: "git ch".into(),
                command: "git checkout main".into(),
            },
            Request::Ping { id: 1 },
            Request::Shutdown { id: 2 },
        ];
        for req in requests {
            let json = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        }
    }

    #[test]
    fn legacy_records_and_feedback_default_to_no_directory() {
        for json in [
            r#"{"op":"record","id":1,"command":"cd project"}"#,
            r#"{"op":"feedback","id":2,"prefix":"cd p","command":"cd project"}"#,
        ] {
            match serde_json::from_str::<Request>(json).unwrap() {
                Request::Record { cwd, .. } | Request::Feedback { cwd, .. } => {
                    assert!(cwd.is_none())
                }
                other => panic!("wrong request: {other:?}"),
            }
        }
    }

    #[test]
    fn responses_roundtrip() {
        let ack = Response::Ack { id: 4 };
        assert_eq!(ack.id(), 4);
        let res = Response::Suggest {
            id: 5,
            ghost: "\x1b[38;2;107;114;128matus\x1b[39m".into(),
            candidates: vec!["git status".into()],
            mode: "subtle".into(),
            source: "history".into(),
            ghost_plain: "atus".into(),
            diagnostic: Some("Provider unavailable".into()),
        };
        for r in [ack, res] {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), r);
        }
    }

    #[test]
    fn error_response_roundtrips() {
        let res = Response::err(9, "boom");
        let json = serde_json::to_string(&res).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), res);
        assert_eq!(res.id(), 9);
    }

    #[test]
    fn ok_response_omits_empty_note() {
        let json = serde_json::to_string(&Response::ok(1)).unwrap();
        assert!(!json.contains("note"), "{json}");
    }

    #[test]
    fn cursor_offsets_clamp_and_prefix() {
        assert_eq!(clamp_cursor("hello", None), 5);
        assert_eq!(clamp_cursor("hello", Some(999)), 5);
        // "é" is two bytes; an offset of 1 is mid-character.
        assert_eq!(clamp_cursor("éx", Some(1)), 0);
        assert_eq!(clamp_cursor("éx", Some(2)), 2);
        assert_eq!(line_prefix("git status", Some(3)), "git");
        assert_eq!(line_prefix("git status", None), "git status");
    }
}
