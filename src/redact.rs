//! Redaction of secrets before anything is persisted or sent to a model.
//!
//! Context is echoed to a third-party endpoint, so the daemon runs every captured
//! command and output chunk through this module first. The goal is not perfect
//! DLP; it is to never write an API key into the history file and to keep obvious
//! credential material out of prompts.

/// Environment variables whose values are replaced with [`MASK`].
const SECRET_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AZURE_OPENAI_API_KEY",
    "COHERE_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GROQ_API_KEY",
    "HF_TOKEN",
    "HUGGINGFACEHUB_API_TOKEN",
    "MISTRAL_API_KEY",
    "NVIDIA_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "PERPLEXITY_API_KEY",
    "REPLICATE_API_TOKEN",
    "TOGETHER_API_KEY",
    "XAI_API_KEY",
];

/// Query-string / JSON-key names whose values are replaced with [`MASK`].
const SECRET_PARAM_NAMES: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "auth",
    "auth_token",
    "authorization",
    "client_secret",
    "key",
    "password",
    "passwd",
    "private_key",
    "refresh_token",
    "secret",
    "session",
    "signature",
    "token",
];

const MASK: &str = "***";

/// Replace the value of any secret-looking `NAME=value` assignment.
///
/// Handles `export FOO=bar`, bare `FOO=bar` and quoted values, in every line of
/// `text`.
fn redact_env_values(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut i = 0;

    while i < bytes.len() {
        if !is_ident_start(bytes[i]) || (i > 0 && is_ident_char(bytes[i - 1])) {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }

        let name_start = i;
        while i < bytes.len() && is_ident_char(bytes[i]) {
            i += 1;
        }
        let name = &text[name_start..i];
        if bytes.get(i) != Some(&b'=') || !is_secret_env(name) {
            continue;
        }

        let value_start = i + 1;
        let (value_end, content_start, content_end) = assignment_value_span(text, value_start);
        out.push_str(&text[cursor..content_start]);
        out.push_str(&mask_preserving_newlines(&text[content_start..content_end]));
        out.push_str(&text[content_end..value_end]);
        cursor = value_end;
        i = value_end;
    }

    out.push_str(&text[cursor..]);
    out
}

/// Mask the body of PEM-style private-key blocks while preserving their
/// boundary markers and line structure. Public keys and certificates remain
/// unchanged.
fn redact_private_key_blocks(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    const BEGIN: &str = "-----begin ";
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut search = 0;

    while search < text.len() {
        let Some(relative) = lower[search..].find(BEGIN) else {
            break;
        };
        let begin_at = search + relative;
        let label_start = begin_at + BEGIN.len();
        let Some(label_end_relative) = lower[label_start..].find("-----") else {
            search = label_start;
            continue;
        };
        let label_end = label_start + label_end_relative;
        let label = lower[label_start..label_end].trim();
        if !label.contains("private key") {
            search = label_end + 5;
            continue;
        }

        let begin_end = label_end + 5;
        let end_marker = format!("-----end {label}-----");
        let data_start = if lower[begin_end..].starts_with("\r\n") {
            begin_end + 2
        } else if lower[begin_end..].starts_with('\n') {
            begin_end + 1
        } else {
            begin_end
        };
        let end_at = lower[begin_end..]
            .find(&end_marker)
            .map(|relative| begin_end + relative);
        let content_end = match end_at {
            Some(end_at) => lower[..end_at]
                .rfind('\n')
                .map(|newline| newline + 1)
                .filter(|line_start| *line_start >= data_start)
                .unwrap_or(end_at),
            None => text.len(),
        };

        out.push_str(&text[cursor..data_start]);
        out.push_str(&mask_preserving_newlines(&text[data_start..content_end]));
        cursor = content_end;
        if let Some(end_at) = end_at {
            search = end_at + end_marker.len();
        } else {
            search = text.len();
            cursor = text.len();
        }
    }

    out.push_str(&text[cursor..]);
    out
}

/// Mask high-confidence token formats even when they appear without a
/// credential-like field name, such as in terminal output or JSON strings.
fn redact_bare_tokens(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_' {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if i > 0 && is_bare_token_char(bytes[i - 1]) {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }

        let start = i;
        while i < bytes.len() && is_bare_token_char(bytes[i]) {
            i += 1;
        }
        let token = &text[start..i];
        if is_recognizable_secret(token) {
            out.push_str(&text[cursor..start]);
            out.push_str(MASK);
            cursor = i;
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// Replace secret-looking `?key=value` / `&key=value` query parameter values.
fn redact_query_params(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let Some(rel) = text[i..].find(['?', '&']) else {
            out.push_str(&text[i..]);
            return out;
        };
        let sep_at = i + rel;
        let param_start = sep_at + 1;
        let param_end = text[param_start..]
            .find('&')
            .map_or(text.len(), |e| param_start + e);
        out.push_str(&text[i..=sep_at]);
        out.push_str(&mask_param(&text[param_start..param_end]));
        i = param_end;
    }
    out
}

/// Replace the value of secret-looking `"key": value` pairs in JSON-ish text.
fn redact_json_fields(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let Some(rel) = text[i..].find(':') else {
            out.push_str(&text[i..]);
            return out;
        };
        let colon = i + rel;
        // The key must be a quoted string immediately preceding the colon.
        let Some(key) = quoted_key_before(&text[..colon]) else {
            out.push_str(&text[i..=colon]);
            i = colon + 1;
            continue;
        };
        if !is_secret_name(key) {
            out.push_str(&text[i..=colon]);
            i = colon + 1;
            continue;
        }
        let value_start = colon + 1;
        let value_end = text[value_start..]
            .find([',', '\n', '}'])
            .map_or(text.len(), |e| value_start + e);
        out.push_str(&text[i..=colon]);
        out.push_str(MASK);
        i = value_end;
    }
    out
}

/// Redact credentials in common HTTP authentication headers.
///
/// Handles bare header text and headers embedded in commands such as
/// `curl -H 'Authorization: Bearer …'`.
fn redact_auth_headers(text: &str) -> String {
    const HEADERS: &[(&str, bool)] = &[
        ("proxy-authorization:", true),
        ("authorization:", true),
        ("x-api-key:", false),
        ("x-auth-token:", false),
        ("cookie:", false),
        ("set-cookie:", false),
    ];

    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut search = 0;
    while search < text.len() {
        let next = HEADERS
            .iter()
            .filter_map(|(header, preserve_scheme)| {
                lower[search..]
                    .find(header)
                    .map(|relative| (search + relative, header.len(), *preserve_scheme))
            })
            .min_by_key(|(at, _, _)| *at);
        let Some((at, header_len, preserve_scheme)) = next else {
            break;
        };
        let value_start = at + header_len;
        let mut end = value_start;
        while end < text.len() {
            let ch = text[end..].chars().next().expect("valid UTF-8 boundary");
            if matches!(ch, '\n' | '\r' | '\'' | '"' | ';') {
                break;
            }
            end += ch.len_utf8();
        }

        out.push_str(&text[cursor..value_start]);
        let value = &text[value_start..end];
        let leading = value.len() - value.trim_start().len();
        out.push_str(&value[..leading]);
        if preserve_scheme {
            let trimmed = &value[leading..];
            let scheme_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
            let scheme = &trimmed[..scheme_end];
            if matches!(
                scheme.to_ascii_lowercase().as_str(),
                "bearer" | "basic" | "digest" | "oauth"
            ) && scheme_end < trimmed.len()
            {
                out.push_str(scheme);
                out.push(' ');
                out.push_str(MASK);
            } else if !trimmed.is_empty() {
                out.push_str(MASK);
            }
        } else if !value[leading..].is_empty() {
            out.push_str(MASK);
        }
        cursor = end;
        search = end.max(value_start);
    }
    out.push_str(&text[cursor..]);
    out
}

/// Redact `user:password@` URL user-info when it appears in a URL.
fn redact_url_userinfo(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut search = 0;
    while let Some(relative) = text[search..].find("://") {
        let scheme_end = search + relative + 3;
        let mut authority_end = scheme_end;
        while authority_end < bytes.len() {
            let ch = text[authority_end..]
                .chars()
                .next()
                .expect("valid UTF-8 boundary");
            if ch.is_whitespace() || matches!(ch, '/' | '?' | '#' | '\'' | '"' | '>') {
                break;
            }
            authority_end += ch.len_utf8();
        }
        let authority = &text[scheme_end..authority_end];
        if let Some(at) = authority.find('@') {
            let userinfo = &authority[..at];
            if userinfo.contains(':') {
                let at_index = scheme_end + at;
                out.push_str(&text[cursor..scheme_end]);
                out.push_str(MASK);
                out.push('@');
                cursor = at_index + 1;
            }
        }
        search = authority_end.max(scheme_end);
    }
    out.push_str(&text[cursor..]);
    out
}

/// Redact curl-style user/password and password option values.
fn redact_cli_credentials(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let options = ["--username", "--password", "--user", "--pass", "-u"];
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut search = 0;
    while search < text.len() {
        let next = options
            .iter()
            .filter_map(|option| {
                lower[search..]
                    .find(option)
                    .map(|relative| (search + relative, *option))
            })
            .filter(|(at, option)| {
                let after = at + option.len();
                let before_ok = *at == 0 || !bytes_ident(lower.as_bytes()[at - 1]);
                let after_ok = if *option == "-u" {
                    true // curl also accepts an attached `-uuser:password` value
                } else {
                    after == text.len()
                        || matches!(lower.as_bytes()[after], b'=' | b' ' | b'\t' | b'\'' | b'"')
                };
                before_ok && after_ok
            })
            .min_by_key(|(at, _)| *at);
        let Some((at, option)) = next else {
            break;
        };
        let mut value_start = at + option.len();
        while value_start < text.len() && matches!(bytes_at(text, value_start), Some(b' ' | b'\t'))
        {
            value_start += 1;
        }
        if text.as_bytes().get(value_start) == Some(&b'=') {
            value_start += 1;
        }
        while value_start < text.len() && matches!(bytes_at(text, value_start), Some(b' ' | b'\t'))
        {
            value_start += 1;
        }
        if value_start >= text.len() {
            search = value_start;
            continue;
        }

        let (value_end, content_start, content_end, quoted) = cli_value_span(text, value_start);
        let is_password = option != "-u" && option != "--user" && option != "--username";
        let has_user_password = text[content_start..content_end].contains(':');
        if is_password || has_user_password {
            out.push_str(&text[cursor..content_start]);
            out.push_str(MASK);
            if quoted {
                out.push_str(&text[content_end..value_end]);
            }
            cursor = value_end;
            search = value_end.max(value_start);
        } else {
            search = value_end.max(value_start + 1);
        }
    }
    out.push_str(&text[cursor..]);
    out
}

fn assignment_value_span(text: &str, start: usize) -> (usize, usize, usize) {
    match text.as_bytes().get(start) {
        Some(b'"' | b'\'') => {
            let quote = text.as_bytes()[start];
            let content_start = start + 1;
            let mut i = content_start;
            while i < text.len() {
                let byte = text.as_bytes()[i];
                if byte == quote && !is_escaped(text.as_bytes(), i) {
                    return (i + 1, content_start, i);
                }
                i += text[i..].chars().next().map_or(1, char::len_utf8);
            }
            (text.len(), content_start, text.len())
        }
        _ => {
            let end = text[start..]
                .find(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '&' | '|'))
                .map_or(text.len(), |offset| start + offset);
            (end, start, end)
        }
    }
}

fn cli_value_span(text: &str, start: usize) -> (usize, usize, usize, bool) {
    match text.as_bytes().get(start) {
        Some(b'"' | b'\'') => {
            let quote = text.as_bytes()[start];
            let content_start = start + 1;
            let mut i = content_start;
            while i < text.len() {
                if text.as_bytes()[i] == quote && !is_escaped(text.as_bytes(), i) {
                    return (i + 1, content_start, i, true);
                }
                i += text[i..].chars().next().map_or(1, char::len_utf8);
            }
            (text.len(), content_start, text.len(), true)
        }
        _ => {
            let end = text[start..]
                .find(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '&' | '|'))
                .map_or(text.len(), |offset| start + offset);
            (end, start, end, false)
        }
    }
}

fn mask_preserving_newlines(value: &str) -> String {
    let mut masked = String::from(MASK);
    masked.extend(value.chars().filter(|ch| *ch == '\n'));
    masked
}

fn is_escaped(bytes: &[u8], at: usize) -> bool {
    let mut slashes = 0;
    let mut i = at;
    while i > 0 && bytes[i - 1] == b'\\' {
        slashes += 1;
        i -= 1;
    }
    slashes % 2 == 1
}

fn bytes_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_bare_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn is_recognizable_secret(token: &str) -> bool {
    let minimum_suffix = |prefix: &str| {
        token
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.len() >= 12)
    };
    if minimum_suffix("sk-proj-")
        || minimum_suffix("sk-ant-")
        || minimum_suffix("sk-")
        || minimum_suffix("ghp_")
        || minimum_suffix("github_pat_")
        || minimum_suffix("xoxb-")
    {
        return true;
    }
    if token.starts_with("AKIA")
        && token.len() == 20
        && token[4..]
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return true;
    }

    let mut segments = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    [header, payload, signature].iter().all(|segment| {
        segment.len() >= 12
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

fn bytes_at(text: &str, index: usize) -> Option<u8> {
    text.as_bytes().get(index).copied()
}

/// Return the last `"..."` group in `head` when it directly abuts the end.
fn quoted_key_before(head: &str) -> Option<&str> {
    let head = head.trim_end();
    let close = head.rfind('"')?;
    if close == 0 {
        return None;
    }
    let open = head[..close].rfind('"')?;
    Some(&head[open + 1..close])
}

fn mask_param(param: &str) -> String {
    match param.split_once('=') {
        Some((k, _)) if is_secret_name(k) => format!("{k}={MASK}"),
        _ => param.to_string(),
    }
}

fn is_secret_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("API_KEY")
        || upper.contains("APIKEY")
        || upper.contains("ACCESS_KEY")
        || upper.contains("PRIVATE_KEY")
        || upper.contains("CREDENTIAL")
        || SECRET_ENV_VARS.iter().any(|k| *k == upper)
}

fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_PARAM_NAMES.iter().any(|p| *p == lower)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Full pipeline the daemon runs before persisting or prompting.
pub fn scrub(text: &str) -> String {
    redact_bare_tokens(&redact_private_key_blocks(&redact_url_userinfo(
        &redact_cli_credentials(&redact_auth_headers(&redact_json_fields(
            &redact_query_params(&redact_env_values(text)),
        ))),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_bare_secret_assignment() {
        assert_eq!(
            redact_env_values("OPENAI_API_KEY=sk-abc123 rest"),
            "OPENAI_API_KEY=*** rest"
        );
    }

    #[test]
    fn masks_export_form_and_keeps_quote() {
        assert_eq!(
            redact_env_values("export GITHUB_TOKEN=\"ghp_zzz\""),
            "export GITHUB_TOKEN=\"***\""
        );
    }

    #[test]
    fn leaves_non_secret_assignments_alone() {
        let line = "PORT=3000 DEBUG=true";
        assert_eq!(redact_env_values(line), line);
    }

    #[test]
    fn masks_secret_env_later_in_line() {
        assert_eq!(
            redact_env_values("PORT=1 AWS_SECRET_ACCESS_KEY=xyz"),
            "PORT=1 AWS_SECRET_ACCESS_KEY=***"
        );
    }

    #[test]
    fn masks_multiple_secrets_and_multiline_quoted_values() {
        let input =
            "TOKEN=first AWS_SECRET_ACCESS_KEY=second\nPRIVATE_KEY=\"BEGIN\nsecret-body\nEND\"";
        let output = redact_env_values(input);
        assert!(!output.contains("first"));
        assert!(!output.contains("second"));
        assert!(!output.contains("secret-body"));
        assert!(output.contains("TOKEN=*** AWS_SECRET_ACCESS_KEY=***"));
        assert!(output.contains("PRIVATE_KEY=\"***\n\n\""));
    }

    #[test]
    fn masks_auth_headers_cli_passwords_and_url_userinfo() {
        let input = "curl -H 'Authorization: Bearer live-token' --user 'alice:live-pass' https://alice:url-pass@example.test/x";
        let output = scrub(input);
        for secret in ["live-token", "live-pass", "url-pass"] {
            assert!(!output.contains(secret), "{secret} leaked in {output}");
        }
        assert!(output.contains("Authorization: Bearer ***"));
        assert!(output.contains("--user '***'"));
        assert!(output.contains("https://***@example.test"));
    }

    #[test]
    fn masks_private_key_blocks_in_stdout_and_json_strings() {
        let body = "PRIVATE_KEY_BODY_SENTINEL";
        let stdout = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{body}\nsecond-private-line\n-----END OPENSSH PRIVATE KEY-----"
        );
        let scrubbed_stdout = scrub(&stdout);
        assert!(!scrubbed_stdout.contains(body));
        assert!(!scrubbed_stdout.contains("second-private-line"));
        assert!(scrubbed_stdout.contains("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(scrubbed_stdout.contains("-----END OPENSSH PRIVATE KEY-----"));

        let json = format!(
            r#"{{"log":"-----BEGIN RSA PRIVATE KEY-----\n{body}\n-----END RSA PRIVATE KEY-----"}}"#
        );
        let scrubbed_json = scrub(&json);
        assert!(!scrubbed_json.contains(body));
        assert!(scrubbed_json.contains("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(scrubbed_json.contains("-----END RSA PRIVATE KEY-----"));
    }

    #[test]
    fn masks_known_bare_token_formats_without_changing_hashes_or_paths() {
        let git_hash = "0123456789abcdef0123456789abcdef01234567";
        let tokens = [
            "sk-proj-abcdefghijkl",
            "sk-ant-abcdefghijkl",
            "sk-abcdefghijkl",
            "ghp_abcdefghijkl",
            "github_pat_abcdefghijkl",
            "xoxb-abcdefghijkl",
            "AKIAIOSFODNN7EXAMPLE",
            "eyJhbGciOiJub25lIn0.eyJzdWIiOiIxMjM0NSJ9.c2lnbmF0dXJlMTIzNDU2",
        ];
        let output = scrub(&format!(
            "stdout {}\n{{\"key\":\"{}\",\"jwt\":\"{}\",\"hash\":\"{}\",\"path\":\"src/main.rs\"}}",
            tokens[0], tokens[1], tokens[7], git_hash
        ));
        for token in [&tokens[0], &tokens[1], &tokens[7]] {
            assert!(!output.contains(token), "{token} leaked in {output}");
        }
        let remaining = scrub(&tokens[2..6].join(" "));
        for token in &tokens[2..6] {
            assert!(!remaining.contains(token), "{token} leaked in {remaining}");
        }
        assert!(output.contains(git_hash));
        assert!(output.contains("src/main.rs"));
    }

    #[test]
    fn unmatched_quoted_secret_is_masked_through_end_of_text() {
        assert_eq!(
            redact_env_values("PRIVATE_KEY=\"partial\nsecret"),
            "PRIVATE_KEY=\"***\n"
        );
    }

    #[test]
    fn masks_query_params() {
        assert_eq!(
            redact_query_params("https://x/y?token=abc&page=2"),
            "https://x/y?token=***&page=2"
        );
    }

    #[test]
    fn leaves_safe_query_params_alone() {
        let u = "https://x/y?page=2&size=10";
        assert_eq!(redact_query_params(u), u);
    }

    #[test]
    fn masks_secret_json_field() {
        assert_eq!(
            redact_json_fields("{\"api_key\": \"abcd\", \"n\": 1}"),
            "{\"api_key\":***, \"n\": 1}"
        );
    }

    #[test]
    fn leaves_safe_json_field_alone() {
        let j = "{\"name\": \"abcd\", \"n\": 1}";
        assert_eq!(redact_json_fields(j), j);
    }

    #[test]
    fn scrub_composes_all_passes() {
        let out = scrub("curl 'https://a/b?token=t1'; API_KEY=zzz");
        assert!(!out.contains("t1"), "query token leaked: {out}");
        assert!(!out.contains("zzz"), "env secret leaked: {out}");
    }
}
