# Decision models and context privacy

TermPlete's decision helper is a constrained judge between local suggestions and the configured command generator. It can rank commands that already exist in TermPlete's candidate list and restrict what context may go to a generator. It cannot write a command, execute one, or override fixed privacy filters.

## Strategies and activation

The helper has a master switch and a strategy. It is active only when `decision.enabled` is true and `decision.strategy` is not `llm`.

| Strategy | Value | Request flow |
| --- | --- | --- |
| Standard AI | `llm` | Skip the decision helper and use the existing completion flow, subject to privacy filtering. This is the default. |
| Decision only | `decision` | Ask the helper to rank known local commands. Return a ranked command or local fallback. Never call the command generator. |
| Decision + AI | `hybrid` | Ask the helper to rank known commands. If it abstains, call the generator only when the helper permits remote generation and fixed filters permit the request. |

`decision.enabled` defaults to `true` so older configuration files keep their previous strategy behavior when the new field is absent. The default strategy is `llm`, so a fresh install does not make helper requests. Turning the master switch off preserves the chosen strategy, provider, and model.

In `tp settings`, open **AI provider → Decision helper**. **Use decision helper** is appended to that submenu. Turning it on when the saved strategy is Standard AI selects Hybrid; turning it off leaves all other helper settings intact. The privacy mode is under **Preferences → Sensitive context**.

The direct CLI control is:

~~~sh
tp config set --key decision.enabled --value false  # bypass helper
tp config set --key decision.enabled --value true   # enable saved strategy
~~~

To use the helper from the CLI, set a non-`llm` strategy. If it was previously disabled, turn it on as well.

## Request pipeline

~~~mermaid
flowchart TD
    A["Shell prefix or # task"] --> B["Local completions and bounded recorded context"]
    B --> C{"Typed text contains a hard-blocked secret or path?"}
    C -- yes --> L["Local result / fallback; no model request"]
    C -- no --> D["Hard filters: scrub secrets, remove credential and configured private paths"]
    D --> E{"Soft private metadata policy"}
    E -- "Block (default)" --> F["Remove soft hidden/private path metadata"]
    E -- "Review" --> G{"Helper active and loopback?"}
    G -- no --> F
    G -- yes --> H["Local judge sees hard-filtered context"]
    F --> I{"Helper active?"}
    H --> I
    I -- no --> J["Configured generator, if this request uses AI"]
    I -- yes --> K["Judge ranks candidate IDs and checks generator privacy"]
    K --> U{"Helper responded?"}
    U -- yes --> M{"Known command ranked?"}
    U -- no --> V{"Hybrid + Block + safe active text?"}
    V -- yes --> R
    V -- no --> L
    M -- yes --> N["Return local command"]
    M -- no --> O{"Strategy"}
    O -- "Decision only" --> L
    O -- "Hybrid" --> P{"allow_remote?"}
    P -- no --> L
    P -- yes --> Q{"omit_context?"}
    Q -- yes --> R["Generator gets the active prompt without remembered context"]
    Q -- no --> S["Generator gets the filtered request"]
    R --> T["Sanitized suggestions"]
    S --> T
    J --> T
~~~

The shell supplies the active line or task and the current directory. TermPlete can also include bounded recent commands, captured output, exit status, and a small snapshot of visible immediate directory entries. It does not open file contents or walk directories recursively. Hidden directory entries are always skipped by the filesystem snapshot; review mode only applies to path text that is already present in a captured command, output, or typed request.

For ordinary command completion, the helper can rank up to eight safe local candidates, including the latest exact resume command printed in retained same-directory output. Candidate metadata identifies context matches and their age alongside usage and acceptance evidence. A ranked result returns directly without a generator request. If no candidate fits, Hybrid can continue to generation after the privacy decision. Decision only returns local suggestions when it has no suitable candidate.

For a `# describe a task` request, the `#` text uses the same fixed privacy checks. Hybrid asks the helper for the privacy decision, then calls the generator when permitted. If the helper fails and the privacy policy is **Block**, a safe task still reaches the generator with only its active text; all remembered context is omitted. The helper ranks and checks privacy; the command LLM writes the shell command. It does not turn the task into an executable command itself. Editing or clearing the task cancels its pending request.

A fresh learned choice with a clear lead can return locally without a helper request. Directory completions are validated against the filesystem and ranked using the same decaying recency, bounded frequency and acceptance evidence as other commands. An active helper can reorder those valid candidates; it cannot invent a directory. Candidate evidence includes usage, last-use age, acceptance age and local score. Relevant recent terminal context and the active prefix take precedence over stale habits; an empty terminal context does not imply a navigation task unless the typed command asks for one. `--local-only` skips both the helper and command generator.

## Ranking and context authority

History commands and valid directory targets share one evidence score. Freshness decays smoothly as `2^(-age / half_life)`; lifetime counts saturate rather than growing without limit.

| Evidence | Contribution | Decay |
| --- | --- | --- |
| Execution frequency | `floor(650 × uses / (uses + 4))` | Bounded lifetime count |
| Last successful execution | Up to 2,000 | Three-day half-life |
| Acceptance frequency | `floor(600 × accepts / (accepts + 2))` × freshness | Two-day half-life |
| Last acceptance | Up to 2,600, plus at most 128 for prefix specificity | Two-day half-life |
| Current project match | Up to 2,800 | Seven-day half-life; frequency is freshness-weighted |
| Latest explicit context match | 9,000 plus up to 1,000 | Thirty-minute half-life for the additional bonus |

Ordinary history cannot exceed 5,976 points; even with the maximum project bonus it stays below the 9,000-point context floor. The latest explicit context match therefore stays ahead after its freshness bonus decays. Unknown execution dates remain unknown and receive only a small recency contribution; source order breaks ties. Unknown context dates receive the 9,000-point minimum. Age and confidence metadata remain visible to the judge.

**Preferences → Suggestion history** switches between **Current project folder** and **Global**. Folder mode scopes executions and accepted choices to the nearest Git root; outside Git it uses the current folder. Global history remains a fallback, including imported commands without a recorded working directory. Project evidence is bounded and decays, and candidate metadata exposes scope-match and freshness signals without sending the folder path to the judge.

A learned choice bypasses the model only with at least two acceptances, an acceptance within seven days, a score of at least 2,500, a lead of at least 300, confidence of at least 0.45, and no relevant terminal output. Acceptance means inserting a chosen suggestion with Right Arrow or Enter from the open selector; browsing with Tab does not count. Enter inserts the selected command; another Enter executes it.

Exact resume hints are validated against the source executable and canonical working directory. The newest retained hint for each executable/resume operation supplies the authoritative identifier; it does not expire after thirty minutes or disappear after eight unrelated commands. Superseded IDs are removed before limiting history candidates, and neither the judge nor generator can put an older ID back above the current hint. OMP's `-r` and `--resume` forms share this identifier while preserving the typed prefix. General output uses the prompt's eight-entry directory window, with explicit ages and sensitive lines excluded individually.

The latest same-directory action can supply a short-lived context match too.
Exit 127 allows a one-edit executable correction (including adjacent letter
transposition), preserving the original arguments and spacing. The corrected
executable must be available on PATH or known in command memory; compound
commands are left to generation. A successful single-target `mkdir` allows the
existing target to rank ahead of old `cd` or `pushd` choices, before the candidate
limit. These transitions expire after five minutes or the next command in that
directory, and never override a conflicting typed target. They use the same
context score and metadata as explicit hints. An immediate repair takes priority
over a retained resume task, and a model cannot replace the grounded first choice
with an unrelated historical target.

A paste containing one command, optionally preceded by comments, can provide
the same short-lived context evidence before execution. It is marked as pending
in stored context and model prompts, contributes no execution usage, and stays
unexecuted until the shell actually runs it. Cancelling the editor line retains
this evidence for retrying the intended command. Multicommand pastes are executed
normally by the shell but do not imply one unambiguous retry candidate. Pasted
comments and multiline shell input bypass `#` task generation.

The daemon rechecks context after a model request, so a capture received during that request can supersede its old snapshot. Native zsh capture waits for bounded publication acknowledgement before showing the next prompt; running command output still streams. Dirty bare-tool prefixes are retried once, while valid argument continuations reuse existing replies. Context is shared by tool and directory; parallel terminals in the same directory are not separate context scopes.

## Privacy rules

Privacy checks run before either model. Secret values are scrubbed from stored history and outbound requests. A typed command or task that matches a hard-block rule stays local instead of being sent to a judge or generator.

| Data | Standard handling | With **Review with a local decision model** |
| --- | --- | --- |
| Recognized credential values, private-key blocks, or credential-bearing typed prefixes | Scrub stored/outbound context; a sensitive active prefix stays local. The value is never sent to either model. | Same. Review cannot override the hard block. |
| Credential locations such as `.env`, `.ssh`, `.aws`, SSH key names, and key files | Hard-blocked before either model sees the path. | Same. |
| Paths listed in `decision.private_paths` | Hard-blocked with case-sensitive substring matching, including for the local judge. | Same. |
| Other soft hidden/private path text already recorded in commands or output, or present in a typed path | Blocked from both judge and generator by default (`privacy.sensitive_context = "block"`). | May be shown to an active loopback judge after hard filtering. A remote judge never receives unfiltered soft-private context. |
| Hidden entries from a fresh filesystem snapshot | Always skipped. | Always skipped. |

The optional local review is enabled with:

~~~toml
[privacy]
sensitive_context = "decision" # "block" (default) or "decision"

[decision]
enabled = true
strategy = "hybrid"             # llm, decision, or hybrid
provider = "ollama"
model = "tev1:0.8b"
local_only = true               # helper endpoint must be loopback
private_paths = ["/work/customer-private"]
~~~

A local judge can inspect soft path metadata only when the helper is active and the resolved helper endpoint is a loopback address. If the helper is disabled, configured as Standard AI, unavailable, or remote, the effective behavior is Block. The fixed filters still apply before the judge. This mode does not read files; it considers only text already captured in context or the current request.

The helper returns `allow_remote` and `omit_context` for Hybrid generation. `allow_remote = false` keeps the request local. `omit_context = true` drops remembered commands, output, and directory facts from the subsequent generator request. A remote generator receives only the fully filtered request; a local judge's access to soft private metadata does not itself authorize forwarding that metadata. These filters reduce accidental exposure but are not a guarantee that every sensitive string will be recognized.

## Decision response contract

All transports map to one validated result:

~~~json
{"allow_remote": true, "omit_context": false, "ranking": [1, 0, 2]}
~~~

- `allow_remote` is a boolean that controls whether Hybrid may use a remote generator.
- `omit_context` is a boolean that removes recorded context and directory metadata from the generator request while retaining the active prompt.
- `ranking` contains unique integer IDs from the supplied candidate list, best first. It cannot contain command text or an unknown ID. An empty ranking means abstention.

At most eight candidates are sent, and the judge receives user-authored context rather than the generator's system prompt. Inputs are bounded and truncated from the oldest context first; truncating a long active message keeps its newest tail.

### Chat JSON

OpenAI-compatible and Anthropic chat providers can return the contract directly. The parser rejects malformed JSON, missing or extra fields, repeated IDs, and IDs outside the supplied candidate list. `protocol = "chat"` forces this format. `protocol = "auto"` selects the protocol associated with the provider.

### SystemOne / JEV

For Ollama Tev, TypeSafe Jev, and compatible SystemOne endpoints, the native request uses named questions. With candidates, it asks three questions:

1. `share_remote`: a `noul` probability that the filtered content is safe for remote generation. It allows generation at probability **0.90 or higher**.
2. `omit_context`: a `noul` probability that context should be omitted. It omits context at probability **0.10 or higher**.
3. `best_command`: a choice among candidate IDs plus `none`. `none` means abstain; otherwise the chosen candidate ranks first and remaining candidates follow their returned probabilities.

A task with no local candidates asks only the two privacy questions. The parser validates the answer names and types, finite probabilities in `[0, 1]`, the exact expected probability keys, and a probability sum within 0.05 of 1.0. Invalid answers fall back to local suggestions and never trigger generation. Choose `protocol = "system-one"` for a compatible custom endpoint; `auto` uses the provider's declared wire format.

## Configure a local Tev judge

Install and start Ollama, and make sure the selected model is available. The check uses synthetic Git context only:

~~~sh
tp config set --key decision.provider --value ollama
tp config set --key decision.model --value tev1:0.8b
tp config set --key decision.strategy --value hybrid
tp config set --key decision.enabled --value true
tp decision check --timeout-ms 10000
# Machine-readable diagnostic:
tp decision check --timeout-ms 10000 --json
~~~

The check reports the provider, model, elapsed time, privacy decision, and selected synthetic candidates. It does not inspect command history or directory contents, and a one-off `--timeout-ms` value does not change the saved interactive budget. The default interactive budget is 1,500 ms; supported values are 100–10,000 ms. A cold model may need a longer diagnostic timeout.

The Ollama native client requests a five-minute keep-alive. The per-daemon helper caches up to 32 context/candidate decisions for three seconds, permits one helper request at a time, and uses a two-second failure cooldown. Invalid replies, unavailable models, timeouts, and a busy helper trigger a prompt-only generator fallback in Hybrid with the **Block** policy, provided the active text passes all deterministic privacy checks. This also applies during cooldown. The fallback drops history, captured output, current directory, file listings and learned preferences; AI history selection does not restrict that fresh generation to previously seen commands. A valid `allow_remote = false` still blocks generation. Decision only and the **Decision (local review)** privacy policy remain local on helper failure. Helper failure and prompt-only fallback are recorded in `daemon.log`; successful generation clears the suggestion error, while a generator failure reports the generator's error.

## Provider setup

The decision provider is independent from the command generator. A local judge can work with a remote generator; a remote judge is separately configured and disabled by default through `decision.local_only = true`. Remote helper endpoints require HTTPS, redirects are not followed, and loopback requests bypass HTTP proxies.

The built-in `jev` provider uses TypeSafe's `https://api.typesafe.ai/v1` endpoint and `jev-latest` model. Authenticate with `tp auth login --provider jev` or `TYPESAFE_API_KEY`. For a custom provider, configure `[[providers]]` and select its name under **Decision provider**. `wire = "jev"` selects native SystemOne. `wire = "openai-compat"` selects chat JSON, or override the decision protocol explicitly. A decision-only `base_url` override does not change the generator endpoint.

The available transports follow the [Ollama SystemOne API](https://docs.ollama.com/api/systemone) and [TypeSafe API](https://docs.typesafe.ai/api). Small local judges such as Tev are experimental: they can abstain or rank poorly, so local history, deterministic directory completion, and fallback behavior remain important.
