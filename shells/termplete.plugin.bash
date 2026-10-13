# Leave excluded terminal apps and their keybindings untouched.
if [[ $("${_TP_BIN:-tp}" config get --key terminal_enabled 2>/dev/null) == false ]]; then
  return 0
fi

if [[ -n ${HISTFILE:-} ]]; then
  export TERMPLETE_HISTFILE="$HISTFILE"
fi

# termplete — bash readline integration.
#
# Install by sourcing this from ~/.bashrc:
#     eval "$(tp init --print bash)"
#
# bash cannot do what the zsh plugin does. ZLE exposes a post-display message
# (`zle -M`) that renders ghost text without touching the buffer; readline has no
# equivalent, and there is no hook that fires "after a keystroke, now redraw the
# line". Attempting to fake it with a DEBUG trap is worse than useless: the trap
# fires before *every simple command*, so it would fire inside our own helpers
# and inside every command the user runs, adding latency and recursing.
#
# So bash gets the two mechanisms it does have, honestly:
#
#   * Default Tab completion via complete -D -F on Bash 4.1+.
#   * An inline preview on ctrl-t on Bash 4.0+, using readline's own
#     READLINE_LINE / READLINE_POINT so readline performs the rendering.
#     Accept is ctrl-y and only edits the buffer; it never runs anything.
#
# Context capture happens in PROMPT_COMMAND via `history 1`, because
# READLINE_LINE is unset by the time PROMPT_COMMAND runs.

_TP_BIN="${_TP_BIN:-tp}"
_TP_GHOST=""
_TP_COLOR=""
_TP_RESET=$'\e[0m'
_TP_DEBOUNCE_MS=220
_TP_TYPED_LEN=0
_TP_MODE="automatic"
_TP_REVEALED=0
_TP_PENDING=""
_TP_KEY_REVEAL="^G"
_TP_KEY_ACCEPT="^[[C"
_TP_KEY_CYCLE="^[[Z"
_TP_KEY_DISMISS="^C"
_TP_KEY_TOGGLE="\C-x\C-x"
_TP_ENABLED=1
_TP_GLOBAL_ENABLED=1
_TP_TIMEOUT_MS=400
_TP_TMP=$(mktemp -d "${TMPDIR:-/tmp}/termplete.XXXXXX" 2>/dev/null) || _TP_ENABLED=0
_TP_REQ_ID=0
_TP_LAST_REQ_MS=0
_TP_LAST_PROMPT_CWD=$PWD
_TP_LAST_HISTORY_ENTRY=$(HISTTIMEFORMAT='' builtin history 1 2>/dev/null)

_tp_load_settings() {
  local hex debounce mode_key legacy_key legacy_alt fallback
  [[ $("$_TP_BIN" config get --key terminal_enabled 2>/dev/null) == false ]] && _TP_GLOBAL_ENABLED=0 || _TP_GLOBAL_ENABLED=1
  hex=$("$_TP_BIN" config get --key ghost_color 2>/dev/null)
  if [[ $hex =~ ^\#?([0-9a-fA-F]{6})$ ]]; then
    _TP_COLOR=$'\e[38;2;'"${BASH_REMATCH[1]:0:2}"';'"${BASH_REMATCH[1]:2:2}"';'"${BASH_REMATCH[1]:4:2}"'m'
  else
    _TP_COLOR=$'\e[38;2;107;114;128m'
  fi
  _TP_MODE=$("$_TP_BIN" config get --key mode 2>/dev/null)
  case $_TP_MODE in
    automatic|aggressive|eager) _TP_MODE=automatic ;;
    on-demand|on_demand|subtle|normal) _TP_MODE=on-demand ;;
    off|none|disabled) _TP_MODE=off ;;
    *) _TP_MODE=automatic ;;
  esac
  case $_TP_MODE in
    automatic)
      mode_key=automatic_debounce_ms
      legacy_key=aggressive_debounce_ms
      legacy_alt=eager_debounce_ms
      fallback=400
      ;;
    *)
      mode_key=on_demand_debounce_ms
      legacy_key=subtle_debounce_ms
      legacy_alt=
      fallback=220
      ;;
  esac
  debounce=$("$_TP_BIN" config get --key "$mode_key" 2>/dev/null)
  [[ $debounce =~ ^[0-9]+$ ]] || debounce=$("$_TP_BIN" config get --key "$legacy_key" 2>/dev/null)
  if [[ ! $debounce =~ ^[0-9]+$ && -n $legacy_alt ]]; then
    debounce=$("$_TP_BIN" config get --key "$legacy_alt" 2>/dev/null)
  fi
  [[ $debounce =~ ^[0-9]+$ ]] || debounce=$fallback
  _TP_DEBOUNCE_MS=$debounce

  # Each key falls back to its default if unset, so a partial `[keys]` block in
  # the user's config still works.
  local k
  k=$("$_TP_BIN" config get --key keys.reveal 2>/dev/null); [[ -n $k ]] && _TP_KEY_REVEAL=$k
  k=$("$_TP_BIN" config get --key keys.accept 2>/dev/null); [[ -n $k ]] && _TP_KEY_ACCEPT=$k
  k=$("$_TP_BIN" config get --key keys.cycle 2>/dev/null); [[ -n $k ]] && _TP_KEY_CYCLE=$k
  k=$("$_TP_BIN" config get --key keys.dismiss 2>/dev/null); [[ -n $k ]] && _TP_KEY_DISMISS=$k
  k=$("$_TP_BIN" config get --key keys.toggle 2>/dev/null); [[ -n $k ]] && _TP_KEY_TOGGLE=$k
}

_tp_now_ms() {
  # Whole-nanosecond epoch is good enough for a ~220ms debounce.
  printf '%s' $(( $(date +%s%N) / 1000000 ))
}

_tp_load_settings
_TP_PROMPT_ENABLED=$_TP_GLOBAL_ENABLED
_TP_CONFIG_PATH=$("$_TP_BIN" config path 2>/dev/null)
_TP_CONFIG_SNAPSHOT=""
[[ -n $_TP_CONFIG_PATH && -r $_TP_CONFIG_PATH ]] && _TP_CONFIG_SNAPSHOT=$(<"$_TP_CONFIG_PATH")

_tp_refresh_settings() {
  [[ -n $_TP_CONFIG_PATH && -r $_TP_CONFIG_PATH ]] || return 0
  local snapshot
  snapshot=$(<"$_TP_CONFIG_PATH")
  [[ $snapshot == "$_TP_CONFIG_SNAPSHOT" ]] && return 0
  _TP_CONFIG_SNAPSHOT=$snapshot
  _tp_load_settings
  if (( ! _TP_GLOBAL_ENABLED )); then
    _TP_GHOST=""
    _TP_PENDING=""
  fi
}

# Strip any SGR sequences; used to keep the recorded command clean.
_tp_strip_ansi() {
  printf '%s' "$1" | sed -E 's/\x1b\[[0-9;]*[A-Za-z]//g'
}

# Fetch the current suggestion.
#
# No fifo here: bash cannot watch a descriptor from a prompt, and a fifo would
# need either a blocking open or a read-to-EOF that can hang. Instead the worker
# writes a plain temp file and renames it into place; the rename is atomic, so
# the file's existence means "complete". This cannot block and cannot race.
_tp_fetch() {
  local line=$1
  local cursor=${2:-${#1}}
  (( _TP_GLOBAL_ENABLED && _TP_ENABLED )) || return 1
  (( ${#line} >= 2 )) || return 1

  (( _TP_REQ_ID++ ))
  local data="$_TP_TMP/data.$_TP_REQ_ID"
  local done="$_TP_TMP/done.$_TP_REQ_ID"
  rm -f "$data" "$done" 2>/dev/null

  (
    "$_TP_BIN" suggest --plain --cursor "$cursor" -- "$line" >"$data" 2>/dev/null
    mv -f "$data" "$done" 2>/dev/null
  ) &
  disown 2>/dev/null

  # Poll for completion, but never indefinitely.
  local deadline=$(( $(_tp_now_ms) + _TP_TIMEOUT_MS ))
  while [[ ! -e $done ]] && (( $(_tp_now_ms) < deadline )); do
    sleep 0.02
  done

  local reply=""
  [[ -e $done ]] && reply=$(cat "$done" 2>/dev/null)
  rm -f "$done" "$data" 2>/dev/null
  [[ -n $reply ]] || return 1
  printf '%s' "$reply"
}

# Tab-completion source. bash passes the current word in $1 and the full line in
# $2; we ask the daemon for candidates and hand them back through COMPREPLY.
_tp_complete() {
  # The whole command line, not just the current word. Suggestions are full
  # commands ("git status --short"), so matching on COMP_WORDS[COMP_CWORD] alone
  # would search for a command called "st" and never find anything.
  local cur=$COMP_LINE
  COMPREPLY=()
  (( _TP_GLOBAL_ENABLED )) || return 0
  (( ${#cur} < 2 )) && return 0

  # `--candidates` prints one command per line, so no JSON parsing (and no jq)
  # is needed here.
  COMPREPLY=()
  local line
  while IFS= read -r line; do
    [[ -n $line ]] && COMPREPLY+=("$line")
  done < <("$_TP_BIN" suggest --candidates --cursor "${#cur}" -- "$cur" 2>/dev/null)
  return 0
}

# ctrl-t: preview the suggestion inline.
#
# Readline renders READLINE_LINE itself, so the buffer must stay pure text —
# embedding SGR escapes there would corrupt bash history and every subsequent
# column calculation. The preview therefore holds the plain completion and the
# cursor is parked at the end of the typed prefix, which is what produces the
# "greyed text to the right of the cursor" effect.
_tp_suggest_now() {
  local typed="${READLINE_LINE:0:$READLINE_POINT}"
  local ghost
  ghost=$(_tp_fetch "$typed") || return 0
  [[ -n $ghost ]] || return 0

  _TP_GHOST=$ghost
  _TP_TYPED_LEN=${#typed}
  READLINE_LINE="${typed}${ghost}"
  READLINE_POINT=${#typed}
  return 0
}

# Right Arrow accepts the preview. The preview text is already sitting at the end of
# READLINE_LINE, so accepting only has to drop the marker and move the cursor
# over it — appending again would duplicate the text. The command is not run.
_tp_accept() {
  (( _TP_GLOBAL_ENABLED && _TP_ENABLED )) || return 1
  [[ -n $_TP_GHOST ]] || return 1
  local prefix="${READLINE_LINE:0:$_TP_TYPED_LEN}"
  local command=$READLINE_LINE
  "$_TP_BIN" feedback --prefix "$prefix" --cwd "$PWD" -- "$command" >/dev/null 2>&1 &
  disown 2>/dev/null
  # Move the caret over the preview so it becomes ordinary text. Appending again
  # would duplicate it, since the preview already sits at the end of the line.
  READLINE_POINT=${#READLINE_LINE}
  _TP_GHOST=""
  return 0
}

_tp_accept_or_move_right() {
  if [[ -n $_TP_GHOST ]]; then
    _tp_accept
    return
  fi
  (( READLINE_POINT < ${#READLINE_LINE} )) && (( READLINE_POINT++ ))
  return 0
}

# Record the command that just ran. PROMPT_COMMAND runs with the line already
# submitted, so `history 1` is the only reliable source.
_tp_prompt_command() {
  local status=$?
  local was_enabled=$_TP_PROMPT_ENABLED
  _tp_refresh_settings
  local last history_entry
  last=$(HISTTIMEFORMAT='' builtin history 1 2>/dev/null)
  history_entry=$last
  # Strip the leading history-number column.
  last=${last#"${last%%[![:space:]]*}"}
  last=${last#* }
  if (( was_enabled && _TP_GLOBAL_ENABLED )) && [[ $history_entry != "$_TP_LAST_HISTORY_ENTRY" && -n ${last//[[:space:]]/} ]]; then
    "$_TP_BIN" record --cwd "$_TP_LAST_PROMPT_CWD" --exit-code "$status" -- "$last" >/dev/null 2>&1 &
    disown 2>/dev/null
  fi
  _TP_LAST_HISTORY_ENTRY=$history_entry
  _TP_LAST_PROMPT_CWD=$PWD
  _TP_PROMPT_ENABLED=$_TP_GLOBAL_ENABLED
  READLINE_LINE=""
  READLINE_POINT=0
  _TP_GHOST=""
  return $status
}

_tp_install() {
  if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 1) )); then
    complete -D -F _tp_complete -o bashdefault -o default 2>/dev/null
  fi
  if (( BASH_VERSINFO[0] >= 4 )); then
    bind -x '"\C-t": _tp_suggest_now' 2>/dev/null
    bind -x '"\C-y": _tp_accept' 2>/dev/null
    bind -x '"\e[C": _tp_accept_or_move_right' 2>/dev/null
  fi
}

_tp_install

# Preserve any existing PROMPT_COMMAND chain.
case "${PROMPT_COMMAND:-}" in
  *_tp_prompt_command*) ;;
  "") PROMPT_COMMAND="_tp_prompt_command" ;;
  *)  PROMPT_COMMAND="_tp_prompt_command;${PROMPT_COMMAND}" ;;
esac

export _TERMPLETE_BASH_LOADED=1
