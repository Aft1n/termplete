# Configuration and architecture reference

Use [the usage guide](USAGE.md) for everyday workflows and
[the README](../README.md) for installation and features. This reference covers
the configuration file, environment overrides, animation controls and source map.
The [decision-model guide](decision-models.md) describes the helper contract.

## Configuration

`~/.termplete/config.toml`:

```toml
provider = "openai"
model = "YOUR_PROVIDER_MODEL_ID" # replace with a model available to your account
mode = "automatic"
enabled = true # pause and resume from Settings → Preferences
disabled_terminals = ["warp"] # use TermPlete everywhere except Warp

[modes]
min_chars = 2
on_demand_debounce_ms = 220      # wait this long after a keystroke before asking
automatic_debounce_ms = 400
max_suggestions = 3

[keys]
reveal = "Ctrl+G"       # reveal a held suggestion
accept = "Right Arrow"  # insert it into the buffer (never runs it)
cycle = "Shift+Tab"     # previous candidate
dismiss = "Ctrl+C"
toggle = "Ctrl+X Ctrl+X"

[style]
ghost_color = "#6b7280"
animations = true
animation_frames = 12
spinner = true

[context]
history_entries = 200
command_memory_entries = 10000
import_shell_history = true
history_candidates = 8
ai_select_history = false   # true lets the model pick an existing history match
suggestion_scope = "folder" # "folder" scopes ranking to the project; "global" shares it across folders
capture_output = true      # capture recent zsh results automatically
max_output_bytes = 4096       # output kept per command
ignore_prefixes = ["sudo ", "passwd", "ssh-keygen"]

[daemon]
autostart = true              # start on demand when a shell asks
idle_shutdown_secs = 1800     # exit after this long with no shells attached

[decision]
enabled = true                 # master switch; llm strategy keeps helper inactive
strategy = "llm"              # llm, decision, hybrid
protocol = "auto"             # auto, system-one, chat
provider = "ollama"
model = "tev1:0.8b"
timeout_ms = 1500             # 100–10000; interactive latency budget
local_only = true             # require a loopback helper endpoint
private_paths = []            # e.g. ["/work/customer-private"]
# base_url = "http://127.0.0.1:11434"  # optional helper-only override

[privacy]
sensitive_context = "block"   # block or decision (local review of soft path metadata)
```

Config, command history, and `daemon.log` share `~/.termplete`, alongside
other home-directory tool folders such as `~/.codex`. The old platform config
folder is moved automatically on first use when `~/.termplete` does not exist.
An existing `~/.termplete` is never replaced.

Set `TERMPLETE_HOME` to use `<TERMPLETE_HOME>/termplete`, or set
`XDG_CONFIG_HOME` to use `<XDG_CONFIG_HOME>/termplete`. Explicit overrides
keep their existing paths and are not migrated.

Choose **Preferences → Terminal apps** in `tp settings` to toggle TermPlete for
Warp, iTerm2, macOS Terminal, VS Code, WezTerm, Ghostty, Kitty, or Alacritty.
Other apps can be added using their `TERM_PROGRAM` value. The exclusion list is
empty by default; names are case insensitive, and `WarpTerminal` also matches
`warp`. `tp status` shows the detected app and whether TermPlete is enabled.

You can also run `tp config set --key disabled_terminals --value warp,iterm`.
Excluded apps load no TermPlete bindings or hooks and send no suggestion or
history requests; other terminals stay enabled. To restore the original
shortcuts in an already open terminal, close its tab and open a fresh shell.

Use `tp stop` to pause the integration globally and `tp start` to resume it.
Already-loaded shells apply the change at their next prompt, while shells
opened during a pause need to be reopened after `tp start`. The **Preferences →
TermPlete enabled** setting and `tp config set --key enabled --value false`
control the same global pause.

## Environment overrides

| Variable | Effect |
| --- | --- |
| `TERMPLETE_HOME` | Use `<value>/termplete` for config, logs and command data |
| `XDG_CONFIG_HOME` | Use `<value>/termplete` when no TermPlete home override is set |
| `TERMPLETE_SOCKET` | Override the daemon socket path; useful for isolated development |
| `TERMPLETE_CAPTURE_OUTPUT=0` | Disable automatic capture in a newly launched shell |
| `TERMPLETE_NO_ANIM=1` | Disable animation |
| `NO_COLOR` | Disable color and motion |
| `TERM=dumb` | Use static output and numbered pickers |
| `TERM_PROGRAM` | Terminal-app detection used by exclusion settings |

Overrides affect the process and its child processes. Configuration paths are
reported by `tp status`. API-key environment variables are provider-specific;
`tp auth status` shows the selected credential source without printing the key.

## Animation

The animation layer lives in the daemon: every suggestion is rendered at the
frame index the shell sends, and the colour for that frame is computed there.

```sh
tp config set --key animations --value false
tp config set --key animation_frames --value 8
tp config set --key spinner --value false
```

Animations are disabled automatically by `NO_COLOR`, by a dumb terminal, and by
`TERMPLETE_NO_ANIM=1`.
Zsh displays inline ghost text through `POSTDISPLAY`, with a standard grey
foreground. The pending selector uses a gentle text shimmer; regular suggestions
stay still. Bash uses readline completion and an explicit preview.


## Architecture

```
   zsh / bash plugin
        │  background fetch, delivered via zle -F
        ▼
   tp suggest ──── unix socket ────▶ daemon
                                       │
                             context store (commands + output)
                             heuristic filter (mode gating)
                             fixed privacy filtering
                             optional decision helper → known command
                                       │ permitted abstention, or
                                       │ safe prompt-only Hybrid fallback
                                       ▼
                             configured command generator
```

The daemon exists so that a suggestion is a socket round-trip against warm
state, rather than re-reading the history file and re-opening a TLS connection
on every keystroke. It exits on its own when no shell has been attached for
`idle_shutdown_secs`.

- `src/auth.rs` — keychain storage
- `src/config.rs` — config and the provider table
- `src/context.rs` — bounded, redacted command history
- `src/memory.rs` — persistent command usage and indexed history retrieval
- `src/suggest.rs` — history matching, prompt assembly, reply sanitising
- `src/daemon.rs` — the server
- `src/provider.rs` — OpenAI-compatible and Anthropic clients
- `src/decision.rs` — unified decision contract and SystemOne/chat adapters
- `src/decision_runtime.rs` — warm helper, bounded cache and failure cooldown
- `src/privacy.rs` — deterministic outbound filtering and private-path rules
- `src/anim.rs` — the animation layer
- `src/pty.rs` — the `tp shell` PTY wrapper
- `shells/` — the zsh and bash integrations
