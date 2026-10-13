# Contributing to TermPlete

TermPlete is a Rust library and CLI with zsh and Bash integrations. Contributions
should preserve a responsive shell, accurate context and explicit command
acceptance. The [usage guide](docs/USAGE.md) describes expected behavior, and
the [README feature overview](README.md#features) summarizes the main workflows.

## Development environment

Use macOS or Linux with Rust 1.89 or newer, zsh 5.9 or newer, Python 3 and Node.js 20 or newer.
The native shell integration uses Unix sockets. Native Windows support is not
currently part of the release pipeline. CI pins Rust 1.89 and uses Node.js 24.

```sh
cargo build --locked --release
```

This produces both `target/release/termplete` and the `target/release/tp` launcher.
Keep both executables together. You can invoke them directly without modifying
your normal shell startup file.

## Keep development separate from personal data

Use an isolated configuration parent and socket when experimenting:

```sh
export TERMPLETE_HOME="$(mktemp -d)"
export TERMPLETE_SOCKET="$TERMPLETE_HOME/daemon.sock"
mkdir -p "$TERMPLETE_HOME/termplete"
cat > "$TERMPLETE_HOME/termplete/config.toml" <<'TOML'
mode = "automatic"

[context]
import_shell_history = false
capture_output = false
TOML
./target/release/tp suggest --local-only -- 'git l'
```

These overrides isolate configuration and command storage. They do not isolate
the OS credential store, so use a no-auth loopback mock instead of real provider
names in tests. Integration fixtures already use temporary homes and mock
providers. Do not call config-saving setup functions from ordinary unit tests;
use the in-memory setup API.

## Validation

Run checks relevant to your change, then the required integration checks before
release. The integration commands below explicitly use the release binary:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
TERMPLETE_TEST_BIN="$PWD/target/release/termplete" python3 tests/lifecycle.py
TERMPLETE_TEST_BIN="$PWD/target/release/termplete" python3 tests/shell_pause.py
TERMPLETE_TEST_BIN="$PWD/target/release/termplete" python3 tests/decision.py
TERMPLETE_TEST_BIN="$PWD/target/release/termplete" python3 tests/terminal.py
python3 tests/distribution.py
python3 tests/release.py
node tests/npm-install.js
cd npm
npm pack --dry-run
```

`cargo package --locked --allow-dirty` additionally verifies the Cargo package
and recompiles it. Run packaging checks from the repository root where required.
Hosted CI runs the full checks on macOS and Linux. A local pass is not proof that
the release matrix or public installation flows have run.

## Where changes belong

| Area | Source |
| --- | --- |
| Config, providers, defaults and migrations | `src/config.rs`, `src/setup.rs`, `src/settings.rs` |
| Credential storage and replacement | `src/auth.rs`, `src/main.rs` |
| History and acceptance learning | `src/context.rs`, `src/memory.rs` |
| Context assembly and command sanitization | `src/suggest.rs`, `src/fs_context.rs` |
| Model transports | `src/provider.rs` |
| Decision contract, caching and privacy | `src/decision.rs`, `src/decision_runtime.rs`, `src/privacy.rs` |
| Shared daemon and request protocol | `src/daemon.rs`, `src/protocol.rs`, `src/client.rs` |
| PTY capture and private framing | `src/pty.rs`, `src/pty_stream.rs` |
| Shell rendering and key bindings | `shells/termplete.plugin.zsh`, `shells/termplete.plugin.bash` |
| Distribution | `.github/workflows/`, `scripts/`, `packaging/`, `npm/` |

## Behavioral requirements

- Suggestions only edit the prompt. Keep execution a separate user action.
- Keep model requests off the typing path and reject stale replies after edits.
- Keep all TermPlete visuals absent when the input is empty.
- Preserve the requesting directory and stream command output immediately.
- Keep deterministic credential/private-path filtering before either model.
- Never treat terminal output, historical commands or model output as trusted
  instructions for the program to execute.
- Keep local review restricted to loopback helpers; remote generators still
  receive fully filtered context.
- Preserve the Hybrid prompt-only fallback under Block and fail closed for
  explicit privacy denials or required local review.
- Use readable shortcuts, high-contrast selection, terminal-size-aware layouts,
  and static/no-color fallbacks.
- Maintain backward-compatible configuration aliases where practical.

Prefer focused regression tests that reproduce real behavior. PTY tests are
appropriate for rendering, startup, streaming and shortcuts. Mock HTTP providers
are appropriate for model order, payload filtering, authentication and failure
handling. Live model checks are optional diagnostics, not deterministic CI tests.

## Issues and pull requests

For bugs, include OS, shell, terminal app, TermPlete version, a minimal synthetic
reproduction, expected behavior and the relevant sanitized diagnostic. Do not
attach raw terminal history, API keys or private output. Use the
[security policy](SECURITY.md) for credential exposure or other vulnerabilities.

Describe what changes for users and how you verified it. Update documentation
when a keybinding, default, configuration field, privacy boundary or installation
contract changes. Do not introduce a live provider requirement into the tests.

## Releases and licensing

See [docs/RELEASING.md](docs/RELEASING.md) for native archives, checksums,
Homebrew formula generation and npm publication. Version changes must stay
aligned across Cargo, npm and the release tag. Maintainers must publish and test
the assets before advertising the corresponding install command as available.

The project uses the [MIT license](LICENSE). Contributions are provided under
the same license; retain applicable copyright and license notices.
