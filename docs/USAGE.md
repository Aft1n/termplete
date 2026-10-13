# Using TermPlete

TermPlete adds command suggestions to your existing shell. It retrieves commands
from local history, learns which suggestions you accept, and can use a configured
LLM to complete commands or translate a `#` task description into command options.
An optional decision model ranks known commands and checks whether generation
should proceed. Selecting a suggestion edits your prompt; it does not run it.

This guide covers everyday use, configuration, privacy, troubleshooting and
removal. See the [README](../README.md) for the project overview,
[configuration reference](CONFIGURATION.md) for TOML and environment controls,
[decision-model guide](decision-models.md) for the helper contract, and
[release runbook](RELEASING.md) for distribution and publishing.

## Contents

- [Supported environments](#supported-environments)
- [Install and start](#install-and-start)
- [Complete and browse commands](#complete-and-browse-commands)
- [Generate a command from a task](#generate-a-command-from-a-task)
- [Choose modes and history behavior](#choose-modes-and-history-behavior)
- [Connect a provider](#connect-a-provider)
- [Configure the decision helper](#configure-the-decision-helper)
- [Terminal context and learning](#terminal-context-and-learning)
- [Privacy and stored data](#privacy-and-stored-data)
- [Settings and terminal appearance](#settings-and-terminal-appearance)
- [Troubleshooting](#troubleshooting)
- [Disable or uninstall](#disable-or-uninstall)

## Supported environments

| Environment | Integration |
| --- | --- |
| macOS and Linux with zsh 5.9+ | Inline suggestions, interactive selector, automatic output capture, and `#` tasks |
| macOS and Linux with Bash 4.1+ | Readline Tab completion, Ctrl+T suggestion preview, and command-history capture |
| Other shells | One-off CLI suggestions; no native integration is supplied |
| Native Windows | Not currently supported by the Unix-socket shell integration or release packages |

Older Bash versions can still record command history. Ctrl+T preview requires
Bash 4.0+, and default Tab completion requires Bash 4.1+. Stock macOS Bash 3.2
therefore provides history capture only.

The release workflow prepares Apple silicon and Intel macOS archives, plus ARM64
and x86-64 Linux GNU archives built against glibc 2.35. Source builds require
Rust 1.89 or newer. Terminal pickers honor `NO_COLOR` and use numbered prompts
when ANSI interaction is unavailable. Local test results do not establish that
every terminal, shell plugin combination or release architecture has been tested.

## Install and start

### Source installation

From a checkout of this repository:

```sh
cargo install --locked --path .
tp setup
```

Ensure Cargo's binary directory is on `PATH`. Both `termplete` and `tp` are
installed: `tp` is the short launcher and `termplete` is the application. Keep
them together when copying a build manually.

For a development build:

```sh
cargo build --locked --release
export PATH="$PWD/target/release:$PATH"
tp setup
```

Setup lets you choose a provider, model and masked API key, then installs the
integration in the current shell's startup file. Choose a model ID available
from your provider; model catalogs are not fixed by this guide. Open a new
terminal or run `exec zsh` after zsh setup.

### Release installation

After a tagged release passes CI and its release workflow completes, install
with Homebrew or the checksum-verifying curl installer:

```sh
brew tap aft1n/termplete https://github.com/aft1n/termplete
brew install aft1n/termplete/termplete
tp setup
```

```sh
curl -fsSL https://github.com/aft1n/termplete/releases/latest/download/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
tp setup
```

The [installer source](../scripts/install.sh) is readable in this repository.
To inspect the exact release copy before running it:

```sh
curl -fsSL https://github.com/aft1n/termplete/releases/latest/download/install.sh -o install.sh
less install.sh
sh install.sh
```

The explicit Homebrew tap URL is required because this repository is named
`termplete`, rather than `homebrew-termplete`.

The release workflow publishes native archives, `SHA256SUMS` and the installer,
then commits the generated formula to the repository's default branch. The npm
wrapper also needs its separate registry publication. Before the first release
is published, use the source-installation instructions above. See the
[release runbook](RELEASING.md) for release status and version-pinned installs.

### Manual shell integration

Add the relevant line after `tp` is available on `PATH`:

```sh
# ~/.zshrc
eval "$(tp init --print zsh)"
```

```sh
# ~/.bashrc
eval "$(tp init --print bash)"
```

Use the integration once in the startup file. Automatic zsh output capture uses
an internal PTY around the existing session; no separate TermPlete session
command is necessary. New sessions preserve their starting directory, and the
wrapper guards against recursively loading itself.

First check:

```sh
tp status
tp suggest --local-only --explain 'git l'
```

The local check needs neither an API key nor a model server. For Git, it can
offer `git log` and related built-in recipes. Model-backed results depend on
your provider, model and context.

## Complete and browse commands

In zsh, type a partial command such as `git l` or `docker`. Automatic mode
displays a suggested suffix. It leaves your typed text intact.

Recent actions take priority over older habits. After `mnkdir orch-test` fails
with command-not-found, typing `mkdir` suggests `mkdir orch-test`. After that
command succeeds, `cd ` favors `cd orch-test/`, provided the directory exists.
Typing another target overrides these inferences. They use the latest command
in the same directory and expire after five minutes or a newer command.
The model receives the failed command, status, and context evidence; these
simple transitions also work locally without a model.

Pasted scripts, including a comment followed by a command, use normal shell
execution when you press Enter. A pasted `#` comment does not trigger AI task
generation. If a paste contains one command alongside comments and you cancel
it, that command remains pending context for retry suggestions in the same
directory for up to five minutes. Pending input does not count as execution or
prove that an installation succeeded. Typing a different target overrides it.

| Key | Result |
| --- | --- |
| Ctrl+G | Reveal or retry a suggestion |
| Tab | Open the selector; move to the next option when already open |
| Shift+Tab | Move to the previous option |
| Up / Down | Navigate shell history; dismiss the selector |
| Right Arrow | Insert the selected suggestion into the prompt |
| Option/Alt+Right Arrow | Insert the next suggested word (zsh) |
| Esc | Close the selector |
| Ctrl+C | Dismiss or interrupt |
| Ctrl+X Ctrl+X | Toggle TermPlete |
| Enter | Accept the highlighted option when the selector is open; otherwise execute the prompt |

Up / Down retain ordinary shell history and multiline navigation, even when
the selector is open. Tab and Shift+Tab browse suggestions. While TermPlete is enabled, Tab belongs to its suggestions rather than falling through
to filename completion. Browsing returned alternatives does not make another
AI request. A single model response can supply multiple options.

Enter and Right Arrow perform the same acceptance action in the selector:
they insert the selected command and close the menu. Press Enter again to run
it. An open selector that is loading or empty does not submit a partial command;
Esc closes it and restores ordinary Enter behavior.

Option/Alt+Right accepts the next word using zsh's native word boundaries and keeps
the remaining suggestion visible. Repeat it to proceed word by word, or type
your own arguments. Partial acceptance closes the selector so Enter runs only
the text already inserted; it does not silently fill the remaining ghost text.
Only accepting the whole suggestion records a learned preference. Without a
visible suggestion, Option/Alt+Right keeps its original cursor-movement binding.

Backspace dismisses the suggestion and selector before deleting the character.
Clearing the line removes counters, hints and all TermPlete highlights. Replies
to an older line cannot restore a dismissed suggestion. Completion continues
after spaces; arguments can be suggested for a completed tool name.

Directory completion uses actual folders in the requesting shell's working
directory, ranked with the same freshness, frequency and acceptance evidence as
other commands. Ranking happens before the suggestion limit. If `nuxt-local-model/` exists,
`cd nuxt-lo` can complete to that name. An unmatched directory prefix does not
receive an invented path. Built-in Docker and ffmpeg recipes provide local
options when AI is unavailable; filenames in recipes are placeholders until
replaced with your own values.

## Generate a command from a task

In zsh, enter:

```text
# change dog.mp4 into dog.mov with ffmpeg
```

Press Enter. The description stays in the prompt while TermPlete requests
command options. A possible result is:

```sh
ffmpeg -i dog.mp4 dog.mov
```

Tab or Shift+Tab browses the returned commands. Enter or Right Arrow replaces the description
with the selected command. Review it and press Enter separately to execute it.
TermPlete does not execute model output automatically. Editing the description
cancels its request; a bare `#` or an empty line sends no task request.

The equivalent one-off CLI request is:

```sh
tp suggest --intent --json -- 'change dog.mp4 into dog.mov with ffmpeg'
```

Standard AI uses the configured command generator. Decision + AI checks with
the helper before generation and has the bounded fallback described below.
Decision only can select related known commands but cannot create a new command
for an unfamiliar task. A small decision model is not the command generator.

## Choose modes and history behavior

These settings answer different questions:

| Setting | Options | Purpose |
| --- | --- | --- |
| Visibility mode | Automatic / On demand | Show suggestions while typing, or wait for Ctrl+G/Tab |
| History matches | Instant / AI chooses | Return ranked local matches immediately, or ask AI to select a known match |
| Decision strategy | Standard AI / Decision only / Decision + AI | Decide which model stages participate |

Automatic, Instant history and Standard AI are the defaults. Automatic does not
mean every keystroke makes a network request: local recipes, directory matches
and trusted learned choices can answer directly. Model requests are debounced.

```sh
tp config set --key mode --value on-demand
tp config set --key ai_select_history --value true
```

With AI history selection, the generator chooses an existing matching command
unless fresh relevant terminal output supplies context for a new continuation.
An invalid history choice falls back to local history. A fresh accepted choice
with a clear lead may answer locally when no relevant output needs review.
An active decision helper can rank the available options with their evidence.

Use `tp settings` for readable pickers. After changing shell behavior, opening
a new session is the simplest way to ensure it is loaded; saved shortcuts and
visibility modes also reload at a subsequent zsh prompt.

## Connect a provider

```sh
tp setup
tp auth providers
tp auth status
```

Switching providers remembers the last selected model for each provider and keeps
its API key in the system credential store. Switching back restores that model
and reuses the saved key. This applies to Settings, setup and CLI provider changes;
keys are replaced or removed only through the explicit key actions.

Select a generation provider and a model served by that provider. Hosted
generation uses OpenAI-compatible chat completions or native Anthropic Messages.
Ollama and LM Studio presets are local and require no API key. The Jev preset is
for the decision helper, not ordinary command generation.

Termplete automatically requests no reasoning for suggestions, using Low (or
the model's lower supported level) when reasoning cannot be disabled. This
applies to ChatGPT, hosted APIs and local models without adding a Settings
option. Ollama and LM Studio model capabilities are cached after discovery.
Custom endpoints and older local servers that expose no reasoning controls
retain their native behavior; a model with only higher reasoning levels may
require choosing a different model.

To use a ChatGPT subscription, sign in with the ChatGPT account in your
browser:

```sh
tp auth login --provider openai-acp --use-provider
```

You can also select `openai-acp` in `tp setup`. On first sign-in, Termplete
installs the Codex CLI into its user cache with npm; a
global install is not required. Node.js/npm and internet access are needed for
that first install. An existing `codex` on `PATH` is reused.
Termplete opens Codex's browser sign-in flow; Codex manages the credentials.
Completions use Codex app-server's ephemeral, read-only threads, with a fresh
thread in a temporary directory for each request. Termplete checks the server's
ephemeral confirmation before sending terminal context. Suggestions are not
saved as Codex chats, and the desktop app does not need to be running.
Inherited apps, plugins, MCP servers, lifecycle hooks and notification commands
are disabled. If the helper cannot confirm isolation, it receives no terminal
context; update Codex and retry.
The provider ID `openai-acp` remains compatible with existing configs.
In **Settings → AI provider**, choose **ChatGPT subscription**, then select
**Sign in to ChatGPT**.
Use `tp auth remove --provider openai-acp` to sign out.

For a local generation model already installed in Ollama:

```sh
tp config set --key provider --value ollama
tp config set --key model --value YOUR_INSTALLED_CHAT_MODEL
```

Replace the uppercase placeholder with the exact model name. A decision model
such as Tev is configured separately under `decision.model`.

### Custom OpenAI-compatible endpoints

Open **Settings → AI provider → Provider → Custom OpenAI-compatible provider**.
Enter a unique name, base URL, model and API key. Local endpoints need no key.
The custom provider draft and its key are stored when you choose **Save and exit**;
cancelling discards the draft.

An equivalent configuration is:

```toml
provider = "my-gateway"
model = "your-model-id"

[[providers]]
name = "my-gateway"
base_url = "https://example.com/v1"
wire = "openai-compat"
env_key = "MY_GATEWAY_API_KEY"
```

Use your actual endpoint and model. A base URL is the API root, not the full
`/chat/completions` URL. Local unauthenticated custom servers can set
`no_auth = true`. Do not put an API key into a URL or config file.

### Replace or remove a key

API keys are stored under the full `termplete` service name in the platform
credential store. Prompts mask input and acknowledge receipt without printing
the key. macOS access requests come from the `termplete` executable.

```sh
tp auth login --provider openai --force
tp auth remove --provider openai
```

The login prompt accepts a replacement key. Removal deletes that provider's
stored entry. Both operations stop an existing daemon so it cannot continue
using the old cached credential; normal autostart can start it again on demand.
Settings also offers replacement and removal. For an existing saved provider,
these keychain actions take effect immediately; backing out of configuration
edits does not undo them. Unsaved custom-provider keys remain drafts until saved.

Resolution order is an explicit key, a stored key, then the provider's `env_key`.
An environment fallback can still authenticate after the stored entry is removed.
Local no-auth providers bypass the credential store. If the store is unavailable,
an environment key can be used; if the store is awaiting an OS access dialog,
complete that dialog or unlock the store before retrying.

## Configure the decision helper

Open **Settings → AI provider → Decision helper**. The generator and helper have
separate provider, endpoint and model settings.

| Strategy | Behavior |
| --- | --- |
| Standard AI (`llm`) | Local completion plus the ordinary generator; helper inactive |
| Decision only (`decision`) | Helper ranks known local commands; no generator call |
| Decision + AI (`hybrid`) | Helper can select a known command or permit the generator to write a new one |

For Ollama with `tev1:0.8b` installed and the local server running:

```sh
tp config set --key decision.provider --value ollama
tp config set --key decision.model --value tev1:0.8b
tp config set --key decision.protocol --value auto
tp config set --key decision.strategy --value hybrid
tp config set --key decision.enabled --value true
tp decision check --timeout-ms 10000 --json
```

The check uses synthetic Git context, not your command history. Its one-off
timeout does not change the interactive budget. Native Tev/Jev use SystemOne;
ordinary chat models use the Chat JSON contract. When using an ordinary chat
model through Ollama, select `decision.protocol = "chat"`.

The helper validates privacy flags and candidate IDs; it cannot return arbitrary
executable command text as a ranked choice. Small models can rank poorly or deny
safe requests, so a successful connection is not evidence of good judgment.

### Slow or unavailable helper

The default interactive helper budget is 1,500 ms. Cold local models may take
longer. The daemon maintains a short decision cache, one helper request at a
time, and a two-second failure cooldown.

With **Decision + AI** and the default **Block** privacy policy, a timeout, busy
helper, malformed reply or cooldown does not disable generation. If the active
text passes the deterministic privacy checks, the generator receives only that
text. History, output, directory metadata and learned preferences are omitted.
This applies to ordinary completion and `#` task requests. A generator failure
then reports the generator's error, rather than blaming the helper.

A valid privacy denial still blocks generation. Decision only stays local on
helper failure. **Review with a local decision model** also requires a successful
review and does not fall through when the helper fails.

To bypass the helper without losing its settings:

```sh
tp config set --key decision.enabled --value false
```

Remote helpers require HTTPS and explicit disabling of the local-only setting.
Local review of soft private metadata requires an active loopback helper. The
[decision-model guide](decision-models.md) documents protocols, thresholds and
the complete privacy policy.

## Terminal context and learning

TermPlete can supply bounded recent commands, captured output, exit statuses,
the requesting directory and immediate visible file/folder names to the model.
It does not open file contents or recursively scan your project. Hidden entries
are skipped. Directory facts come from the requesting shell, rather than a
different terminal's working directory.

This lets the generator interpret sequences such as a listing followed by
navigation, or a failed command followed by an attempted repair. Context is
evidence for a suggestion, not permission to execute instructions found in output.

Fresh output from the same tool and directory reaches the model even when
history already contains a matching command. For example, typing `omp` after
a session prints its resume command lets the model reuse the exact session ID.
Explicit `Resume this session with` hints also provide an immediate local
candidate for 30 minutes. In on-demand mode, revealing the suggestion requests
the contextual model refinement.

### Output capture

**Preferences → Capture terminal output** toggles collection in new zsh sessions.
It captures output from commands in the integrated session and displays that
output immediately while retaining a bounded, scrubbed tail for context.

```sh
tp config set --key capture_output --value false
```

Open a new terminal after changing capture. Disabling collection does not erase
previously saved output. `TERMPLETE_CAPTURE_OUTPUT=0` is a per-process override
when launching a new shell. Bash does not supply the same automatic observer.

### History retrieval and learning

TermPlete imports native zsh/Bash history on daemon startup and maintains local
command memory. Prefix matching and lexical retrieval reuse known targets such
as a usual SSH host. No embedding model, vector service or external database is
needed. Imported commands without an observed exit status are treated as unknown.
History without execution timestamps keeps its file order and receives weaker
freshness evidence; changing the history file does not make every old command
recent. Live executions and timestamped native history retain actual recency.

Enter in the selector and Right Arrow acceptance record a preference for that
prefix and command. Browsing or dismissal is not acceptance. Freshness decays
gradually, lifetime frequency has a bounded influence, and old acceptance
bonuses fade. Recent successful execution can overtake an older accepted choice.
Repeated fresh choices with a clear ranking lead become trusted local answers;
failures suppress the preference until another acceptance
or successful execution restores it. Memory is bounded, including at most
128 learned prefixes and eight choices per prefix.

**Preferences → Suggestion history** selects **Current project folder** or
**Global**. Folder mode groups executions and acceptances by the nearest Git
root, or by the current folder outside Git. It applies a bounded, decaying local
bonus while keeping global history as fallback; imported commands without a
recorded working directory remain global. The decision helper receives numeric
scope and freshness evidence, not the folder path. Choose Global to share ranking
across all folders.

Directory history compares resolved destinations, so `~/developer/project`,
its absolute spelling and a trailing slash share evidence. Relative destinations
use the directory recorded before the command executes. Suggestions retain the
prefix and spelling the user typed. The optional decision helper receives
usage, freshness, acceptance and scope evidence alongside relevant terminal context,
and can reorder valid local directories without inventing paths.

```sh
tp context --search 'ssh '
tp context --import ~/.zsh_history
tp config set --key ai_select_history --value false
```

Multiple shells share the daemon. Clearing a visible suggestion does not clear
learned command memory.

## Privacy and stored data

The default directory is `~/.termplete`. `TERMPLETE_HOME` selects
`<TERMPLETE_HOME>/termplete`; `XDG_CONFIG_HOME` selects
`<XDG_CONFIG_HOME>/termplete` when no TermPlete override is set. `tp status`
prints the actual paths. Legacy platform configuration migrates only when the
new default directory does not already exist.

| Item | Contents |
| --- | --- |
| `config.toml` | Provider/model selection, modes, shortcuts, context and privacy preferences |
| `history.json` | Bounded recent command context and scrubbed captured output |
| `commands.json` | Retrieved command memory, global and project-scoped usage and accepted-choice statistics |
| `daemon.log` | Operational diagnostics; not a complete terminal recording |
| OS credential store | Provider API keys; separate from these files |

Configuration and socket directories are checked for ownership and use private
permissions. The daemon socket lives in a per-user temporary directory by default;
`TERMPLETE_SOCKET` can override it for isolated development.

Known secrets are scrubbed before storage or outbound use. Credential paths such
as `.env`, `.ssh`, `.aws`, key files and configured private paths are hard-blocked
from model requests. Commands with recognized secret-bearing text are excluded
from suggestion memory. Ignore prefixes can prevent recording selected commands.

**Block** removes other soft hidden/private path metadata from both models.
**Local review** lets only an active loopback judge inspect already-recorded soft
metadata after hard filtering. A remote generator still receives fully filtered
context; a positive judge result does not authorize sending excluded data.

Redaction is a best-effort defense, not a guarantee that every sensitive value
will be recognized. Review provider data-handling policies and use local models
or local-only completion when you do not want context sent to a remote service.

```sh
tp suggest --local-only --json -- 'git l'
tp config set --key decision.private_paths --value /work/customer-private
```

To remove saved command data, stop the daemon first so it cannot write memory
back at shutdown. Disable native imports if you do not want shell history
re-imported. Use the actual directory printed by `tp status`; for the default:

```sh
tp config set --key import_shell_history --value false
tp service stop
rm ~/.termplete/history.json ~/.termplete/commands.json
```

Only run the deletion if you intend to discard those records. Removing files
does not remove your native shell history or OS keychain entries.

## Settings and terminal appearance

`tp settings` groups mode, provider, shortcuts, appearance and preferences.
Use Up/Down or j/k to navigate, Enter to select, and Esc/q to go back. Choose
**Save and exit** to persist configuration; leaving the main menu otherwise
discards configuration edits. Existing-provider key actions are immediate, as
described above.

**Keyboard shortcuts** records the bytes your terminal actually sends and shows
readable names. Some terminals report Ctrl+Shift combinations as the same key
without Shift; others use enhanced encodings. Use **Reset shortcuts to defaults**
and save to discard custom bindings.

**Appearance** controls animations, ghost and accent colors. Selected options
use high-contrast cyan with bold black text; unselected options use the terminal's
normal foreground. Pending text has a bounded shimmer, and selection changes
briefly fade. `NO_COLOR`, `TERM=dumb`, `TERMPLETE_NO_ANIM=1` and disabled animations
produce static output.

To keep another terminal's autocomplete, exclude that app:

```sh
tp config set --key disabled_terminals --value warp
```

The **Preferences → Terminal apps** picker can edit exclusions individually.
The default list is empty. Matching uses terminal detection and names are case
insensitive; WarpTerminal also matches Warp. Restart an existing integrated shell
to restore its original hooks and keys after excluding the app.

## Troubleshooting

Start with a local result, then check the configured model path:

```sh
tp status
tp suggest --local-only --explain -- 'git l'
tp suggest --explain -- 'docker '
tp suggest --intent --json -- 'change dog.mp4 into dog.mov with ffmpeg'
```

| Symptom | What to check |
| --- | --- |
| No suggestions anywhere | Integration installed once, `tp` on PATH, terminal not excluded, TermPlete toggle enabled, nonempty input |
| Local suggestions work, AI does not | Selected model, endpoint, provider key, `tp status`, one-off `--explain` result |
| API key missing or rejected | Stored key versus environment fallback, model access, keychain unlocked; replace the key with masked login if necessary |
| Keychain request appears to hang | Complete the OS access prompt; rebuilding the executable can require fresh macOS authorization |
| Helper exceeds its budget | Run `tp decision check --timeout-ms 10000`; Hybrid + Block falls back with active text only |
| `#` task has no result in Decision only | That strategy only ranks existing commands; choose Standard AI or Decision + AI to generate new ones |
| `#` task has no result despite a healthy helper | Inspect a privacy denial, required local review, and the generator's model/auth diagnostic |
| Ctrl+Shift shortcut inserts escape text | The terminal encoding may differ; recapture it in settings or reset to defaults |
| Tab browses files instead of suggestions | Check whether the plugin loaded, is toggled off, or the terminal is excluded |
| Fresh zsh has a blank prompt | Try a new session after updating, check duplicate integration lines and other startup scripts; do not repeatedly wrap an already wrapped session |
| Output capture setting appears unchanged | It applies to new sessions; saved output remains until explicitly removed |

Find the log path in `tp status`. Review and sanitize diagnostic output before
sharing it: hostnames, directory names and commands can still be personal.
For a foreground development daemon, stop the background daemon first:

```sh
tp service stop
tp daemon --foreground
```

The foreground command stays running. Use another terminal for CLI requests.
Stop it before starting another daemon on the same socket.

## Disable or uninstall

Use `tp stop` to pause the shell integration and `tp start` to resume it. The
change takes effect at the next prompt in shells that already loaded TermPlete;
at that prompt, pending suggestions and ghost text are cleared, and TermPlete
stops its command-history and output capture. The local Ctrl+X Ctrl+X toggle
remains independent in each shell. The same global setting is available in
Preferences or through `tp config set --key enabled --value false`.

A shell opened while TermPlete is stopped does not load its hooks. After
`tp start`, open a new shell to enable integration there. Commenting out or
deleting the integration line in `.zshrc` or `.bashrc` affects new shells only.
Existing integrated sessions may have a `termplete shell` parent forwarding
terminal input and output. Close the terminal tab after saving and exiting its
work; re-sourcing a startup file or running `exec zsh` does not remove that
parent process.

`tp service stop` controls only the background daemon. It does not pause shell
integration, and an enabled shell can start the daemon again on its next
request. After closing integrated sessions, stop any remaining daemon with:

```sh
tp service stop
```

Before deleting the binaries, remove any stored provider keys you want discarded
with `tp auth remove --provider NAME`. Remove environment keys separately from
your environment or startup configuration.

Uninstall with the manager that installed it (`cargo uninstall termplete`,
`npm uninstall --global termplete`, or `brew uninstall termplete` after those
distributions are available). For a manual install, remove both binaries from
the chosen installation directory. Configuration and learned data are retained;
delete the directory printed by `tp status` only if you want them gone.
