//! Deterministic checks for content that may leave the machine.
//!
//! This module only inspects prompt text. It never opens paths or reads file
//! contents. Known secret values are redacted and lines naming private paths
//! are removed before a request is sent to a model.

use crate::provider::ChatRequest;
use crate::redact;

/// Whether sending `text` unchanged could expose a known secret or private
/// path. This is a local, deterministic check; a model cannot override it.
pub fn sensitive(text: &str) -> bool {
    sensitive_with_paths(text, &[])
}

/// Whether sending `text` unchanged could expose a known secret, private path,
/// or a path denied by the user's configuration.
pub fn sensitive_with_paths(text: &str, denied_paths: &[String]) -> bool {
    if redact::scrub(text) != text {
        return true;
    }
    text.split_inclusive('\n')
        .any(|line| contains_sensitive_path(line) || contains_denied_path(line, denied_paths))
}

/// Credentials and explicitly excluded paths cannot be approved by a model.
pub fn hard_sensitive_with_paths(text: &str, denied_paths: &[String]) -> bool {
    redact::scrub(text) != text
        || text.split_inclusive('\n').any(|line| {
            contains_path_component(line, is_credential_path_component)
                || contains_denied_path(line, denied_paths)
        })
}

/// Return a copy of `req` with obvious secrets masked and sensitive path lines
/// removed from every message body.
pub fn sanitize_request(req: &ChatRequest) -> ChatRequest {
    sanitize_request_with_paths(req, &[])
}

/// Like [`sanitize_request`], also removing lines that contain any configured
/// denied path. Denied paths use case-sensitive substring matching.
pub fn sanitize_request_with_paths(req: &ChatRequest, denied_paths: &[String]) -> ChatRequest {
    sanitize_with_rule(req, denied_paths, is_sensitive_path_component)
}

/// Remove hard exclusions while retaining other private metadata for local
/// review. Callers must never send this request to an unapproved remote model.
pub(crate) fn sanitize_for_review_with_paths(
    req: &ChatRequest,
    denied_paths: &[String],
) -> ChatRequest {
    sanitize_with_rule(req, denied_paths, is_credential_path_component)
}

fn sanitize_with_rule(
    req: &ChatRequest,
    denied_paths: &[String],
    component_rule: fn(&str) -> bool,
) -> ChatRequest {
    let mut sanitized = req.clone();
    for message in &mut sanitized.messages {
        message.content = sanitize_text(&message.content, denied_paths, component_rule);
    }
    sanitized
}

fn sanitize_text(text: &str, denied_paths: &[String], component_rule: fn(&str) -> bool) -> String {
    let redacted = redact::scrub(text);
    let mut out = String::with_capacity(redacted.len());
    for line in redacted.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        if contains_path_component(body, component_rule) || contains_denied_path(body, denied_paths)
        {
            out.push_str("[sensitive context omitted]");
        } else {
            out.push_str(body);
        }
        out.push_str(newline);
    }
    out
}

fn contains_denied_path(text: &str, denied_paths: &[String]) -> bool {
    denied_paths
        .iter()
        .filter(|path| !path.is_empty())
        .any(|path| text.contains(path))
}

fn contains_sensitive_path(text: &str) -> bool {
    contains_path_component(text, is_sensitive_path_component)
}

fn contains_path_component(text: &str, component_rule: fn(&str) -> bool) -> bool {
    // JSON can escape path separators, so normalize just that spelling before
    // examining components. This does not decode or evaluate prompt content.
    let normalized = text.replace("\\/", "/");
    normalized
        .split(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    ':' | '=' | ',' | ';' | '"' | '\'' | '`' | '{' | '}' | '[' | ']' | '(' | ')'
                )
        })
        .filter(|part| !part.is_empty())
        .any(|token| {
            let mut components = token.split(['/', '\\']).enumerate().peekable();
            while let Some((index, component)) = components.next() {
                // macOS canonicalizes /var, /tmp and /etc through /private.
                // That OS prefix is not a user-created private directory.
                let system_alias = index == 1
                    && token.starts_with("/private/")
                    && components
                        .peek()
                        .is_some_and(|(_, next)| matches!(*next, "var" | "tmp" | "etc"));
                if !system_alias && component_rule(component) {
                    return true;
                }
            }
            false
        })
}

fn is_sensitive_path_component(component: &str) -> bool {
    let component =
        component.trim_matches(|ch: char| matches!(ch, '?' | '!' | '<' | '>' | '$' | '\\' | '*'));
    if component.is_empty() || component == "." || component == ".." {
        return false;
    }

    let lower = component.to_ascii_lowercase();
    lower.starts_with('.')
        || is_credential_path_component(component)
        || ["secret", "secrets", "private", "confidential", "vault"]
            .iter()
            .any(|name| {
                lower == *name
                    || lower.strip_prefix(*name).is_some_and(|suffix| {
                        suffix
                            .chars()
                            .next()
                            .is_some_and(|ch| matches!(ch, '-' | '_' | '.'))
                    })
            })
}

fn is_credential_path_component(component: &str) -> bool {
    let lower = component.to_ascii_lowercase();
    lower.starts_with("credential")
        || lower.starts_with(".env")
        || matches!(
            lower.as_str(),
            ".ssh"
                | ".aws"
                | ".gnupg"
                | ".azure"
                | ".kube"
                | ".docker"
                | ".npmrc"
                | ".netrc"
                | ".pypirc"
        )
        || matches!(
            lower.as_str(),
            "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519"
        )
        || lower.contains("private_key")
        || lower.contains("private-key")
        || lower.contains("private.key")
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|ext| lower.trim_end_matches('.').ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Message, Sampling};

    #[test]
    fn local_review_preserves_metadata_but_never_credentials_or_exclusions() {
        let request = ChatRequest::new("m", vec![Message::user(
            "look /work/.cache-project/report.txt METADATA\nAPI_KEY=actual-secret\ncat ~/.ssh/config SSH_MARKER\ncat /work/deny/report.txt DENIED_MARKER\n"
        )]);
        let denied = vec!["/work/deny".into()];
        let reviewed = sanitize_for_review_with_paths(&request, &denied);
        let content = &reviewed.messages[0].content;
        assert!(content.contains("/work/.cache-project/report.txt METADATA"));
        for excluded in ["actual-secret", "SSH_MARKER", "DENIED_MARKER"] {
            assert!(!content.contains(excluded));
        }
        assert!(!sanitize_request_with_paths(&request, &denied).messages[0]
            .content
            .contains("METADATA"));
        assert!(!hard_sensitive_with_paths(
            "look /work/.cache-project/report.txt",
            &[]
        ));
        for forbidden in [
            "API_KEY=actual-secret",
            "cat ~/.ssh/config",
            "cat /work/deny/report.txt",
        ] {
            assert!(hard_sensitive_with_paths(forbidden, &denied));
        }
    }

    #[test]
    fn request_masks_secrets_and_omits_private_path_lines() {
        let request = ChatRequest::new(
            "local-model",
            vec![Message::user(
                "OPENAI_API_KEY=sk-example\ncat ~/.ssh/id_ed25519\necho visible\n",
            )],
        );
        let sanitized = sanitize_request(&request);
        let content = &sanitized.messages[0].content;
        assert!(!content.contains("sk-example"));
        assert!(!content.contains("id_ed25519"));
        assert!(content.contains("OPENAI_API_KEY=***"));
        assert!(content.contains("[sensitive context omitted]"));
        assert!(content.contains("echo visible"));
        assert_eq!(sanitized.model, request.model);
        assert_eq!(sanitized.sampling, request.sampling);
    }

    #[test]
    fn private_and_hidden_paths_are_sensitive_but_safe_text_is_not() {
        for text in [
            "cat .env.local",
            "cat /home/user/.aws/credentials",
            "openssl req -key server.pem",
            "cat /home/user/.ssh/config",
        ] {
            assert!(sensitive(text), "expected sensitive: {text}");
        }
        assert!(!sensitive("git status --short"));
    }

    #[test]
    fn generic_secret_directory_components_are_private() {
        for text in [
            "cat /home/user/secret/settings.toml",
            "cat /home/user/secrets/config.json",
            "cat /home/user/private/key",
            "cat /home/user/confidential/notes.txt",
            "cat /home/user/vault/records.db",
        ] {
            assert!(sensitive(text), "expected sensitive: {text}");
        }
    }

    #[test]
    fn macos_system_aliases_preserve_public_context_but_not_private_children() {
        for path in [
            "/private/var/folders/test/project",
            "/private/tmp/project/src/main.rs",
            "/private/etc/hosts",
        ] {
            assert!(!sensitive(path), "system alias should be public: {path}");
        }
        for path in [
            "/private/var/folders/test/.env",
            "/private/tmp/private/customer.txt",
            "/work/private/var/customer.txt",
            "/private/test-data/customer.txt",
        ] {
            assert!(sensitive(path), "private path should remain local: {path}");
        }
    }

    #[test]
    fn configured_denied_paths_are_case_sensitive_and_line_scoped() {
        let denied = vec!["/work/deny-fragment".to_owned()];
        let request = ChatRequest::new(
            "m",
            vec![Message::user(
                "read /work/deny-fragment/file.txt\nread /work/Deny-fragment/public.txt\n",
            )],
        )
        .with_sampling(Sampling::completion());
        let sanitized = sanitize_request_with_paths(&request, &denied);
        let content = &sanitized.messages[0].content;
        assert!(content.starts_with("[sensitive context omitted]\n"));
        assert!(content.contains("read /work/Deny-fragment/public.txt"));
        assert!(sensitive_with_paths(
            "read /work/deny-fragment/file.txt",
            &denied
        ));
        assert!(!sensitive_with_paths(
            "read /work/Deny-fragment/file.txt",
            &denied
        ));
    }
}
