//! Rolling shell context: what the user actually ran, and what it printed.
//!
//! This is the memory behind the suggestions. The store holds a bounded deque of
//! [`Entry`] records (command text, exit status, captured output), persists to
//! JSON. A separate indexed command memory keeps frequently used commands after
//! their captured output has aged out of the recent context.
//!
//! Privacy posture: every command and every captured output blob is passed
//! through [`crate::redact::scrub`] on the way in, and output blobs are truncated
//! to the configured cap, so secrets do not land in the history file or in a
//! prompt.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, ContextConfig};
use crate::redact;
use crate::shellutil;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse history {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not serialize history: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// One recorded command and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The command line, redacted.
    pub command: String,
    /// Process exit status when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Captured output, redacted and truncated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
    /// Monotonic sequence number, used to drop stale duplicate records.
    #[serde(default)]
    pub seq: u64,
    /// Execution directory, when supplied by the shell observer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Wall-clock capture time in milliseconds; zero for older untimed entries.
    #[serde(default)]
    pub recorded_at: u64,
    /// Observed paste that has not executed; never counts as command usage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

impl Entry {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: redact::scrub(&command.into()),
            exit_code: None,
            output: Vec::new(),
            seq: 0,
            cwd: None,
            recorded_at: 0,
            pending: false,
        }
    }

    /// Total bytes of captured output.
    pub fn output_bytes(&self) -> usize {
        self.output.iter().map(String::len).sum()
    }

    /// Output joined with newlines, truncated to `max_bytes` from the tail so the
    /// most relevant part (errors usually appear last) survives.
    pub fn output_tail(&self, max_bytes: usize) -> String {
        let mut tail = String::new();
        for line in self.output.iter().rev() {
            if tail.len() + line.len() + 1 > max_bytes && !tail.is_empty() {
                break;
            }
            if !tail.is_empty() {
                tail.insert(0, '\n');
            }
            tail.insert_str(0, line);
            if tail.len() >= max_bytes {
                break;
            }
        }
        tail
    }

    /// One-line, display-safe summary used in `tp context`.
    pub fn summary(&self) -> String {
        let status = if self.pending {
            "pending".to_owned()
        } else {
            match self.exit_code {
                Some(0) => "ok".to_string(),
                Some(c) => format!("exit {c}"),
                None => "—".to_string(),
            }
        };
        let out = if self.output.is_empty() {
            String::new()
        } else {
            format!(" · {}b", self.output_bytes())
        };
        format!("[{status}] {}{out}", self.command)
    }
}

/// Bounded, persisted command history with captured output.
#[derive(Debug, Clone)]
pub struct Store {
    entries: VecDeque<Entry>,
    next_seq: u64,
    cfg: ContextConfig,
    memory: crate::memory::CommandMemory,
}

impl Store {
    pub fn new(cfg: ContextConfig) -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 1,
            cfg,
            memory: crate::memory::CommandMemory::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Most recent entries first.
    pub fn recent(&self, n: usize) -> Vec<&Entry> {
        self.entries.iter().rev().take(n).collect()
    }

    pub fn config(&self) -> &ContextConfig {
        &self.cfg
    }

    pub fn memory(&self) -> &crate::memory::CommandMemory {
        &self.memory
    }

    /// Record a completion the user explicitly accepted for this input prefix.
    pub fn accept_suggestion(&mut self, prefix: &str, command: &str) -> bool {
        self.accept_suggestion_with_cwd(prefix, command, None)
    }

    pub fn accept_suggestion_with_cwd(
        &mut self,
        prefix: &str,
        command: &str,
        cwd: Option<&Path>,
    ) -> bool {
        self.memory.accept_with_cwd(prefix, command, &self.cfg, cwd)
    }

    pub fn import_history(&mut self, path: &Path) -> Result<usize, ContextError> {
        self.memory
            .import_file(path, &self.cfg)
            .map_err(|source| ContextError::Io {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Record a command, redacting secrets and dropping ignored commands.
    ///
    /// Returns the stored entry, or `None` when the command was skipped.
    pub fn record(&mut self, command: &str, exit_code: Option<i32>) -> Option<Entry> {
        self.record_with_cwd(command, exit_code, None)
    }

    pub fn record_with_cwd(
        &mut self,
        command: &str,
        exit_code: Option<i32>,
        cwd: Option<&Path>,
    ) -> Option<Entry> {
        let command = command.trim();
        let memory_command = single_input_command(command).unwrap_or(command);
        if command.is_empty() || self.is_ignored(memory_command) {
            return None;
        }
        let mut entry = Entry::new(command);
        entry.exit_code = exit_code;
        entry.recorded_at = crate::memory::now();
        entry.cwd = cwd
            .filter(|path| path.is_absolute())
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
        entry.seq = self.next_seq;
        self.next_seq += 1;
        self.memory
            .record_with_cwd(memory_command, exit_code, &self.cfg, entry.cwd.as_deref());
        self.entries.retain(|previous| {
            !(previous.pending && previous.cwd == entry.cwd && previous.command == memory_command)
        });
        // Keep the output context compact while still counting repeated executions.
        if self
            .entries
            .back()
            .is_some_and(|previous| previous.command == entry.command && previous.cwd == entry.cwd)
        {
            if let Some(previous) = self.entries.back_mut() {
                previous.seq = entry.seq;
                previous.exit_code = entry.exit_code;
                previous.recorded_at = entry.recorded_at;
                previous.pending = false;
                previous.output.clear();
            }
            return Some(entry);
        }
        self.push(entry.clone());
        Some(entry)
    }

    /// Keep one unambiguous pasted command as context without recording a use.
    pub fn observe_input(&mut self, input: &str, cwd: Option<&Path>) -> bool {
        let Some(command) = single_input_command(input) else {
            return false;
        };
        if self.is_ignored(command) || crate::privacy::sensitive_with_paths(command, &[]) {
            return false;
        }
        let mut entry = Entry::new(command);
        entry.pending = true;
        entry.recorded_at = crate::memory::now();
        entry.cwd = cwd
            .filter(|path| path.is_absolute())
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
        entry.seq = self.next_seq;
        self.next_seq += 1;
        self.push(entry);
        true
    }

    /// Attach captured output to the most recent entry whose command matches
    /// `command` and which has no output yet.
    pub fn attach_output(&mut self, command: &str, output: &str) {
        let command = command.trim();
        let output = redact::scrub(output);
        if output.trim().is_empty() {
            return;
        }
        if output.len() > self.cfg.ignore_output_over_bytes {
            return;
        }
        let target = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| !e.pending && e.command == command && e.output.is_empty());
        let Some(entry) = target else { return };
        entry.output = split_lines(&output);
        truncate_output(entry, self.cfg.max_output_bytes);
    }

    /// Convenience: record a command and its output together.
    pub fn record_with_output(
        &mut self,
        command: &str,
        exit_code: Option<i32>,
        output: &str,
    ) -> Option<Entry> {
        let entry = self.record(command, exit_code)?;
        self.attach_output(command, output);
        Some(entry)
    }

    pub fn record_with_output_and_cwd(
        &mut self,
        command: &str,
        exit_code: Option<i32>,
        output: &str,
        cwd: Option<&Path>,
    ) -> Option<Entry> {
        let entry = self.record_with_cwd(command, exit_code, cwd)?;
        self.attach_output(command, output);
        Some(entry)
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push_back(entry);
        while self.entries.len() > self.cfg.history_entries {
            self.entries.pop_front();
        }
    }

    fn is_ignored(&self, command: &str) -> bool {
        self.cfg
            .ignore_prefixes
            .iter()
            .any(|p| !p.is_empty() && command.starts_with(p.as_str()))
    }

    // --- persistence ---------------------------------------------------------

    pub fn load(path: &Path) -> Result<Self, ContextError> {
        let cfg = ContextConfig::default();
        let store = match std::fs::read_to_string(path) {
            Ok(text) => {
                let entries: Vec<Entry> =
                    serde_json::from_str(&text).map_err(|source| ContextError::Parse {
                        path: path.to_path_buf(),
                        source,
                    })?;
                let next_seq = entries.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
                Self {
                    entries: entries.into(),
                    next_seq,
                    cfg,
                    memory: crate::memory::CommandMemory::default(),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::new(cfg),
            Err(source) => {
                return Err(ContextError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Ok(store)
    }

    pub fn load_with(path: &Path, cfg: &Config) -> Result<Self, ContextError> {
        let mut store = Self::load(path)?;
        store.cfg = cfg.context.clone();
        store.enforce_bounds();
        let memory_path = path.with_file_name("commands.json");
        store.memory = match crate::memory::CommandMemory::load(&memory_path, &cfg.context) {
            Ok(memory) => memory,
            Err(error) => {
                tracing::warn!(error = %error, "could not load command memory; rebuilding from recent context");
                if memory_path.exists() {
                    let backup = memory_path
                        .with_extension(format!("corrupt-{}.json", crate::memory::now()));
                    std::fs::copy(&memory_path, backup).map_err(|source| ContextError::Io {
                        path: memory_path.clone(),
                        source,
                    })?;
                }
                crate::memory::CommandMemory::default()
            }
        };
        // Seed old installations once, without counting the same recent entries
        // again every time a client reads context.
        let missing: std::collections::HashSet<_> = store
            .entries
            .iter()
            .filter(|entry| !entry.pending)
            .map(|entry| single_input_command(&entry.command).unwrap_or(&entry.command))
            .filter(|command| !store.memory.contains(command))
            .map(str::to_owned)
            .collect();
        for entry in &store.entries {
            let command = single_input_command(&entry.command).unwrap_or(&entry.command);
            if !entry.pending && missing.contains(command) {
                store.memory.record_with_cwd(
                    command,
                    entry.exit_code,
                    &cfg.context,
                    entry.cwd.as_deref(),
                );
            }
        }
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<(), ContextError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        {
            std::fs::create_dir_all(parent).map_err(|source| ContextError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut file =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| ContextError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        serde_json::to_writer(&mut file, &self.entries)?;
        file.flush().map_err(|source| ContextError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        file.persist(path).map_err(|error| ContextError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
        let memory_path = path.with_file_name("commands.json");
        self.memory
            .save(&memory_path)
            .map_err(|source| ContextError::Io {
                path: memory_path,
                source,
            })
    }

    fn enforce_bounds(&mut self) {
        while self.entries.len() > self.cfg.history_entries {
            self.entries.pop_front();
        }
        for e in &mut self.entries {
            truncate_output(e, self.cfg.max_output_bytes);
        }
    }
}

fn split_lines(text: &str) -> Vec<String> {
    shellutil::strip_ansi(text)
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Keep only the last lines that fit in `max_bytes`.
fn truncate_output(entry: &mut Entry, max_bytes: usize) {
    while entry.output_bytes() > max_bytes && entry.output.len() > 1 {
        entry.output.remove(0);
    }
}

// A comment-prefixed installation snippet still has one exact actionable command.
fn single_input_command(input: &str) -> Option<&str> {
    if input.len() > 4096 {
        return None;
    }
    let mut commands = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let command = commands.next()?;
    (commands.next().is_none() && !command.chars().any(char::is_control)).then_some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_memory_outlives_output_context_and_reload_does_not_inflate_counts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        let cfg = Config {
            context: ContextConfig {
                history_entries: 2,
                ..ContextConfig::default()
            },
            ..Config::default()
        };
        let mut store = Store::new(cfg.context.clone());
        for _ in 0..5 {
            store.record("ssh root@203.0.113.7", Some(0));
        }
        for index in 0..10 {
            store.record(&format!("echo {index}"), Some(0));
        }
        assert_eq!(store.len(), 2);
        store.save(&path).unwrap();
        for _ in 0..3 {
            store = Store::load_with(&path, &cfg).unwrap();
            assert_eq!(store.memory().related("ssh", 3)[0].1.uses, 5);
            store.save(&path).unwrap();
        }
    }

    #[test]
    fn pending_paste_survives_reload_without_learning_execution() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        let cfg = Config::default();
        let mut store = Store::new(cfg.context.clone());
        let input = "# Installs the latest release\nbrew install jj";
        assert!(store.observe_input(input, Some(directory.path())));
        assert_eq!(store.memory().len(), 0);
        assert!(store.entries().next().unwrap().pending);
        assert!(store
            .entries()
            .next()
            .unwrap()
            .summary()
            .contains("[pending]"));
        store.save(&path).unwrap();
        let mut restored = Store::load_with(&path, &cfg).unwrap();
        assert_eq!(restored.memory().len(), 0);
        assert!(restored.entries().next().unwrap().pending);
        restored.record_with_cwd(input, Some(0), Some(directory.path()));
        assert!(restored.entries().all(|entry| !entry.pending));
        assert_eq!(restored.memory().related("brew", 1)[0].1.uses, 1);
        restored.save(&path).unwrap();
        std::fs::remove_file(path.with_file_name("commands.json")).unwrap();
        assert_eq!(
            Store::load_with(&path, &cfg)
                .unwrap()
                .memory()
                .related("brew", 1)[0]
                .1
                .uses,
            1
        );
        for rejected in [
            "# only a comment",
            "brew install jj\nbrew install tmux",
            "sudo install jj",
            "echo API_KEY=secret",
        ] {
            assert!(
                !restored.observe_input(rejected, Some(directory.path())),
                "{rejected}"
            );
        }
    }

    #[test]
    fn corrupt_memory_is_backed_up_and_rebuilt_without_breaking_context() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        let cfg = Config::default();
        let mut store = Store::new(cfg.context.clone());
        store.record("ssh root@203.0.113.7", Some(0));
        store.save(&path).unwrap();
        std::fs::write(directory.path().join("commands.json"), "{broken").unwrap();
        let restored = Store::load_with(&path, &cfg).unwrap();
        assert_eq!(
            restored.memory().prefix("ssh ", 3)[0].0,
            "ssh root@203.0.113.7"
        );
        assert!(std::fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("corrupt-")
        }));
    }

    fn cfg() -> ContextConfig {
        ContextConfig {
            history_entries: 5,
            ..ContextConfig::default()
        }
    }

    #[test]
    fn records_and_orders_entries() {
        let mut s = Store::new(cfg());
        s.record("ls", Some(0));
        s.record("cat x", Some(1));
        let recent: Vec<_> = s
            .recent(2)
            .into_iter()
            .map(|e| e.command.as_str())
            .collect();
        assert_eq!(recent, vec!["cat x", "ls"]);
    }

    #[test]
    fn enforces_history_cap() {
        let mut s = Store::new(cfg());
        for i in 0..10 {
            s.record(&format!("cmd{i}"), Some(0));
        }
        assert_eq!(s.len(), 5);
        assert_eq!(s.entries().next().unwrap().command, "cmd5");
    }

    #[test]
    fn ignores_configured_prefixes() {
        let mut s = Store::new(cfg());
        assert!(s.record("sudo rm -rf /", None).is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn counts_repeated_executions_without_duplicating_output_context() {
        let mut s = Store::new(cfg());
        assert!(s.record("   ", None).is_none());
        s.record("ls", Some(0));
        assert!(s.record("ls", Some(0)).is_some());
        assert_eq!(s.len(), 1);
        assert_eq!(s.memory().related("ls", 1)[0].1.uses, 2);
    }

    #[test]
    fn accepted_suggestions_are_prefix_scoped_ranked_and_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        let cfg = Config::default();
        let mut store = Store::new(cfg.context.clone());
        assert!(store.accept_suggestion("git ch", "git checkout main"));
        assert!(store.accept_suggestion("git ch", "git checkout main"));
        assert!(store.accept_suggestion("git ch", "git cherry-pick abc"));
        assert!(store.accept_suggestion("git st", "git status --short"));

        assert_eq!(store.memory().prefix("git ch", 3)[0].0, "git checkout main");
        assert_eq!(
            store.memory().prefix("git st", 3)[0].0,
            "git status --short"
        );
        // Two accepts versus one competing fresh accept is still ambiguous.
        assert!(!store.memory().has_trusted_choice("git ch"));
        assert!(!store.memory().has_trusted_choice("git st"));
        assert_eq!(
            store.memory().accepted_choices("git ch", 3),
            vec![
                ("git checkout main".to_string(), 2),
                ("git cherry-pick abc".to_string(), 1)
            ]
        );

        store.save(&path).unwrap();
        let restored = Store::load_with(&path, &cfg).unwrap();
        assert_eq!(
            restored.memory().accepted_choices("git ch", 3),
            store.memory().accepted_choices("git ch", 3)
        );
        // Scores decay with elapsed time, while the ranking order should persist.
        let ranked_commands = |store: &Store| {
            store
                .memory()
                .prefix("git ch", 3)
                .into_iter()
                .map(|(command, _)| command)
                .collect::<Vec<_>>()
        };
        assert_eq!(ranked_commands(&restored), ranked_commands(&store));
        assert!(!restored.memory().has_trusted_choice("git ch"));
    }

    #[test]
    fn failed_execution_suppresses_an_accepted_choice_until_success() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.json");
        let cfg = Config::default();
        let mut store = Store::new(ContextConfig::default());
        let command = "docker ps --all";
        store.accept_suggestion("docker p", command);
        store.accept_suggestion("docker p", command);
        assert!(store.memory().has_trusted_choice("docker p"));

        store.record(command, Some(1));
        assert!(!store.memory().has_trusted_choice("docker p"));
        assert!(store.memory().accepted_choices("docker p", 3).is_empty());
        assert!(store.memory().prefix("docker p", 3).is_empty());
        store.save(&path).unwrap();
        let mut store = Store::load_with(&path, &cfg).unwrap();
        assert!(!store.memory().has_trusted_choice("docker p"));
        assert!(store.memory().prefix("docker p", 3).is_empty());

        store.record(command, Some(0));
        assert!(store.memory().has_trusted_choice("docker p"));
        assert_eq!(store.memory().prefix("docker p", 1)[0].0, command);
    }

    #[test]
    fn redacts_secrets_on_record() {
        let mut s = Store::new(cfg());
        s.record("export OPENAI_API_KEY=sk-leaked", Some(0));
        let stored = &s.entries().next().unwrap().command;
        assert!(!stored.contains("sk-leaked"), "{stored}");
    }

    #[test]
    fn redacts_secrets_in_output() {
        let mut s = Store::new(cfg());
        s.record("deploy", Some(0));
        s.attach_output("deploy", "using token=ghp_supersecret now");
        let out = s.entries().next().unwrap().output.join(" ");
        assert!(!out.contains("ghp_supersecret"), "{out}");
    }

    #[test]
    fn attaches_output_to_matching_entry() {
        let mut s = Store::new(cfg());
        s.record("ls", Some(0));
        s.attach_output("ls", "a\nb\nc");
        assert_eq!(s.entries().next().unwrap().output, vec!["a", "b", "c"]);
    }

    #[test]
    fn ignores_output_for_unknown_command() {
        let mut s = Store::new(cfg());
        s.record("ls", Some(0));
        s.attach_output("something-else", "noise");
        assert!(s.entries().next().unwrap().output.is_empty());
    }

    #[test]
    fn drops_oversized_output() {
        let mut s = Store::new(ContextConfig {
            ignore_output_over_bytes: 10,
            ..ContextConfig::default()
        });
        s.record("big", Some(0));
        s.attach_output("big", &"x".repeat(500));
        assert!(s.entries().next().unwrap().output.is_empty());
    }

    #[test]
    fn caps_output_bytes() {
        let mut s = Store::new(ContextConfig {
            max_output_bytes: 20,
            ..ContextConfig::default()
        });
        let lines = (0..10)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        s.record("x", Some(0));
        s.attach_output("x", &lines);
        let entry = s.entries().next().unwrap();
        assert!(entry.output_bytes() <= 20, "{:?}", entry.output);
        // The tail is what survives.
        assert!(entry.output.last().unwrap().contains("line-9"));
    }

    #[test]
    fn output_tail_keeps_recent_lines() {
        let mut s = Store::new(cfg());
        s.record("x", Some(0));
        s.attach_output("x", "old-line\nnew-line");
        let tail = s.entries().next().unwrap().output_tail(8);
        assert!(tail.contains("new-line"), "{tail}");
        assert!(!tail.contains("old-line"), "{tail}");
    }

    #[test]
    fn strips_ansi_from_output() {
        let mut s = Store::new(cfg());
        s.record("ls", Some(0));
        s.attach_output("ls", "\x1b[32mfile.txt\x1b[0m");
        assert_eq!(s.entries().next().unwrap().output, vec!["file.txt"]);
    }

    #[test]
    fn persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        let mut s = Store::new(cfg());
        s.record_with_output("git status", Some(0), "clean");
        s.save(&path).unwrap();

        let back = Store::load(&path).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.entries().next().unwrap().command, "git status");
        assert_eq!(back.entries().next().unwrap().output, vec!["clean"]);
    }

    #[test]
    fn missing_history_file_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::load(&dir.path().join("absent.json")).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn relative_commands_keep_their_execution_directory_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(first.join("project")).unwrap();
        std::fs::create_dir_all(second.join("project")).unwrap();
        let first = std::fs::canonicalize(first).unwrap();
        let second = std::fs::canonicalize(second).unwrap();
        let mut store = Store::new(cfg());
        store.record_with_cwd("cd project", Some(0), Some(&first));
        store.record_with_cwd("cd project", Some(0), Some(&second));
        assert_eq!(store.len(), 2);
        let captured_at = store.recent(1)[0].recorded_at;
        assert!(captured_at > 0);
        let path = dir.path().join("history.json");
        store.save(&path).unwrap();
        let restored = Store::load_with(&path, &Config::default()).unwrap();
        assert_eq!(restored.recent(1)[0].recorded_at, captured_at);
        assert_eq!(restored.recent(2)[0].cwd.as_deref(), Some(second.as_path()));
        assert_eq!(restored.recent(2)[1].cwd.as_deref(), Some(first.as_path()));
        assert_eq!(
            restored
                .memory()
                .candidate_evidence("cd project/", Some(&first))
                .uses,
            1
        );
        assert_eq!(
            restored
                .memory()
                .candidate_evidence("cd project/", Some(&second))
                .uses,
            1
        );
    }

    #[test]
    fn seq_continues_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        let mut s = Store::new(cfg());
        s.record("one", Some(0));
        s.save(&path).unwrap();

        let mut back = Store::load(&path).unwrap();
        let seq = back.record("two", Some(0)).unwrap().seq;
        assert_eq!(seq, 2, "seq must not collide after reload");
    }

    #[test]
    fn summary_reports_status() {
        let e = Entry {
            pending: false,
            cwd: None,
            recorded_at: 0,
            command: "ls".into(),
            exit_code: Some(127),
            output: vec![],
            seq: 1,
        };
        assert!(e.summary().starts_with("[exit 127] ls"));
    }
}
