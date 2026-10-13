//! Small, local-only filesystem context for directory completion and prompts.
//!
//! This module only reads directory entries. It never runs shell commands or
//! reads file contents.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::SuggestionScope;
use crate::memory::{now, rank_evidence_at, CommandMemory};

const MAX_DIRECTORY_SCAN: usize = 4096;
const MAX_CANDIDATES: usize = 32;
const MAX_SNAPSHOT_SCAN: usize = 2048;
const MAX_SNAPSHOT_BYTES: usize = 2048;

/// Return full command lines that complete a `cd` or `pushd` path to a real
/// directory. Every result starts with `line` byte-for-byte.
///
/// Paths are checked against `cwd`; only immediate directory entries are
/// scanned. The result count and directory scan are both bounded.
pub fn candidates(line: &str, cwd: &Path, max: usize) -> Vec<String> {
    candidates_with_home(line, cwd, max, dirs::home_dir().as_deref())
}

/// Include the latest successful directory creation in the same rank, before
/// applying the result limit or asking a model to choose among candidates.
pub fn candidates_with_context(
    line: &str,
    cwd: &Path,
    max: usize,
    store: &crate::context::Store,
) -> Vec<String> {
    let created = crate::suggest::recent_created_directory(store, Some(cwd));
    ranked_candidates(
        line,
        cwd,
        max,
        store.memory(),
        created.as_ref(),
        store.config().suggestion_scope,
    )
}

fn ranked_candidates(
    line: &str,
    cwd: &Path,
    max: usize,
    memory: &CommandMemory,
    created: Option<&(PathBuf, u64)>,
    scope: SuggestionScope,
) -> Vec<String> {
    let limit = max.min(MAX_CANDIDATES);
    if limit == 0 {
        return Vec::new();
    }

    let remembered = memory
        .directory_candidates()
        .into_iter()
        .map(|(path, _)| path)
        .chain(created.map(|(path, _)| path.clone()));
    let pool = candidate_pool_with_home(line, cwd, dirs::home_dir().as_deref(), remembered);
    let timestamp = now();
    let project = (scope == SuggestionScope::Folder)
        .then(|| project_identity(Some(cwd)))
        .flatten();
    let mut ranked = pool
        .into_iter()
        .map(|candidate| {
            let mut evidence = memory.candidate_evidence_in_project(
                &candidate,
                Some(cwd),
                scope,
                project.as_deref(),
            );
            if let Some((target, recorded_at)) = created {
                if directory_identity(&candidate, Some(cwd)).as_ref() == Some(target) {
                    evidence.context_match = true;
                    evidence.context_last_seen = *recorded_at;
                }
            }
            let ranked = rank_evidence_at(&evidence, timestamp);
            (candidate, evidence, ranked)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(
        |(left_command, left_evidence, left), (right_command, right_evidence, right)| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| {
                    right_evidence
                        .last_used_known
                        .cmp(&left_evidence.last_used_known)
                })
                .then_with(|| right_evidence.last_used.cmp(&left_evidence.last_used))
                .then_with(|| {
                    right_evidence
                        .last_accepted
                        .cmp(&left_evidence.last_accepted)
                })
                .then_with(|| {
                    right_evidence
                        .last_used_order
                        .cmp(&left_evidence.last_used_order)
                })
                .then_with(|| left_command.cmp(right_command))
        },
    );
    ranked
        .into_iter()
        .take(limit)
        .map(|(candidate, _, _)| candidate)
        .collect()
}

/// Recognize an unambiguous single target using the existing shell path parser.
pub(crate) fn created_directory(command: &str, cwd: &Path) -> Option<PathBuf> {
    let command = command.trim();
    let end = command.find(char::is_whitespace)?;
    if &command[..end] != "mkdir" {
        return None;
    }
    let mut argument = command[end..].trim_start();
    loop {
        let word_end = argument.find(char::is_whitespace).unwrap_or(argument.len());
        match &argument[..word_end] {
            "-p" | "--parents" => argument = argument[word_end..].trim_start(),
            "--" => {
                argument = argument[word_end..].trim_start();
                break;
            }
            option if option.starts_with('-') => return None,
            _ => break,
        }
    }
    directory_identity(&format!("cd {argument}"), Some(cwd))
}

/// Resolve a single `cd` or `pushd` command argument to its canonical
/// directory identity. Shell quoting and tilde expansion follow the same
/// rules as completion, and the path must already exist as a directory.
pub fn directory_identity(line: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let home = dirs::home_dir();
    match cwd {
        Some(cwd) => directory_identity_with_home(line, cwd, home.as_deref()),
        None => directory_identity_without_cwd_with_home(line, home.as_deref()),
    }
}

/// Use the nearest Git root as a stable history scope; ordinary folders scope
/// themselves when they are outside a repository.
pub fn project_identity(cwd: Option<&Path>) -> Option<PathBuf> {
    let cwd = fs::canonicalize(cwd?).ok()?;
    Some(
        cwd.ancestors()
            .find(|path| path.join(".git").exists())
            .unwrap_or(&cwd)
            .to_path_buf(),
    )
}

/// Whether the first shell word is exactly `cd` or `pushd`.
///
/// This deliberately recognizes only a plain leading command word, so text
/// such as `echo cd path` or `cdrom path` does not enable directory handling.
pub fn is_directory_command(line: &str) -> bool {
    directory_command_end(line).is_some()
}

/// A bounded JSON snapshot containing the supplied cwd and immediate regular
/// file/directory names. It does not recurse or read file contents.
pub fn snapshot(cwd: &Path) -> String {
    let cwd_label = cwd.to_string_lossy();
    let mut label = cwd_label.into_owned();
    let mut base = snapshot_prefix(&label);

    // Keep the JSON valid even for an unusually long cwd path. A 256-byte
    // label remains below the budget even if every character needs JSON
    // escaping.
    if base.len() + 2 > MAX_SNAPSHOT_BYTES {
        label = truncate_utf8(&label, 256);
        base = snapshot_prefix(&label);
    }

    let mut children = Vec::new();
    if let Ok(entries) = fs::read_dir(cwd) {
        for entry in entries.take(MAX_SNAPSHOT_SCAN).flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let kind = if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else {
                continue;
            };
            children.push((name.to_owned(), kind));
        }
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));

    let mut output = base;
    for (included, (name, kind)) in children.into_iter().enumerate() {
        let item = format!(
            "{}{{\"name\":{},\"kind\":{}}}",
            if included == 0 { "" } else { "," },
            json_string(&name),
            json_string(kind),
        );
        if output.len() + item.len() + 2 > MAX_SNAPSHOT_BYTES {
            break;
        }
        output.push_str(&item);
    }
    output.push_str("]}");
    output
}

fn candidates_with_home(line: &str, cwd: &Path, max: usize, home: Option<&Path>) -> Vec<String> {
    let limit = max.min(MAX_CANDIDATES);
    if limit == 0 {
        return Vec::new();
    }

    let mut candidates = candidate_pool_with_home(line, cwd, home, std::iter::empty());
    candidates.truncate(limit);
    candidates
}

fn directory_identity_with_home(line: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let argument = parsed_directory_argument(line)?;
    if !argument.safe
        || argument.raw.is_empty()
        || argument.value.is_empty()
        || argument.quote.is_some()
    {
        return None;
    }
    let path = resolve_path(&argument.value, argument.raw, cwd, home)?;
    let canonical = fs::canonicalize(path).ok()?;
    canonical.is_dir().then_some(canonical)
}

fn directory_identity_without_cwd_with_home(line: &str, home: Option<&Path>) -> Option<PathBuf> {
    let argument = parsed_directory_argument(line)?;
    if !argument.safe
        || argument.raw.is_empty()
        || argument.value.is_empty()
        || argument.quote.is_some()
    {
        return None;
    }
    let is_unquoted_home = argument.raw.starts_with('~') && argument.value.starts_with('~');
    if !Path::new(&argument.value).is_absolute() && !is_unquoted_home {
        return None;
    }
    directory_identity_with_home(line, Path::new("/"), home)
}

fn parsed_directory_argument(line: &str) -> Option<ParsedArgument<'_>> {
    let command_end = directory_command_end(line)?;
    let after_command = &line[command_end..];
    if after_command.is_empty() {
        return None;
    }

    let arg_start =
        command_end + after_command.len() - after_command.trim_start_matches([' ', '\t']).len();
    let raw_prefix = &line[command_end..arg_start];
    if raw_prefix.is_empty() {
        return None;
    }

    let argument = parse_argument(line, arg_start)?;
    if !argument.safe || (argument.raw.is_empty() && argument.quote_closed) {
        return None;
    }
    Some(argument)
}

fn candidate_pool_with_home(
    line: &str,
    cwd: &Path,
    home: Option<&Path>,
    remembered: impl IntoIterator<Item = PathBuf>,
) -> Vec<String> {
    let Some(argument) = parsed_directory_argument(line) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut seen_directories = std::collections::BTreeSet::new();

    // An already exact directory is useful as a completion even when its
    // parent cannot be listed. Appending `/` also makes the next component
    // available to the shell.
    if !argument.value.is_empty() && !argument.trailing_separator {
        if let Some(path) = resolve_path(&argument.value, argument.raw, cwd, home) {
            if path.is_dir() {
                if let Some(candidate) = append_directory_suffix(line, &argument, "") {
                    push_unique_directory_candidate(
                        candidate,
                        cwd,
                        home,
                        &mut result,
                        &mut seen,
                        &mut seen_directories,
                    );
                }
            }
        }
    }

    let Some((parent_text, partial, trailing_separator)) = split_path(&argument.value) else {
        return result;
    };
    if argument.value == "~" && argument.raw.starts_with('~') {
        return result;
    }
    if partial.is_empty() && !trailing_separator {
        return result;
    }
    let Some(parent) = resolve_path(parent_text, argument.raw, cwd, home) else {
        return result;
    };

    let canonical_parent = fs::canonicalize(&parent).ok();
    if let Some(canonical_parent) = canonical_parent.as_deref() {
        for path in remembered {
            let Ok(path) = fs::canonicalize(path) else {
                continue;
            };
            if !path.is_dir() || path.parent() != Some(canonical_parent) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.chars().any(char::is_control)
                || name.starts_with('.') && !partial.starts_with('.')
            {
                continue;
            }
            let Some(remaining) = matching_suffix(&parent, name, partial, &path) else {
                continue;
            };
            if let Some(candidate) = append_directory_suffix(line, &argument, &remaining) {
                push_unique_directory_candidate(
                    candidate,
                    cwd,
                    home,
                    &mut result,
                    &mut seen,
                    &mut seen_directories,
                );
            }
            if result.len() >= MAX_DIRECTORY_SCAN {
                break;
            }
        }
    }

    let Ok(entries) = fs::read_dir(&parent) else {
        return result;
    };
    let mut matching = Vec::new();
    for entry in entries.take(MAX_DIRECTORY_SCAN).flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.chars().any(char::is_control) {
            continue;
        }
        if name.starts_with('.') && !partial.starts_with('.') {
            continue;
        }
        let remaining = if let Some(remaining) = name.strip_prefix(partial) {
            if !entry
                .metadata()
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            remaining.to_owned()
        } else {
            if !entry
                .metadata()
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(remaining) = matching_suffix(&parent, name, partial, &entry.path()) else {
                continue;
            };
            remaining
        };
        matching.push((name.to_owned(), remaining));
    }
    matching.sort_by(|a, b| a.0.cmp(&b.0));

    for (_, remaining) in matching {
        if result.len() >= MAX_DIRECTORY_SCAN {
            break;
        }
        if let Some(candidate) = append_directory_suffix(line, &argument, &remaining) {
            push_unique_directory_candidate(
                candidate,
                cwd,
                home,
                &mut result,
                &mut seen,
                &mut seen_directories,
            );
        }
    }
    result
}

fn push_unique_directory_candidate(
    candidate: String,
    cwd: &Path,
    home: Option<&Path>,
    result: &mut Vec<String>,
    seen: &mut std::collections::BTreeSet<String>,
    seen_directories: &mut std::collections::BTreeSet<PathBuf>,
) {
    if seen.contains(&candidate) {
        return;
    }
    let Some(identity) = directory_identity_with_home(&candidate, cwd, home) else {
        return;
    };
    if seen_directories.insert(identity) {
        seen.insert(candidate.clone());
        result.push(candidate);
    }
}

fn matching_suffix(parent: &Path, name: &str, partial: &str, path: &Path) -> Option<String> {
    if let Some(remaining) = name.strip_prefix(partial) {
        return Some(remaining.to_owned());
    }
    if !name.is_ascii()
        || !partial.is_ascii()
        || partial.len() > name.len()
        || !name[..partial.len()].eq_ignore_ascii_case(partial)
        || !has_case_insensitive_alias(parent, name, path)
    {
        return None;
    }
    Some(name[partial.len()..].to_owned())
}

fn has_case_insensitive_alias(parent: &Path, name: &str, path: &Path) -> bool {
    let Some((index, ch)) = name.char_indices().find(|(_, ch)| ch.is_ascii_alphabetic()) else {
        return false;
    };
    let replacement = if ch.is_ascii_lowercase() {
        ch.to_ascii_uppercase()
    } else {
        ch.to_ascii_lowercase()
    };
    let mut alias = name.to_owned();
    alias.replace_range(index..index + ch.len_utf8(), &replacement.to_string());
    let Some(actual_identity) = fs::canonicalize(path).ok() else {
        return false;
    };
    fs::canonicalize(parent.join(alias))
        .map(|alias_identity| alias_identity == actual_identity)
        .unwrap_or(false)
}

#[derive(Debug)]
struct ParsedArgument<'a> {
    raw: &'a str,
    value: String,
    quote: Option<char>,
    quote_closed: bool,
    trailing_separator: bool,
    safe: bool,
}

fn directory_command_end(line: &str) -> Option<usize> {
    let start = line
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '\t'))
        .map(|(index, _)| index)?;
    let rest = &line[start..];
    let end = rest
        .char_indices()
        .find(|(_, ch)| {
            matches!(
                ch,
                ' ' | '\t' | '\n' | '\r' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
            )
        })
        .map(|(index, _)| start + index)
        .unwrap_or(line.len());
    let command = &line[start..end];
    if command != "cd" && command != "pushd" {
        return None;
    }
    if end < line.len() && !matches!(line.as_bytes()[end], b' ' | b'\t') {
        return None;
    }
    Some(end)
}

fn parse_argument(line: &str, start: usize) -> Option<ParsedArgument<'_>> {
    let mut value = String::new();
    let mut quote = None;
    let mut quote_closed = false;
    let mut safe = true;
    let mut index = start;
    let mut saw_any = false;

    while index < line.len() {
        let ch = line[index..].chars().next()?;
        if quote.is_none() && (ch.is_whitespace() || is_shell_operator(ch)) {
            break;
        }
        saw_any = true;
        let next_index = index + ch.len_utf8();
        match quote {
            None => match ch {
                '\\' => {
                    let escaped = line[next_index..].chars().next()?;
                    if escaped == '\n' || escaped == '\r' {
                        return None;
                    }
                    value.push(escaped);
                    index = next_index + escaped.len_utf8();
                    quote_closed = false;
                }
                '\'' | '"' => {
                    quote = Some(ch);
                    index = next_index;
                    quote_closed = false;
                }
                _ => {
                    if is_shell_expansion(ch) {
                        safe = false;
                    }
                    value.push(ch);
                    index = next_index;
                    quote_closed = false;
                }
            },
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                    index = next_index;
                    quote_closed = true;
                } else {
                    value.push(ch);
                    index = next_index;
                    quote_closed = false;
                }
            }
            Some('"') => match ch {
                '"' => {
                    quote = None;
                    index = next_index;
                    quote_closed = true;
                }
                '\\' => {
                    let Some(escaped) = line[next_index..].chars().next() else {
                        value.push('\\');
                        index = next_index;
                        quote_closed = false;
                        continue;
                    };
                    if matches!(escaped, '"' | '\\' | '$' | '`') {
                        value.push(escaped);
                        index = next_index + escaped.len_utf8();
                    } else {
                        value.push('\\');
                        index = next_index;
                    }
                    quote_closed = false;
                }
                '$' | '`' => {
                    safe = false;
                    value.push(ch);
                    index = next_index;
                    quote_closed = false;
                }
                _ => {
                    value.push(ch);
                    index = next_index;
                    quote_closed = false;
                }
            },
            _ => return None,
        }
    }

    let raw = &line[start..index];
    if index < line.len() {
        let rest = &line[index..];
        // Whitespace after an already entered path means the argument is
        // complete. Other trailing text means this is a compound command.
        if saw_any || rest.chars().any(|ch| !ch.is_whitespace()) {
            return None;
        }
    }
    if !saw_any && !line[start..].chars().all(|ch| matches!(ch, ' ' | '\t')) {
        return None;
    }
    if value.starts_with('~') && raw.starts_with('~') {
        let after_tilde = &value[1..];
        if !after_tilde.is_empty() && !after_tilde.starts_with('/') {
            safe = false;
        }
    }
    if value.starts_with('#') && raw.starts_with('#') {
        safe = false;
    }
    if value.chars().any(char::is_control) {
        safe = false;
    }

    Some(ParsedArgument {
        raw,
        trailing_separator: value.ends_with('/') || cfg!(windows) && value.ends_with('\\'),
        value,
        quote,
        quote_closed,
        safe,
    })
}

fn is_shell_operator(ch: char) -> bool {
    matches!(ch, ';' | '&' | '|' | '<' | '>' | '(' | ')' | '\n' | '\r')
}

fn is_shell_expansion(ch: char) -> bool {
    matches!(
        ch,
        '$' | '`' | '*' | '?' | '[' | ']' | '!' | '{' | '}' | '^'
    )
}

fn split_path(value: &str) -> Option<(&str, &str, bool)> {
    if value.is_empty() {
        return Some(("", "", true));
    }
    let trailing_separator = value.ends_with('/') || cfg!(windows) && value.ends_with('\\');
    if trailing_separator {
        let trimmed = value.trim_end_matches(['/', '\\']);
        let parent = if trimmed.is_empty() {
            if value.starts_with('/') {
                "/"
            } else {
                ""
            }
        } else {
            trimmed
        };
        return Some((parent, "", true));
    }
    if let Some(index) = value.rfind('/') {
        let parent = if index == 0 { "/" } else { &value[..index] };
        return Some((parent, &value[index + 1..], false));
    }
    if cfg!(windows) {
        if let Some(index) = value.rfind('\\') {
            let parent = if index == 0 { "\\" } else { &value[..index] };
            return Some((parent, &value[index + 1..], false));
        }
    }
    Some(("", value, false))
}

fn resolve_path(value: &str, raw: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let path_value = if raw.starts_with('~') && value.starts_with('~') {
        let home = home?;
        let rest = value.strip_prefix('~')?;
        if !rest.is_empty() && !rest.starts_with('/') {
            return None;
        }
        home.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(value)
    };
    Some(if path_value.is_absolute() {
        path_value
    } else {
        cwd.join(path_value)
    })
}

fn append_directory_suffix(
    line: &str,
    argument: &ParsedArgument<'_>,
    suffix: &str,
) -> Option<String> {
    // A closed quote is already part of the exact typed prefix. We can append
    // `/` after it for an exact directory, but cannot insert missing basename
    // characters before the quote without rewriting the user's input.
    if argument.quote_closed && !suffix.is_empty() {
        return None;
    }

    let mut output = line.to_owned();
    match argument.quote {
        Some('\'') => {
            output.push_str(&encode_single_quoted_suffix(suffix));
            output.push('/');
            output.push('\'');
        }
        Some('"') => {
            output.push_str(&encode_double_quoted_suffix(suffix));
            output.push('/');
            output.push('"');
        }
        None => {
            output.push_str(&encode_unquoted_suffix(suffix));
            output.push('/');
        }
        _ => return None,
    }
    Some(output)
}

fn encode_unquoted_suffix(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '\'' | '"'
                    | '\\'
                    | '$'
                    | '`'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '!'
                    | '{'
                    | '}'
                    | '^'
                    | '#'
                    | '~'
                    | ';'
                    | '&'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
            )
        {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn encode_double_quoted_suffix(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if matches!(ch, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn encode_single_quoted_suffix(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn snapshot_prefix(cwd: &str) -> String {
    format!("{{\"cwd\":{},\"entries\":[", json_string(cwd))
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = value[..end].to_owned();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_identity_uses_nearest_git_root_and_folder_fallback() {
        let workspace = tempfile::tempdir().unwrap();
        let first = workspace.path().join("first");
        let nested = first.join("packages/app");
        let second = workspace.path().join("second");
        fs::create_dir_all(nested.join("src")).unwrap();
        fs::create_dir_all(first.join(".git")).unwrap();
        fs::create_dir_all(second.join(".git")).unwrap();

        assert_eq!(
            project_identity(Some(&nested)),
            Some(first.canonicalize().unwrap())
        );
        assert_eq!(
            project_identity(Some(&second)),
            Some(second.canonicalize().unwrap())
        );

        let plain = workspace.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            project_identity(Some(&plain)),
            Some(plain.canonicalize().unwrap())
        );
        assert_eq!(project_identity(None), None);
    }

    #[test]
    fn project_identity_accepts_git_worktree_files() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join("worktree");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".git"), "gitdir: /tmp/worktrees/example").unwrap();
        assert_eq!(
            project_identity(Some(&root)),
            Some(root.canonicalize().unwrap())
        );
    }
    use std::fs;

    #[test]
    fn completes_only_real_directories_and_preserves_the_input() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nuxt-local-model")).unwrap();
        fs::write(temp.path().join("nuxt-long-file"), "ignored").unwrap();

        let input = "  cd nuxt-lo";
        let found = candidates_with_home(input, temp.path(), 10, None);
        assert_eq!(found, vec!["  cd nuxt-local-model/"]);
        assert!(found[0].starts_with(input));
        assert!(candidates_with_home("cd nuxt-missing", temp.path(), 10, None).is_empty());
    }

    #[test]
    fn completes_nested_absolute_and_tilde_paths() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir_all(home.join("work/project-alpha")).unwrap();
        fs::create_dir_all(temp.path().join("root/child-dir")).unwrap();

        assert_eq!(
            candidates_with_home("cd work/project-a", &home, 10, Some(&home)),
            vec!["cd work/project-alpha/"]
        );
        let absolute = format!("pushd {}/root/child-", temp.path().display());
        assert_eq!(
            candidates_with_home(&absolute, temp.path(), 10, Some(&home)),
            vec![format!("pushd {}/root/child-dir/", temp.path().display())]
        );
        assert_eq!(
            candidates_with_home("cd ~/work/project-a", temp.path(), 10, Some(&home)),
            vec!["cd ~/work/project-alpha/"]
        );
    }

    #[test]
    fn handles_quoted_and_escaped_spaces() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("folder name")).unwrap();

        assert_eq!(
            candidates_with_home("cd \"folder na", temp.path(), 10, None),
            vec!["cd \"folder name/\""]
        );
        assert_eq!(
            candidates_with_home("cd folder\\ na", temp.path(), 10, None),
            vec!["cd folder\\ name/"]
        );
        let identity = fs::canonicalize(temp.path().join("folder name")).unwrap();
        assert_eq!(
            directory_identity_with_home("cd \"folder name\"/", temp.path(), None),
            Some(identity.clone())
        );
        assert_eq!(
            directory_identity_with_home("pushd folder\\ name/", temp.path(), None),
            Some(identity)
        );
    }

    #[test]
    fn directory_identity_normalizes_valid_directory_arguments() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = home.join("Developer/project");
        fs::create_dir_all(&project).unwrap();
        fs::write(home.join("regular-file"), "contents").unwrap();

        let canonical = fs::canonicalize(&project).unwrap();
        assert_eq!(
            directory_identity_with_home("cd ~/Developer/project/", temp.path(), Some(&home)),
            Some(canonical.clone())
        );
        assert_eq!(
            directory_identity_with_home(
                &format!("pushd {}/Developer/project", home.display()),
                temp.path(),
                Some(&home),
            ),
            Some(canonical)
        );
        assert!(directory_identity_with_home("cd ~/missing", temp.path(), Some(&home)).is_none());
        assert!(
            directory_identity_with_home("cd ~/regular-file", temp.path(), Some(&home)).is_none()
        );
        assert!(directory_identity_with_home(
            "cd ~/Developer/project other",
            temp.path(),
            Some(&home)
        )
        .is_none());
        assert!(directory_identity_with_home(
            "cd ~/Developer/project; pwd",
            temp.path(),
            Some(&home)
        )
        .is_none());
        assert!(
            directory_identity_with_home("cd \"~/Developer/project", temp.path(), Some(&home))
                .is_none()
        );
    }

    #[test]
    fn missing_cwd_does_not_resolve_relative_directory_commands() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let relative = temp.path().join("relative");
        let tilde = home.join("project");
        fs::create_dir_all(&relative).unwrap();
        fs::create_dir_all(&tilde).unwrap();

        assert!(directory_identity_without_cwd_with_home("cd relative", Some(&home)).is_none());
        assert_eq!(
            directory_identity_without_cwd_with_home(
                &format!("cd {}/relative/", temp.path().display()),
                Some(&home),
            ),
            Some(fs::canonicalize(&relative).unwrap())
        );
        assert_eq!(
            directory_identity_without_cwd_with_home("cd ~/project/", Some(&home)),
            Some(fs::canonicalize(&tilde).unwrap())
        );
        assert!(
            directory_identity_without_cwd_with_home("cd \"~/project\"", Some(&home)).is_none()
        );
        assert!(directory_identity_without_cwd_with_home("cd \\~/project", Some(&home)).is_none());
    }

    #[test]
    fn escaped_tilde_stays_literal_for_candidates_and_identity() {
        let temp = tempfile::tempdir().unwrap();
        let literal_parent = temp.path().join("~");
        fs::create_dir_all(literal_parent.join("development")).unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join("developer-home")).unwrap();

        assert_eq!(
            candidates_with_home("cd \\~/develop", temp.path(), 10, Some(&home)),
            vec!["cd \\~/development/"]
        );
        assert_eq!(
            directory_identity_with_home("cd \\~/development/", temp.path(), Some(&home)),
            Some(fs::canonicalize(literal_parent.join("development")).unwrap())
        );
    }

    #[test]
    fn case_mismatched_prefix_is_offered_only_when_filesystem_resolves_it() {
        let temp = tempfile::tempdir().unwrap();
        let developer = temp.path().join("Developer");
        fs::create_dir(&developer).unwrap();

        let expected = if has_case_insensitive_alias(temp.path(), "Developer", &developer) {
            vec!["cd developer/".to_owned()]
        } else {
            Vec::new()
        };
        assert_eq!(
            candidates_with_home("cd dev", temp.path(), 10, None),
            expected
        );
    }

    #[test]
    fn remembered_directory_can_enter_the_pool_after_the_scan_window() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..=MAX_DIRECTORY_SCAN {
            fs::create_dir(temp.path().join(format!("project-{index:04}"))).unwrap();
        }
        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        let omitted = entries[MAX_DIRECTORY_SCAN].clone();
        let name = omitted.file_name().unwrap().to_str().unwrap();
        let completion = format!("cd {name}/");

        let ordinary =
            candidate_pool_with_home("cd project-", temp.path(), None, std::iter::empty());
        assert!(!ordinary.contains(&completion));
        let remembered =
            candidate_pool_with_home("cd project-", temp.path(), None, std::iter::once(omitted));
        assert!(remembered.contains(&completion));
    }

    #[test]
    fn memory_ranks_accepted_directory_before_the_display_limit() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("alpha-project")).unwrap();
        fs::create_dir(temp.path().join("zulu-project")).unwrap();
        let cfg = crate::config::ContextConfig::default();
        let mut memory = CommandMemory::default();
        assert_eq!(
            ranked_candidates(
                "cd ",
                temp.path(),
                1,
                &memory,
                None,
                SuggestionScope::Global
            ),
            vec!["cd alpha-project/"]
        );
        for _ in 0..2 {
            assert!(memory.accept_with_cwd("cd ", "cd zulu-project/", &cfg, Some(temp.path()),));
        }

        assert_eq!(
            ranked_candidates(
                "cd ",
                temp.path(),
                1,
                &memory,
                None,
                SuggestionScope::Global
            ),
            vec!["cd zulu-project/"]
        );
    }

    #[test]
    fn live_directory_usage_beats_undated_history_and_undated_ties_keep_file_order() {
        let temp = tempfile::Builder::new()
            .prefix("termplete-directory-import-")
            .tempdir()
            .unwrap();
        fs::create_dir(temp.path().join("a-older")).unwrap();
        fs::create_dir(temp.path().join("z-recent")).unwrap();
        let older = fs::canonicalize(temp.path().join("a-older")).unwrap();
        let recent = fs::canonicalize(temp.path().join("z-recent")).unwrap();
        let history = temp.path().join("history");
        fs::write(
            &history,
            format!("cd {}\ncd {}\n", older.display(), recent.display()),
        )
        .unwrap();
        let cfg = crate::config::ContextConfig::default();
        let mut memory = CommandMemory::default();
        memory.import_file(&history, &cfg).unwrap();
        assert_eq!(
            ranked_candidates(
                "cd ",
                temp.path(),
                1,
                &memory,
                None,
                SuggestionScope::Global
            ),
            vec!["cd z-recent/"]
        );

        fs::write(&history, format!("cd {}\n", older.display()).repeat(100)).unwrap();
        memory.import_file(&history, &cfg).unwrap();
        for _ in 0..2 {
            memory.record_with_cwd("cd z-recent", Some(0), &cfg, Some(temp.path()));
        }
        assert_eq!(
            ranked_candidates(
                "cd ",
                temp.path(),
                1,
                &memory,
                None,
                SuggestionScope::Global
            ),
            vec!["cd z-recent/"]
        );
    }

    #[test]
    fn hidden_directories_require_a_dot_prefix_and_results_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".private")).unwrap();
        fs::create_dir(temp.path().join("visible")).unwrap();

        assert_eq!(
            candidates_with_home("cd ", temp.path(), 1, None),
            vec!["cd visible/"]
        );
        assert_eq!(
            candidates_with_home("pushd ", temp.path(), 1, None),
            vec!["pushd visible/"]
        );
        assert!(candidates_with_home("cd", temp.path(), 10, None).is_empty());
        assert_eq!(
            candidates_with_home("cd .p", temp.path(), 10, None),
            vec!["cd .private/"]
        );
    }

    #[test]
    fn creation_targets_use_the_same_validated_shell_path_identity() {
        let cwd = tempfile::tempdir().unwrap();
        let target = cwd.path().join("orch test");
        fs::create_dir(&target).unwrap();
        for command in [
            "mkdir 'orch test'",
            "mkdir -p \"orch test\"  ",
            "mkdir -p -- orch\\ test",
        ] {
            assert_eq!(
                created_directory(command, cwd.path()),
                Some(fs::canonicalize(&target).unwrap())
            );
        }
        for command in [
            "mkdir missing",
            "mkdir 'orch test' another",
            "mkdir -m 755 'orch test'",
            "mkdir 'orch test'; echo next",
            "mkdir $(echo orch)",
            "echo mkdir 'orch test'",
        ] {
            assert!(
                created_directory(command, cwd.path()).is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn directory_command_detection_is_word_safe() {
        assert!(is_directory_command("  cd path"));
        assert!(is_directory_command("pushd "));
        assert!(!is_directory_command("cdrom path"));
        assert!(!is_directory_command("echo cd path"));
        assert!(!is_directory_command("cd; rm -rf anything"));
    }

    #[test]
    fn snapshot_is_bounded_json_of_immediate_names_only() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("child")).unwrap();
        fs::write(temp.path().join("notes.txt"), "contents must not appear").unwrap();
        fs::write(temp.path().join(".hidden"), "hidden").unwrap();

        let snapshot = snapshot(temp.path());
        assert!(snapshot.len() <= MAX_SNAPSHOT_BYTES);
        assert!(snapshot.starts_with("{\"cwd\":"));
        assert!(snapshot.contains("child"));
        assert!(snapshot.contains("notes.txt"));
        assert!(!snapshot.contains("contents must not appear"));
        assert!(!snapshot.contains(".hidden"));
        let _: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    }
}
