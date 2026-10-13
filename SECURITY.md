# Security policy

TermPlete handles terminal commands, captured output, model requests and provider
credentials. Treat accidental disclosure of these data, bypasses of filtering,
and unintended command execution as security issues.

## Reporting a vulnerability

Once the project is hosted on GitHub, use **Security → Report a vulnerability**
if private vulnerability reporting is enabled. If that option is unavailable,
ask the maintainer for a private reporting channel without posting exploit
details or sensitive data in a public issue. No dedicated security email or
response-time guarantee has been established yet.

Provide the affected version or commit, platform and shell, the relevant feature,
and a minimal reproduction using synthetic values. Explain the expected and
observed boundary. Do not send real API keys, credential files, full shell
history or private terminal recordings.

If a key may have leaked, revoke or rotate it with the provider first. Removing
the local keychain entry does not revoke the remote credential.

## Security boundaries

- API keys live in the OS credential store, with an optional environment fallback.
- Commands and output are scrubbed before storage and outbound use. Recognized
  credential paths and configured private paths stay out of model requests.
- A local review helper can receive hard-filtered soft private metadata only
  through a loopback endpoint. A remote generator receives fully filtered data.
- Explicit privacy denials and failed required local review prevent generation.
- Hybrid fallback under Block sends only a deterministically checked active
  prompt and omits all remembered context.
- Terminal output and model replies are untrusted. Accepting a suggestion
  changes the prompt; execution requires a separate user action.
- ChatGPT completions disable inherited Codex apps, plugins, MCP servers and
  lifecycle hooks and notification commands, and withhold terminal context until
  an ephemeral thread and disabled integrations are confirmed.
- Output-capture framing uses a private per-session token. Configuration and
  default socket directories are checked for ownership and private permissions.

These measures reduce accidental exposure. They do not guarantee recognition
of every secret, correctness of a model decision, or safety of a suggested
command. A loopback model may itself have logging or network access. The
terminal integration is not a sandbox against programs running as the same user.

For storage controls and removal, see the [usage guide](docs/USAGE.md). Release
archives are verified against published SHA-256 checksums; this checks integrity
relative to the manifest and is not an independent signature or attestation.
Review [the release runbook](docs/RELEASING.md) for the distribution contract.

## Supported versions

The project is in early development. There is no long-term support branch or
published security-support window. Reports should identify the exact version
or commit. Maintainership and release availability are described in the
repository documentation; release automation does not mean a particular version
has been published.
