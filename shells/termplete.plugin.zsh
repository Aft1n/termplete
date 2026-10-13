# Leave excluded terminal apps and their keybindings untouched.
if [[ $("${_TP_BIN:-tp}" config get --key terminal_enabled 2>/dev/null) == false ]]; then
  return 0
fi

# ZLE ownership markers are preserved only by zsh 5.9 and newer.
autoload -Uz is-at-least
if ! is-at-least 5.9; then
  print -u2 -- 'TermPlete requires zsh 5.9 or newer; upgrade zsh to enable suggestions.'
  return 0
fi

# Wrapper presence is independent from capture readiness: capture stays off
# while the wrapped child sources the user's startup files.
if [[ -o interactive && -t 0 && -t 1 &&
      ${TERMPLETE_CAPTURE_WRAPPED:-0} != 1 &&
      ${TERMPLETE_CAPTURE_ACTIVE:-0} != 1 &&
      ${TERMPLETE_CAPTURE_OUTPUT:-1} != 0 &&
      $("${_TP_BIN:-tp}" config get --key capture_output 2>/dev/null) == true ]]; then
  exec "${_TP_BIN:-tp}" shell --shell "${commands[zsh]:-/bin/zsh}"
fi

# termplete — zsh ZLE integration.
#
# Install by sourcing this from ~/.zshrc:
#     eval "$(tp init --print zsh)"
#
# Ctrl+G reveals a suggestion. Tab opens and cycles a selector; Right Arrow
# inserts the selected command; Option/Alt+Right inserts one word. Automatic mode shows ghost text as you type;
# On demand waits for a reveal. Tab stays within the suggestion selector.
#
# POSTDISPLAY lets ZLE draw inline ghost text without changing BUFFER. Only
# redraw when the display or its colors change. Empty input clears all owned
# UI; animation is limited to pending requests and short selection transitions.
#
# Concurrency
# -----------
# The model round-trip never runs on the keystroke path. Each fetch is detached
# and announces completion through a fifo watched by `zle -F`. The fifo is
# opened read-write and read exactly one line, never to EOF: read-write means
# this shell holds the write end open forever, so EOF never arrives and a `cat`
# would hang the editor indefinitely.

typeset -g _TP_BIN="${_TP_BIN:-tp}"
# Keep cleanup inside ZLE so Ctrl+C cannot be consumed by an rm child.
zmodload -F zsh/files b:zf_rm 2>/dev/null || return 0
if [[ -n ${HISTFILE:-} ]]; then
  export TERMPLETE_HISTFILE="$HISTFILE"
fi
typeset -gi _TP_REQ_ID=0        # id of the most recently issued request
typeset -gi _TP_FRAME=0         # animation frame, advanced per keystroke
typeset -gi _TP_ENABLED=1
typeset -gi _TP_GLOBAL_ENABLED=1 # config pause, independent from the local key toggle
typeset -gi _TP_DIRTY=0         # a keystroke landed while a request was in flight
typeset -g  _TP_FD=""           # fd of the in-flight reply fifo
typeset -g  _TP_GLOW_FD=""      # timer only while a visible request is pending
typeset -gi _TP_GLOW_PHASE=0
typeset -g  _TP_ANIMATIONS=true
typeset -gi _TP_PULSE=0
typeset -gF _TP_PULSE_UNTIL=0
typeset -gi _TP_EMPTY_CLEARED=0
typeset -gi _TP_FRAME_OPEN=0
typeset -gi _TP_COMMAND_ENABLED=0
typeset -gi _TP_SETTINGS_CHANGED=0 _TP_CLEAR_ON_LINE_INIT=0
typeset -g _TP_MUTED_COLOR=8 _TP_ACCENT_COLOR=cyan _TP_SELECTED_INK=black
typeset -g _TP_SELECTION_STYLE='fg=black,bg=cyan,bold'
typeset -ga _TP_FADE_COLORS=(cyan blue 8 8)
typeset -g _TP_REVEAL_LABEL="Ctrl+G"
typeset -ga _TP_HIGHLIGHTS=()
typeset -gA _TP_TAB_WIDGET
typeset -gA _TP_ESCAPE_WIDGET
typeset -gA _TP_ARROW_WIDGET
typeset -gA _TP_BINDINGS        # original widget per map/key, restored on reload
typeset -g _TP_CONFIG_PATH=""
typeset -g _TP_CONFIG_SNAPSHOT=""
typeset -g  _TP_GHOST=""        # ghost text currently displayed
typeset -ga _TP_CANDIDATES=()
typeset -ga _TP_ALL_CANDIDATES=()
typeset -gi _TP_CANDIDATE_INDEX=1
typeset -gi _TP_SELECTION_EXPLICIT=0
typeset -g  _TP_PENDING=""      # suggestion held back, awaiting a reveal
typeset -g  _TP_REVEALED=0      # 1 once the user has pressed reveal
typeset -gi _TP_MENU_OPEN=0
typeset -gi _TP_PASTED=0
typeset -g _TP_LAST_COMMAND="" _TP_LAST_COMMAND_CWD=""
typeset -g  _TP_REQUEST_BUFFER=""
typeset -g  _TP_FULL=""         # full suggestion the ghost is a tail of
typeset -g _TP_ISSUE="" _TP_INTENT_BUFFER=""
typeset -g _TP_AI_BUFFER="" _TP_CONTEXT_BUFFER=""
typeset -g  _TP_MODE="automatic"
typeset -g  _TP_DECISION_STRATEGY="llm"
typeset -g  _TP_TMP=""
typeset -gi _TP_TEMP_READY=0
if _TP_TMP=$(mktemp -d "${TMPDIR:-/tmp}/termplete.XXXXXX" 2>/dev/null); then
  _TP_TEMP_READY=1
else
  _TP_ENABLED=0
fi
typeset -gi _TP_DEBOUNCE_MS=220
typeset -g  _TP_KEY_REVEAL="^G"
typeset -g  _TP_KEY_ACCEPT="^[[C"
typeset -g  _TP_KEY_CYCLE="^[[Z"
typeset -g  _TP_KEY_DISMISS="^C"
typeset -g  _TP_KEY_TOGGLE="^X^X"

typeset -gA _TP_INFLIGHT        # fd -> request id

# Read settings at load and after the config changes, outside the keystroke path.
_tp_load_settings() {
  _TP_SELECTION_STYLE='fg=black,bg=cyan,bold'
  [[ $("$_TP_BIN" config get --key terminal_enabled 2>/dev/null) == false ]] && _TP_GLOBAL_ENABLED=0 || _TP_GLOBAL_ENABLED=1
  _TP_MODE=$("$_TP_BIN" config get --key mode 2>/dev/null) || _TP_MODE=automatic
  [[ -n $_TP_MODE ]] || _TP_MODE=automatic
  local debounce key=on_demand_debounce_ms
  [[ $_TP_MODE == automatic || $_TP_MODE == aggressive || $_TP_MODE == eager ]] && key=automatic_debounce_ms
  debounce=$("$_TP_BIN" config get --key "$key" 2>/dev/null)
  [[ $debounce == <-> ]] || debounce=220
  _TP_DEBOUNCE_MS=$debounce
  _TP_AI_HISTORY=$("$_TP_BIN" config get --key ai_select_history 2>/dev/null)
  _TP_DECISION_STRATEGY=$("$_TP_BIN" config get --key decision.strategy 2>/dev/null)
  [[ -n $_TP_DECISION_STRATEGY ]] || _TP_DECISION_STRATEGY=llm
  [[ $("$_TP_BIN" config get --key decision.active 2>/dev/null) == true ]] || _TP_DECISION_STRATEGY=llm
  _TP_ANIMATIONS=$("$_TP_BIN" config get --key animations 2>/dev/null)
  zmodload zsh/terminfo 2>/dev/null
  # Extended palette entries have fixed RGB values, unlike a terminal theme's
  # ANSI black/cyan slots. Pure black on bright cyan stays readable.
  (( ${terminfo[colors]:-0} >= 256 )) && _TP_SELECTION_STYLE='fg=16,bg=51,bold'
  local -a palette
  palette=( ${=$("$_TP_BIN" config get --key zle_palette 2>/dev/null)} )
  _TP_MUTED_COLOR=8 _TP_ACCENT_COLOR=cyan _TP_SELECTED_INK=black
  _TP_FADE_COLORS=(cyan blue 8 8)
  if (( ${terminfo[colors]:-0} >= 256 && ${#palette} == 7 )); then
    local valid=1 value
    for value in "${palette[@]}"; do
      [[ $value == <-> ]] && (( value <= 255 )) || valid=0
    done
    if (( valid )); then
      _TP_MUTED_COLOR=$palette[1]
      _TP_ACCENT_COLOR=$palette[2]
      _TP_SELECTED_INK=$palette[3]
      _TP_FADE_COLORS=("${palette[@]:3}")
    fi
  fi

  # Each key falls back to its default if unset, so a partial `[keys]` block in
  # the user's config still works.
  local k
  k=$($_TP_BIN config get --key keys.reveal 2>/dev/null); [[ -n $k ]] && _TP_KEY_REVEAL=$k
  k=$($_TP_BIN config get --key keys.accept 2>/dev/null); [[ -n $k ]] && _TP_KEY_ACCEPT=$k
  k=$($_TP_BIN config get --key keys.cycle 2>/dev/null); [[ -n $k ]] && _TP_KEY_CYCLE=$k
  k=$($_TP_BIN config get --key keys.dismiss 2>/dev/null); [[ -n $k ]] && _TP_KEY_DISMISS=$k
  k=$($_TP_BIN config get --key keys.toggle 2>/dev/null); [[ -n $k ]] && _TP_KEY_TOGGLE=$k
  _TP_REVEAL_LABEL=$("$_TP_BIN" config get --key keys.reveal_label 2>/dev/null)
  [[ -n $_TP_REVEAL_LABEL ]] || _TP_REVEAL_LABEL="Ctrl+G"
}

_tp_load_settings
zmodload zsh/datetime 2>/dev/null
_TP_CONFIG_PATH=$("$_TP_BIN" config path 2>/dev/null)
zmodload zsh/mapfile 2>/dev/null
[[ -n $_TP_CONFIG_PATH ]] && _TP_CONFIG_SNAPSHOT=${mapfile[$_TP_CONFIG_PATH]}

# A settings command returns to a fresh prompt. Compare the config using a zsh
# builtin, so unchanged prompts do not spawn any configuration subprocesses.
_tp_refresh_settings() {
  [[ -n $_TP_CONFIG_PATH && -r $_TP_CONFIG_PATH ]] || return 0
  local snapshot=${mapfile[$_TP_CONFIG_PATH]}
  [[ $snapshot == "$_TP_CONFIG_SNAPSHOT" ]] && return 0
  _TP_CONFIG_SNAPSHOT=$snapshot
  _tp_load_settings
  _TP_SETTINGS_CHANGED=1
  (( _TP_GLOBAL_ENABLED )) || _TP_CLEAR_ON_LINE_INIT=1
}

_tp_next_frame() {
  (( _TP_FRAME = (_TP_FRAME + 1) % 12 ))
}

_tp_start_pulse() {
  _TP_PULSE=4
  (( _TP_PULSE_UNTIL = ${EPOCHREALTIME:-0} + 0.32 ))
}

# Should the ghost be shown without the user asking?
#
# Automatic always shows it. On demand holds it back so the line stays quiet until
# the user asks for a suggestion with one keypress.
_tp_auto_show() {
  [[ $_TP_MODE == automatic || $_TP_MODE == aggressive || $_TP_MODE == eager ]]
}

_tp_mkfifo() {
  setopt localoptions localtraps
  local -i result=0
  # A foreground child can consume Ctrl+C before ZLE sees it.
  trap - INT
  {
    mkfifo -m 600 "$1" 2>/dev/null || result=1
  } always {
    if (( TRY_BLOCK_INTERRUPT )); then
      zf_rm -f "$1" 2>/dev/null
      _tp_cancel_inflight
    fi
  }
  return $result
}

# One timer supplies ticks for loading and short fades. It never requests AI or
# edits the command. Closing its reader stops the detached writer.
_tp_stop_glow() {
  [[ -n $_TP_GLOW_FD ]] || return 0
  zle -F "$_TP_GLOW_FD" 2>/dev/null
  { exec {_TP_GLOW_FD}>&- } 2>/dev/null
  _TP_GLOW_FD=""
  zf_rm -f "$_TP_TMP/glow" 2>/dev/null
}

_tp_start_glow() {
  [[ -z $_TP_GLOW_FD ]] || return 0
  local fifo="$_TP_TMP/glow"
  _tp_mkfifo "$fifo" || return 0
  (
    while true; do
      sleep 0.08
      print -r -- tick || break
    done
  ) >"$fifo" 2>/dev/null &!
  { exec {_TP_GLOW_FD}<>"$fifo" } 2>/dev/null || {
    zf_rm -f "$fifo" 2>/dev/null
    _TP_GLOW_FD=""
    return 0
  }
  zle -F -w "$_TP_GLOW_FD" _tp_on_glow
}

# Interrupts must unwind the callback, with watched fds removed outside a trap.
_tp_fd_widget() {
  setopt localoptions localtraps
  trap - INT
  {
    "$WIDGET" "$@"
  } always {
    if (( TRY_BLOCK_INTERRUPT )); then
      _tp_cancel_inflight
    fi
  }
}

_tp_on_glow() {
  local tick
  IFS= read -r tick <&$1 2>/dev/null
  if [[ -z ${BUFFER//[[:space:]]/} ]]; then
    _tp_dismiss
    _tp_render
    return 0
  fi
  if (( _TP_PULSE > 0 )); then
    if (( ${EPOCHREALTIME:-0} > 0 )); then
      if (( EPOCHREALTIME >= _TP_PULSE_UNTIL )); then
        _TP_PULSE=0
      else
        (( _TP_PULSE = (_TP_PULSE_UNTIL - EPOCHREALTIME) / 0.08 + 1 ))
      fi
      (( _TP_PULSE > 4 )) && _TP_PULSE=4
    else
      (( _TP_PULSE-- ))
    fi
  fi
  (( _TP_GLOW_PHASE++ ))
  _tp_render
}

_tp_render() {
  # Clear the complete UI even if a third-party edit widget bypassed ours, or
  # an older plugin left its counter in ZLE's separate message area.
  if [[ -z ${BUFFER//[[:space:]]/} ]]; then
    local -i changed=0
    [[ -n $POSTDISPLAY || -n ${(M)region_highlight:#*memo=termplete} ]] && changed=1
    _tp_clear_state
    if (( ! _TP_EMPTY_CLEARED )); then
      _TP_EMPTY_CLEARED=1
      zle -M ""
    fi
    region_highlight=("${(@)region_highlight:#*memo=termplete}")
    _TP_HIGHLIGHTS=()
    POSTDISPLAY=""
    (( changed )) && zle -R
    return 0
  fi
  _TP_EMPTY_CLEARED=0
  local ghost=${_TP_GHOST:-}
  [[ -n ${LBUFFER//[[:space:]]/} && -z $RBUFFER ]] || ghost=""
  local display=$ghost line highlight
  local -a highlights
  local -i color=0 animate=0 motion=0 start width=${COLUMNS:-80} height=${LINES:-24} index first last
  [[ ${+NO_COLOR} == 0 && $TERM != dumb ]] && color=1
  (( color )) && [[ $_TP_ANIMATIONS == true && ${TERMPLETE_NO_ANIM:-0} != 1 ]] && motion=1
  (( motion )) || _TP_PULSE=0
  if [[ -n $ghost ]] && (( color )); then
    local ghost_color=$_TP_MUTED_COLOR
    (( _TP_PULSE > 0 )) && ghost_color=${_TP_FADE_COLORS[$(( 5 - _TP_PULSE ))]}
    highlights+=("${#BUFFER} $(( ${#BUFFER} + ${#ghost} )) fg=$ghost_color memo=termplete")
  fi
  if (( _TP_MENU_OPEN )) && [[ -n ${LBUFFER//[[:space:]]/} && -z $RBUFFER ]]; then
    (( width = width > 4 ? width - 2 : 2 ))
    (( height = height > 5 ? height - 4 : 1 ))
    (( height > 5 )) && height=5
    (( first = ((_TP_CANDIDATE_INDEX - 1) / height) * height + 1 ))
    (( last = first + height - 1 ))
    (( last > ${#_TP_CANDIDATES} )) && last=${#_TP_CANDIDATES}
    line="TermPlete  $_TP_CANDIDATE_INDEX/${#_TP_CANDIDATES}"
    if (( ${#_TP_CANDIDATES} == 0 )); then
      line="TermPlete  No suggestions - tp status"
      if [[ -n $_TP_FD ]]; then
        line="TermPlete  Thinking..."
        if (( motion )); then
          animate=1
        fi
      fi
    fi
    line=${line[1,$width]}
    display+=$'\n'
    (( start = ${#BUFFER} + ${#display} ))
    display+=$line
    (( color )) && highlights+=("$start $(( start + ${#line} )) fg=$_TP_ACCENT_COLOR,bold memo=termplete")
    if (( color && ${#_TP_CANDIDATES} && ${#line} > 10 )); then
      local count_color=green
      (( _TP_PULSE > 2 )) && count_color=white
      highlights+=("$(( start + 10 )) $(( start + ${#line} )) fg=$count_color,bold memo=termplete")
    fi
    if (( animate )); then
      # A narrow white highlight sweeps over cyan letters, like a chat shimmer.
      # Its positions change; the text, prompt and cursor stay in place.
      local -i center=$(( _TP_GLOW_PHASE % (${#line} + 6) - 3 )) left right
      (( left = center - 2, right = center + 3 ))
      (( left < 0 )) && left=0
      (( right > ${#line} )) && right=${#line}
      if (( left < right )); then
        highlights+=("$(( start + left )) $(( start + right )) fg=white,bold memo=termplete")
      fi
    fi
    if (( ${#_TP_CANDIDATES} == 0 )) && [[ -n $_TP_ISSUE && -z $_TP_FD ]]; then
      line=${_TP_ISSUE[1,$width]}
      display+=$'\n'
      (( start = ${#BUFFER} + ${#display} ))
      display+=$line
      (( color )) && highlights+=("$start $(( start + ${#line} )) fg=yellow memo=termplete")
    fi
    for (( index=first; index <= last; index++ )); do
      line="  ${_TP_CANDIDATES[$index]}"
      (( index == _TP_CANDIDATE_INDEX )) && line="> ${_TP_CANDIDATES[$index]}"
      if (( ${#line} > width )); then
        line="${line[1,$(( width - 1 ))]}~"
      fi
      display+=$'\n'
      (( start = ${#BUFFER} + ${#display} ))
      display+=$line
      if (( color && index == _TP_CANDIDATE_INDEX )); then
        highlights+=("$start $(( start + ${#line} )) $_TP_SELECTION_STYLE memo=termplete")
      fi
    done
    line="Tab/Shift+Tab browse | Enter/Right accept | Alt+Right word | Esc close"
    (( ${#_TP_CANDIDATES} == 0 )) && line="$_TP_REVEAL_LABEL retry | Esc close"
    line=${line[1,$width]}
    display+=$'\n'
    (( start = ${#BUFFER} + ${#display} ))
    display+=$line
    if (( color )); then
      highlights+=("$start $(( start + ${#line} )) fg=$_TP_MUTED_COLOR memo=termplete")
      local label prefix key_color
      for label in Tab 'Shift+Tab' 'Enter/Right' Esc "$_TP_REVEAL_LABEL"; do
        [[ $line == *"$label"* ]] || continue
        prefix=${line%%"${label}"*}
        key_color=yellow
        [[ $label == Enter/Right ]] && key_color=green
        highlights+=("$(( start + ${#prefix} )) $(( start + ${#prefix} + ${#label} )) fg=$key_color,bold memo=termplete")
      done
    fi
  fi
  if (( animate || (_TP_PULSE > 0 && (${#ghost} || _TP_MENU_OPEN)) )); then
    _tp_start_glow
  else
    _tp_stop_glow
  fi
  if (( color )) && [[ -n $display ]]; then
    # Reset inherited gray text or backgrounds across our display, then apply
    # each specific highlight on top of the terminal's normal text colors.
    highlights=("${#BUFFER} $(( ${#BUFFER} + ${#display} )) fg=default,bg=default memo=termplete" "${highlights[@]}")
  fi
  [[ $POSTDISPLAY == "$display" && "${(j:|:)_TP_HIGHLIGHTS}" == "${(j:|:)highlights}" ]] && return 0
  # ZLE canonicalizes color styles; ownership survives that normalization.
  region_highlight=("${(@)region_highlight:#*memo=termplete}")
  POSTDISPLAY=$display
  _TP_HIGHLIGHTS=("${highlights[@]}")
  region_highlight+=("${_TP_HIGHLIGHTS[@]}")
  zle -R
}

_tp_close_menu() {
  if (( _TP_MENU_OPEN )); then
    _TP_MENU_OPEN=0
    _TP_PULSE=0
    _tp_render
  else
    zle "${_TP_ESCAPE_WIDGET[$KEYMAP]:-undefined-key}"
  fi
}

_tp_tab() {
  if [[ $BUFFER == *$'\n'* || $BUFFER == *$'\r'* ]]; then
    _tp_dismiss
    zle "${_TP_TAB_WIDGET[$KEYMAP]:-expand-or-complete}"
    return 0
  fi
  if (( ! _TP_GLOBAL_ENABLED || ! _TP_ENABLED )); then
    zle "${_TP_TAB_WIDGET[$KEYMAP]:-expand-or-complete}"
    return 0
  fi
  if [[ -z ${LBUFFER//[[:space:]]/} ]]; then
    _tp_dismiss
    return 0
  fi
  local typed=${BUFFER#${BUFFER%%[![:space:]]*}}
  if [[ $typed == \#* && -z $_TP_INTENT_BUFFER ]]; then
    _tp_dismiss
    return 0
  fi
  _tp_filter_candidates
  if (( ${#_TP_CANDIDATES} )); then
    if (( _TP_MENU_OPEN )); then
      _tp_cycle_forward
    else
      _TP_MENU_OPEN=1
      _TP_SELECTION_EXPLICIT=0
      _tp_start_pulse
      _TP_REVEALED=1
      _tp_select_candidate
      _tp_refine_common
      _tp_render
    fi
  else
    _TP_MENU_OPEN=1
    _TP_SELECTION_EXPLICIT=0
    _TP_REVEALED=1
    if [[ -n $_TP_FD ]]; then
      _TP_DIRTY=1
    else
      _tp_request "$LBUFFER"
    fi
    _tp_render
  fi
}

# Abandon the in-flight request so its reply cannot land later.
_tp_cancel_inflight() {
  local preserve_context=${1:-0}
  _tp_stop_glow
  _TP_DIRTY=0
  (( preserve_context )) || _TP_CONTEXT_BUFFER=""
  [[ -n $_TP_FD ]] || return 0
  local id=${_TP_INFLIGHT[$_TP_FD]}
  zle -F "$_TP_FD" 2>/dev/null
  unset "_TP_INFLIGHT[$_TP_FD]"
  zf_rm -f "$_TP_TMP/reply.$id" 2>/dev/null
  { exec {_TP_FD}>&- } 2>/dev/null
  _TP_FD=""
  [[ $id == "$_TP_REQ_ID" ]] && (( _TP_REQ_ID++ ))
}

# Re-entered by zle -F once the worker's reply is readable. Never blocks.
_tp_on_reply() {
  local fd=$1 id="" reply="" selected="" candidate="" typed=""
  local -i selection_pinned=0 reply_context_refine=0
  id=${_TP_INFLIGHT[$fd]}

  # Only a candidate reached through explicit browsing can pin a later result.
  if (( _TP_SELECTION_EXPLICIT && _TP_MENU_OPEN && _TP_CANDIDATE_INDEX > 0 &&
        _TP_CANDIDATE_INDEX <= ${#_TP_CANDIDATES} )); then
    selected=${_TP_CANDIDATES[$_TP_CANDIDATE_INDEX]}
    selected="${selected#${selected%%[![:space:]]*}}"
    if [[ -n $_TP_INTENT_BUFFER && $BUFFER == "$_TP_INTENT_BUFFER" ]]; then
      [[ $selected == "$_TP_FULL" ]] || selected=""
    else
      typed="${LBUFFER#${LBUFFER%%[![:space:]]*}}"
      if [[ -z $typed || -n $RBUFFER || -z $_TP_GHOST ||
            $_TP_FULL != "$LBUFFER$_TP_GHOST" ||
            $selected != "$typed"* || $selected == "$typed" ||
            "$LBUFFER${selected#$typed}" != "$_TP_FULL" ]]; then
        selected=""
      fi
    fi
    [[ -n $selected ]] && selection_pinned=1
  fi

  zle -F "$fd" 2>/dev/null
  IFS= read -r reply <&$fd 2>/dev/null
  unset "_TP_INFLIGHT[$fd]"
  { exec {fd}>&- } 2>/dev/null
  zf_rm -f "$_TP_TMP/reply.$id" 2>/dev/null
  [[ $_TP_FD == "$fd" ]] && _TP_FD=""

  if [[ -z ${LBUFFER//[[:space:]]/} ]]; then
    _tp_dismiss
    _tp_render
    return 0
  fi

  if [[ -n $_TP_INTENT_BUFFER && $BUFFER != "$_TP_INTENT_BUFFER" ]]; then
    _tp_dismiss
    _tp_render
    return 0
  fi
  if [[ -n $reply && $id == "$_TP_REQ_ID" ]]; then
    local -a fields
    fields=("${(@ps:\t:)reply}")
    _TP_ISSUE=""
    _TP_ALL_CANDIDATES=()
    case ${fields[1]} in
      C) _TP_ALL_CANDIDATES=("${fields[@]:1}");;
      R)
        _TP_ALL_CANDIDATES=("${fields[@]:1}")
        reply_context_refine=1
        ;;
      E) _TP_ISSUE=${fields[2]:-"Suggestions unavailable - tp status"}
         (( _TP_REVEALED )) && _TP_MENU_OPEN=1;;
    esac
    if [[ -n $selected ]]; then
      local found=0
      for candidate in "${_TP_ALL_CANDIDATES[@]}"; do
        candidate="${candidate#${candidate%%[![:space:]]*}}"
        if [[ $candidate == "$selected" ]]; then
          found=1
          break
        fi
      done
      (( found )) || _TP_ALL_CANDIDATES+=("$selected")
    fi
    _tp_filter_candidates
    if (( reply_context_refine )); then
      if [[ -n $_TP_REQUEST_BUFFER && -z $RBUFFER &&
            $BUFFER == "$_TP_REQUEST_BUFFER"* && ${#_TP_CANDIDATES} -gt 0 ]]; then
        _TP_CONTEXT_BUFFER=$BUFFER
      else
        _TP_CONTEXT_BUFFER=""
      fi
    fi
    if (( ${#_TP_CANDIDATES} )); then
      if (( selection_pinned )); then
        # Keep a user's Tab choice even if the refreshed ranking omitted it.
        _tp_select_candidate || {
          _TP_CANDIDATES=()
          _TP_CANDIDATE_INDEX=1
          _TP_SELECTION_EXPLICIT=0
        }
      else
        # Passive ghosts are defaults, so contextual results can replace them.
        _TP_SELECTION_EXPLICIT=0
        _TP_CANDIDATE_INDEX=1
        _tp_select_candidate || {
          _TP_CANDIDATES=()
          _TP_CANDIDATE_INDEX=1
        }
      fi
    fi
    if (( ! ${#_TP_CANDIDATES} )); then
      _TP_CANDIDATE_INDEX=1
      _TP_SELECTION_EXPLICIT=0
      _TP_FULL=""
      _TP_GHOST=""
      _TP_PENDING=""
    fi
  fi

  # Coalesce any keystrokes that landed while this request was outstanding.
  if (( _TP_DIRTY )); then
    _TP_DIRTY=0
    _tp_request "$LBUFFER" 0 1
  fi
  # Publish the reply only after its callback has finished changing state.
  _tp_render
}

# Spawn a detached fetch and watch its fifo. Returns immediately.
_tp_request() {
  local buffer=$1 intent=${2:-0} force_current=${3:-0}
  [[ $buffer != *$'\n'* && $buffer != *$'\r'* && $BUFFER != *$'\n'* && $BUFFER != *$'\r'* ]] || return 0
  local typed=${buffer#${buffer%%[![:space:]]*}}
  local -i context_refine=0
  # Only refetch a coalesced dirty edit while the user is completing a bare
  # command. Once arguments are present, a reply that already extends the
  # command line remains reusable.
  if (( force_current )) && [[ -z $typed || $typed == *[[:space:]]* ]]; then
    force_current=0
  fi
  if [[ -n $_TP_CONTEXT_BUFFER ]]; then
    if [[ $_TP_CONTEXT_BUFFER == "$buffer" ]]; then
      if (( _TP_REVEALED )) || [[ $_TP_MODE == automatic ]]; then
        context_refine=1
      fi
    else
      _TP_CONTEXT_BUFFER=""
    fi
  fi
  [[ -n ${buffer//[[:space:]]/} ]] || return 0
  (( ${#buffer} >= 2 && _TP_GLOBAL_ENABLED && _TP_ENABLED )) || return 0
  [[ -z $RBUFFER ]] || return 0
  # A reply for `do` may already extend `docker`, but it can contain just one
  # guess. Fetch the complete bare-tool options rather than pinning that reply.
  if (( context_refine == 0 && force_current == 0 )); then
    [[ $intent != 1 && -n $_TP_FULL && $_TP_FULL == "$buffer"?* && $typed != docker && $typed != ffmpeg ]] && return 0
  fi
  [[ $intent == 1 || $typed != \#* ]] || return 0
  # Coalesce edits while a worker is running. Never wait on the editor path.
  if [[ -n $_TP_FD ]]; then
    _TP_DIRTY=1
    return 0
  fi
  if (( context_refine == 0 && force_current == 0 )); then
    [[ $_TP_REQUEST_BUFFER == "$buffer" && -n $_TP_PENDING ]] && return 0
  fi
  _TP_REQUEST_BUFFER=$buffer
  _TP_ISSUE=""
  [[ $intent == 1 ]] && _TP_INTENT_BUFFER=$buffer
  (( _TP_REQ_ID++ ))
  local id=$_TP_REQ_ID
  local request_mode=$_TP_MODE
  (( _TP_REVEALED )) && [[ $_TP_MODE != off ]] && request_mode=automatic
  if (( context_refine )) && [[ $_TP_MODE != off ]]; then
    request_mode=automatic
    _TP_CONTEXT_BUFFER=""
    _TP_AI_BUFFER=$buffer
  fi
  local contextual_tool=0
  if [[ $request_mode == automatic && ($typed == docker || $typed == ffmpeg || $typed == ffmpeg\ *) ]]; then
    contextual_tool=1
    _TP_AI_BUFFER=$buffer
  fi
  local fifo="$_TP_TMP/reply.$id"

  zf_rm -f "$fifo" 2>/dev/null
  _tp_mkfifo "$fifo" || return 0

  # `&!` rather than `&` plus `disown`: the latter still creates a job entry, so
  # zsh prints `[2] 9208` on every suggestion and floods the prompt.
  (
    local reply="" instruction
    # History has its own short delay and never waits for the model debounce.
    if [[ $intent == 1 ]]; then
      instruction=${buffer#*\#}
      reply=$("$_TP_BIN" suggest --intent --shell-reply -- "$instruction" 2>/dev/null) || reply=$'E\tSuggestion service unavailable - tp status'
    elif (( ! context_refine )) && [[ $_TP_AI_HISTORY != true && $contextual_tool != 1 && $_TP_DECISION_STRATEGY == llm ]]; then
      sleep 0.03
      reply=$("$_TP_BIN" suggest --local-only --shell-reply --mode "$request_mode" --frame "$_TP_FRAME" --cursor "${#buffer}" -- "$buffer" 2>/dev/null)
    fi
    if [[ $intent != 1 && $reply != C$'\t'* &&
          ($request_mode != on-demand || $reply != R$'\t'*) ]]; then
      sleep $(( _TP_DEBOUNCE_MS / 1000.0 ))
      reply=$("$_TP_BIN" suggest --shell-reply --mode "$request_mode" --frame "$_TP_FRAME" --cursor "${#buffer}" -- "$buffer" 2>/dev/null) || reply=$'E\tSuggestion service unavailable - tp status'
    fi
    print -r -- "$reply"
  ) >"$fifo" 2>/dev/null &!

  # Read-write open never blocks, so a worker that fails to exec cannot freeze
  # the prompt.
  { exec {_TP_FD}<>"$fifo" } 2>/dev/null || {
    zf_rm -f "$fifo" 2>/dev/null
    _TP_FD=""
    return 0
  }
  _TP_INFLIGHT[$_TP_FD]=$id
  zle -F -w "$_TP_FD" _tp_on_reply
}

# Retain only alternatives that still extend the current input. Replies may
# arrive after more text was typed, narrowing the set of useful variants.
_tp_filter_candidates() {
  if [[ -n $_TP_INTENT_BUFFER && $BUFFER == "$_TP_INTENT_BUFFER" ]]; then
    _TP_CANDIDATES=("${_TP_ALL_CANDIDATES[@]}")
    _TP_CANDIDATE_INDEX=1
    local index candidate
    for (( index=1; index <= ${#_TP_CANDIDATES}; index++ )); do
      candidate=${_TP_CANDIDATES[$index]}
      candidate="${candidate#${candidate%%[![:space:]]*}}"
      if [[ $candidate == "$_TP_FULL" ]]; then
        _TP_CANDIDATE_INDEX=$index
        break
      fi
    done
    return 0
  fi
  local typed="${LBUFFER#${LBUFFER%%[![:space:]]*}}"
  local candidate
  local -a matching
  for candidate in "${_TP_ALL_CANDIDATES[@]}"; do
    candidate="${candidate#${candidate%%[![:space:]]*}}"
    [[ -n $typed && $candidate == "$typed"?* ]] && matching+=("$candidate")
  done
  _TP_CANDIDATES=("${matching[@]}")
  _TP_CANDIDATE_INDEX=1
  local index
  for (( index=1; index <= ${#_TP_CANDIDATES}; index++ )); do
    candidate=${_TP_CANDIDATES[$index]}
    if [[ "$LBUFFER${candidate#$typed}" == "$_TP_FULL" ]]; then
      _TP_CANDIDATE_INDEX=$index
      break
    fi
  done
}

# Keep full alternatives so browsing never launches a second model request.
_tp_select_candidate() {
  (( _TP_GLOBAL_ENABLED && _TP_ENABLED )) || return 1
  [[ -z $RBUFFER ]] || return 1
  local candidate=${_TP_CANDIDATES[$_TP_CANDIDATE_INDEX]}
  candidate="${candidate#${candidate%%[![:space:]]*}}"
  local typed="${LBUFFER#${LBUFFER%%[![:space:]]*}}"
  if [[ -n $_TP_INTENT_BUFFER && $BUFFER == "$_TP_INTENT_BUFFER" && -n $candidate ]]; then
    [[ $_TP_FULL != "$candidate" ]] && _tp_start_pulse
    _TP_FULL=$candidate
    _TP_PENDING=""
    _TP_GHOST=""
    return 0
  fi
  [[ -n $typed && -n $candidate && $candidate == "$typed"* && $candidate != "$typed" ]] || return 1
  local previous=$_TP_FULL previous_ghost=$_TP_GHOST
  _TP_PENDING="${candidate#$typed}"
  _TP_FULL="$LBUFFER$_TP_PENDING"
  if _tp_auto_show || (( _TP_REVEALED )); then
    _TP_GHOST=$_TP_PENDING
    [[ $_TP_FULL != "$previous" || -z $previous_ghost ]] && _tp_start_pulse
  fi
  return 0
}

# Reveal is separate from acceptance, even when a request is still in flight.
_tp_reveal() {
  (( _TP_GLOBAL_ENABLED && _TP_ENABLED )) || return 0
  if [[ -z ${LBUFFER//[[:space:]]/} ]]; then
    _tp_dismiss
    return 0
  fi
  _TP_REVEALED=1
  if [[ -n $_TP_PENDING ]]; then
    [[ -z $_TP_GHOST ]] && _tp_start_pulse
    _TP_GHOST=$_TP_PENDING
    _tp_render
  elif [[ -z $_TP_GHOST ]]; then
    # An explicit reveal bypasses On demand's automatic-request gating.
    if [[ $_TP_CONTEXT_BUFFER == "$BUFFER" ]]; then
      _tp_cancel_inflight 1
    else
      _tp_cancel_inflight
    fi
    _tp_request "$LBUFFER"
  fi
  _tp_refine_common
}

# On demand can hold instant recipes before reveal. The first explicit reveal
# asks AI to use current context, while repeated browsing stays request-free.
_tp_refine_common() {
  [[ $_TP_MODE != off ]] || return 0
  local typed=${LBUFFER#${LBUFFER%%[![:space:]]*}}
  if [[ $_TP_CONTEXT_BUFFER == "$LBUFFER" ]]; then
    [[ -n $_TP_FD ]] || _TP_REQUEST_BUFFER=""
    _tp_request "$LBUFFER"
  elif [[ ($typed == docker || $typed == ffmpeg) && $_TP_AI_BUFFER != "$LBUFFER" ]]; then
    [[ -n $_TP_FD ]] || _TP_REQUEST_BUFFER=""
    _tp_request "$LBUFFER"
  fi
}

_tp_accept() {
  (( _TP_GLOBAL_ENABLED && _TP_ENABLED )) || return 1
  local -i word_only=${1:-0} start=${#LBUFFER} end
  local prefix=$BUFFER command
  if [[ -n $_TP_INTENT_BUFFER && $BUFFER == "$_TP_INTENT_BUFFER" && -n $_TP_FULL ]]; then
    command=$_TP_FULL
    start=0
    if (( _TP_MENU_OPEN )); then
      local selected=${_TP_CANDIDATES[$_TP_CANDIDATE_INDEX]}
      [[ -n $selected && $selected == "$command" ]] || return 1
    fi
  else
    [[ -n ${LBUFFER//[[:space:]]/} && -n $_TP_GHOST && -z $RBUFFER && $_TP_FULL == "$LBUFFER$_TP_GHOST" ]] || return 1
    prefix=$LBUFFER
    command="$LBUFFER$_TP_GHOST"
    if (( _TP_MENU_OPEN )); then
      local selected=${_TP_CANDIDATES[$_TP_CANDIDATE_INDEX]}
      selected="${selected#${selected%%[![:space:]]*}}"
      local typed="${LBUFFER#${LBUFFER%%[![:space:]]*}}"
      [[ -n $selected && -n $typed && $selected == "$typed"* && $selected != "$typed" &&
         $command == "$LBUFFER${selected#$typed}" ]] || return 1
    fi
  fi
  if (( word_only )); then
    # Use the user's native word boundaries without committing the remaining text.
    local saved_buffer=$BUFFER saved_cursor=$CURSOR
    BUFFER=$command
    CURSOR=$start
    zle .forward-word
    end=$CURSOR
    BUFFER=$saved_buffer
    CURSOR=$saved_cursor
    (( end > start )) || return 1
    if (( end < ${#command} )); then
      _tp_cancel_inflight
      BUFFER=${command[1,end]}
      CURSOR=$end
      _TP_FULL=$command
      _TP_GHOST=${command[end+1,-1]}
      _TP_PENDING=$_TP_GHOST
      _TP_INTENT_BUFFER=""
      _TP_MENU_OPEN=0
      _TP_REVEALED=1
      _TP_CANDIDATES=("$command")
      _TP_ALL_CANDIDATES=("$command")
      _TP_CANDIDATE_INDEX=1
      _TP_SELECTION_EXPLICIT=0
      _tp_render
      return 0
    fi
  fi
  _tp_dismiss
  BUFFER=$command
  CURSOR=${#BUFFER}
  "$_TP_BIN" feedback --prefix "$prefix" --cwd "$PWD" -- "$command" >/dev/null 2>&1 &!
  return 0
}

_tp_accept_or_complete() {
  _tp_tab
}

_tp_accept_or_forward() {
  _tp_accept && return 0
  zle .forward-char
  [[ -n $RBUFFER ]] && _tp_dismiss
}

_tp_accept_word_or_forward() {
  _tp_accept 1 && return 0
  _tp_history_arrow forward-word
}

# Cycle to the next candidate rather than inserting a literal tab.
_tp_cycle() {
  _tp_cycle_by -1
}

_tp_cycle_forward() {
  _tp_cycle_by 1
}

# Up/Down always keep shell history and multiline navigation, even while a
# suggestion selector is open. Tab and Shift+Tab browse suggestions.
_tp_history_arrow() {
  local slot="$KEYMAP:${(V)KEYS}" original
  original=${_TP_ARROW_WIDGET[$slot]}
  # Some terminals only bind one arrow encoding. Give newly bound encodings
  # native history behavior, and never delegate back into our own wrapper.
  case $original in
    ''|undefined-key|_tp_up|_tp_down|_tp_accept_word_or_forward) original=".$1" ;;
  esac
  _tp_dismiss
  zle "$original"
}

_tp_up() { _tp_history_arrow up-line-or-history; }
_tp_down() { _tp_history_arrow down-line-or-history; }

_tp_enter() {
  if (( ! _TP_GLOBAL_ENABLED || ! _TP_ENABLED )) || [[ $BUFFER == *$'\n'* || $BUFFER == *$'\r'* ]]; then
    _tp_dismiss
    zle _tp_original_accept_line
    return 0
  fi
  if (( _TP_MENU_OPEN )); then
    _tp_accept && return 0
    # A pending or empty selector is still an explicit choice to browse.
    # Never send the incomplete prefix to the shell from this state.
    return 0
  fi
  local typed=${BUFFER#${BUFFER%%[![:space:]]*}}
  if (( ! _TP_PASTED )) && [[ $typed == \#* && -n ${${typed#\#}//[[:space:]]/} && -z $_TP_INTENT_BUFFER ]]; then
    _tp_dismiss
    _TP_MENU_OPEN=1
    _TP_REVEALED=1
    CURSOR=${#BUFFER}
    _tp_request "$BUFFER" 1
    _tp_render
    return 0
  fi
  _tp_dismiss
  zle _tp_original_accept_line
}

_tp_cycle_by() {
  _TP_REVEALED=1
  _tp_filter_candidates
  local attempt direction=${1:-1}
  (( ${#_TP_CANDIDATES} )) && _TP_MENU_OPEN=1
  for (( attempt=0; attempt < ${#_TP_CANDIDATES}; attempt++ )); do
    (( _TP_CANDIDATE_INDEX = (_TP_CANDIDATE_INDEX - 1 + direction + ${#_TP_CANDIDATES}) % ${#_TP_CANDIDATES} + 1 ))
    if _tp_select_candidate; then
      _TP_SELECTION_EXPLICIT=1
      _tp_render
      return 0
    fi
  done
  _tp_reveal
}

# Drop the current suggestion without touching the buffer.
_tp_clear_state() {
  _tp_cancel_inflight
  _TP_PULSE=0
  _TP_PULSE_UNTIL=0
  _TP_GHOST=""
  _TP_PENDING=""
  _TP_FULL=""
  _TP_ISSUE=""
  _TP_INTENT_BUFFER=""
  _TP_CONTEXT_BUFFER=""
  _TP_AI_BUFFER=""
  _TP_REQUEST_BUFFER=""
  _TP_CANDIDATES=()
  _TP_ALL_CANDIDATES=()
  _TP_CANDIDATE_INDEX=1
  _TP_SELECTION_EXPLICIT=0
  _TP_REVEALED=0
  _TP_MENU_OPEN=0
}

_tp_dismiss() {
  _tp_clear_state
  _tp_render
  return 0
}

_tp_dismiss_or_interrupt() {
  _tp_dismiss
  # Ctrl+C must continue to cancel input normally.
  if [[ $_TP_KEY_DISMISS == '^C' ]]; then
    zle .send-break
  fi
}

# Turn suggestions off and back on without restarting the shell.
_tp_toggle() {
  if (( ! _TP_TEMP_READY )); then
    zle -M "termplete could not create a private temporary directory"
    return 0
  fi
  if (( ${+_TP_ENABLED} && _TP_ENABLED )); then
    _TP_ENABLED=0
    _tp_dismiss
    [[ -n ${BUFFER//[[:space:]]/} ]] && zle -M "termplete off"
  else
    _TP_ENABLED=1
    [[ -n ${BUFFER//[[:space:]]/} ]] && zle -M "termplete on"
  fi
}

# The edit path. The full suggestion is retained so a reply for "git st" stays
# valid as the user types "git sta" — it is simply one character shorter.
_tp_after_edit() {
  (( _TP_GLOBAL_ENABLED && _TP_ENABLED )) || return 0
  if [[ -z ${LBUFFER//[[:space:]]/} ]]; then
    _tp_dismiss
    return 0
  fi
  local typed=${BUFFER#${BUFFER%%[![:space:]]*}}
  if [[ $typed == \#* ]]; then
    _tp_dismiss
    return 0
  fi
  _TP_REVEALED=0
  _TP_MENU_OPEN=0
  _TP_SELECTION_EXPLICIT=0
  _TP_CONTEXT_BUFFER=""
  _tp_filter_candidates
  if [[ -n $_TP_FULL && $_TP_FULL == "$LBUFFER"* && -n $_TP_GHOST ]]; then
    _TP_GHOST="${_TP_FULL#$LBUFFER}"
    _TP_PENDING=$_TP_GHOST
    _tp_render
  else
    _TP_FULL=""
    _TP_CANDIDATES=()
    _TP_ALL_CANDIDATES=()
    _TP_CANDIDATE_INDEX=1
    _TP_GHOST=""
    _TP_PENDING=""
    _tp_render
  fi
  _tp_next_frame
  _tp_request "$LBUFFER"
}

_tp_self_insert() {
  zle .self-insert
  _tp_after_edit
}

_tp_bracketed_paste() {
  _tp_dismiss
  zle _tp_original_bracketed_paste
  local result=$?
  _TP_PASTED=1
  _tp_dismiss
  # Acknowledgement orders pending input before any subsequent execution record.
  if (( _TP_GLOBAL_ENABLED && _TP_ENABLED )); then
    "$_TP_BIN" record --pending --cwd "$PWD" -- "$BUFFER" >/dev/null 2>&1
  fi
  return $result
}

_tp_magic_space() {
  zle _tp_original_magic_space
  _tp_after_edit
}

_tp_backward_delete() {
  _tp_dismiss
  zle .backward-delete-char
}

_tp_vi_backward_delete() {
  _tp_dismiss
  zle .vi-backward-delete-char
}

_tp_delete_char() {
  _tp_dismiss
  zle .delete-char
}

# Record execution outcomes, rather than treating every edited line as success.
_tp_record() {
  local cmd=$1 code=${2:-0}
  (( _TP_GLOBAL_ENABLED )) && [[ -n ${cmd//[[:space:]]/} ]] || return 0
  "$_TP_BIN" record --cwd "$_TP_LAST_COMMAND_CWD" --exit-code "$code" -- "$cmd" >/dev/null 2>&1 &!
}

_tp_preexec() {
  _TP_LAST_COMMAND=$1
  _TP_LAST_COMMAND_CWD=$PWD
  _TP_COMMAND_ENABLED=$_TP_GLOBAL_ENABLED
  _TP_FRAME_OPEN=0
  if (( _TP_COMMAND_ENABLED )) && [[ ${TERMPLETE_CAPTURE_ACTIVE:-0} == 1 ]]; then
    _TP_FRAME_OPEN=1
    "$_TP_BIN" frame --token "$_TP_FRAME_TOKEN" --command "$1" --cwd "$PWD"
  fi
}

_tp_precmd() {
  local code=$?
  _tp_refresh_settings
  if [[ -n $_TP_LAST_COMMAND ]]; then
    if (( _TP_FRAME_OPEN )); then
      printf '\e]777;termplete;{"event":"end","code":%d,"token":"%s"}\a' "$code" "$_TP_FRAME_TOKEN"
      _TP_FRAME_OPEN=0
    elif (( _TP_GLOBAL_ENABLED && _TP_COMMAND_ENABLED )) && [[ ${TERMPLETE_CAPTURE_ACTIVE:-0} != 1 ]]; then
      _tp_record "$_TP_LAST_COMMAND" "$code"
    fi
    _TP_LAST_COMMAND=""
    _TP_LAST_COMMAND_CWD=""
    _TP_COMMAND_ENABLED=0
  fi
  return 0
}

_tp_bind() {
  local map=$1 key=$2 widget=$3 slot original
  # terminfo returns raw Escape bytes while config uses caret notation. Store
  # one original binding even when both spellings name the same key.
  [[ $key == *$'\e'* ]] && key=${(V)key}
  slot="$map:$key"
  if (( ! ${+_TP_BINDINGS[$slot]} )); then
    original=${${(z)"$(bindkey -M "$map" "$key")"}[-1]}
    _TP_BINDINGS[$slot]=$original
  fi
  bindkey -M "$map" "$key" "$widget" 2>/dev/null
}

_tp_install() {
  # Restore every previous custom binding before applying the current set.
  # Otherwise an old reveal/cycle key keeps invoking TermPlete after reset.
  local slot old_widget map key original
  for slot old_widget in "${(@kv)_TP_BINDINGS}"; do
    map=${slot%%:*}
    key=${slot#*:}
    bindkey -M "$map" "$key" "$old_widget" 2>/dev/null
  done
  _TP_BINDINGS=()
  zle -N _tp_on_reply _tp_fd_widget
  zle -N _tp_on_glow _tp_fd_widget
  zle -N _tp_reveal
  zle -N _tp_accept
  zle -N _tp_accept_or_complete
  zle -N _tp_tab
  zle -N _tp_accept_or_forward
  zle -N _tp_accept_word_or_forward
  zle -N _tp_cycle
  zle -N _tp_cycle_forward
  zle -N _tp_up
  zle -N _tp_down
  if (( ! ${+widgets[_tp_original_accept_line]} )); then
    zle -A accept-line _tp_original_accept_line
  fi
  zle -N accept-line _tp_enter
  if (( ! ${+widgets[_tp_original_bracketed_paste]} )); then
    zle -A bracketed-paste _tp_original_bracketed_paste
  fi
  zle -N bracketed-paste _tp_bracketed_paste
  zle -N _tp_close_menu
  zle -N _tp_dismiss
  zle -N _tp_dismiss_or_interrupt
  zle -N _tp_toggle
  zle -N _tp_self_insert
  if (( ! ${+widgets[_tp_original_magic_space]} )); then
    zle -A magic-space _tp_original_magic_space
  fi
  zle -N magic-space _tp_magic_space
  zle -N _tp_backward_delete
  zle -N _tp_vi_backward_delete
  zle -N _tp_delete_char

  local -a cycle_keys
  cycle_keys=("$_TP_KEY_CYCLE")
  if [[ $_TP_KEY_CYCLE == '^[[Z' ]]; then
    # Legacy back-tab, modified CSI, Kitty and xterm modifyOtherKeys encodings.
    cycle_keys+=('^[[1;2Z' '^[[9;2u' '^[[27;2;9~')
    zmodload zsh/terminfo 2>/dev/null
    [[ -n ${terminfo[kcbt]:-} ]] && cycle_keys+=("${terminfo[kcbt]}")
  fi
  _TP_TAB_WIDGET[main]=${${(z)"$(bindkey '^I')"}[-1]}
  _TP_ESCAPE_WIDGET[main]=${${(z)"$(bindkey '^[')"}[-1]}
  local -a up_keys down_keys word_keys
  up_keys=('^[[A' '^[OA' "${terminfo[kcuu1]:-}")
  down_keys=('^[[B' '^[OB' "${terminfo[kcud1]:-}")
  word_keys=('^[[1;3C' '^[f' "${terminfo[kRIT3]:-}")
  _TP_ARROW_WIDGET=()
  # Capture every encoding before changing either map: main aliases one of
  # these maps, and CSI and application-mode arrows may use different widgets.
  for map in main emacs viins; do
    for key in "${up_keys[@]}" "${down_keys[@]}" "${word_keys[@]}"; do
      [[ -n $key ]] || continue
      [[ $key == *$'\e'* ]] && key=${(V)key}
      slot="$map:$key"
      _TP_ARROW_WIDGET[$slot]=${${(z)"$(bindkey -M "$map" "$key")"}[-1]}
    done
  done
  for map in emacs viins; do
    original=${${(z)"$(bindkey -M "$map" '^I')"}[-1]}
    [[ $original == _tp_accept_or_complete ]] || _TP_TAB_WIDGET[$map]=$original
    original=${${(z)"$(bindkey -M "$map" '^[')"}[-1]}
    [[ $original == _tp_close_menu ]] || _TP_ESCAPE_WIDGET[$map]=$original
    for key in "${word_keys[@]}"; do
      [[ -n $key ]] && _tp_bind "$map" "$key" _tp_accept_word_or_forward
    done
    _tp_bind "$map" "$_TP_KEY_REVEAL" _tp_reveal
    _tp_bind "$map" "$_TP_KEY_ACCEPT" _tp_accept
    for key in "${cycle_keys[@]}"; do
      _tp_bind "$map" "$key" _tp_cycle
    done
    _tp_bind "$map" '^I' _tp_accept_or_complete
    _tp_bind "$map" '^[[C' _tp_accept_or_forward
    _tp_bind "$map" '^[OC' _tp_accept_or_forward
    for key in "${up_keys[@]}"; do
      [[ -n $key ]] && _tp_bind "$map" "$key" _tp_up
    done
    for key in "${down_keys[@]}"; do
      [[ -n $key ]] && _tp_bind "$map" "$key" _tp_down
    done
    _tp_bind "$map" '^[' _tp_close_menu
    _tp_bind "$map" "$_TP_KEY_TOGGLE" _tp_toggle
    _tp_bind "$map" "$_TP_KEY_DISMISS" _tp_dismiss_or_interrupt
  done

  # Override the editing widgets so the ghost refreshes as the user types.
  # Suggestions appearing with no keypress is the point of the plugin.
  zle -N self-insert          _tp_self_insert
  zle -N backward-delete-char _tp_backward_delete
  zle -N vi-backward-delete-char _tp_vi_backward_delete
  zle -N delete-char          _tp_delete_char
}

_tp_install

# Fresh prompt: drop stale state and any outstanding request.
_tp_line_init() {
  _TP_PASTED=0
  _tp_refresh_settings
  if (( _TP_CLEAR_ON_LINE_INIT )); then
    _tp_clear_state
    _TP_CLEAR_ON_LINE_INIT=0
  fi
  if (( _TP_SETTINGS_CHANGED )); then
    _tp_install
    _TP_SETTINGS_CHANGED=0
  fi
  _tp_cancel_inflight
  _TP_GHOST=""
  _TP_PENDING=""
  _TP_FULL=""
  _TP_ISSUE=""
  _TP_INTENT_BUFFER=""
  _TP_CANDIDATES=()
  _TP_ALL_CANDIDATES=()
  _TP_CANDIDATE_INDEX=1
  _TP_SELECTION_EXPLICIT=0
  _TP_REVEALED=0
  _TP_MENU_OPEN=0
  _TP_DIRTY=0
  _TP_PULSE=0
  _tp_render
}

# Enter pressed: record the command, then let zsh run it.
_tp_line_finish() {
  _tp_cancel_inflight
  _TP_PULSE=0
  _TP_MENU_OPEN=0
  _TP_SELECTION_EXPLICIT=0
  _TP_GHOST=""
  _tp_render
}

# Cover other editing widgets too, including Ctrl+U and vi line deletion.
_tp_before_redraw() {
  if [[ -z ${LBUFFER//[[:space:]]/} || -n $RBUFFER ]]; then
    [[ -n ${BUFFER//[[:space:]]/} ]] || _TP_PASTED=0
    _tp_dismiss
  elif [[ -n $_TP_INTENT_BUFFER ]]; then
    [[ $BUFFER == "$_TP_INTENT_BUFFER" ]] || _tp_dismiss
  elif [[ -n $_TP_FULL && $_TP_FULL != "$LBUFFER${_TP_GHOST:-$_TP_PENDING}" ]]; then
    _tp_dismiss
  fi
  # Other widgets may append a gray autosuggestion highlight spanning the
  # entire POSTDISPLAY. Our explicit foreground/background take precedence.
  if [[ -n $POSTDISPLAY && ${#_TP_HIGHLIGHTS} -gt 0 ]]; then
    local highlight
    local -i owned_seen=0 needs_reorder=0
    for highlight in "${region_highlight[@]}"; do
      if [[ $highlight == *memo=termplete ]]; then
        owned_seen=1
      elif (( owned_seen )); then
        needs_reorder=1
        break
      fi
    done
    if (( needs_reorder )); then
      # Preserve ZLE's canonicalized styles and avoid rewriting them on every
      # redraw; repeatedly changing this array can keep the editor repainting.
      region_highlight=("${(@)region_highlight:#*memo=termplete}" "${(@M)region_highlight:#*memo=termplete}")
    fi
  fi
  return 0
}

autoload -Uz add-zle-hook-widget
add-zle-hook-widget line-init _tp_line_init
add-zle-hook-widget line-finish _tp_line_finish
add-zle-hook-widget line-pre-redraw _tp_before_redraw

autoload -Uz add-zsh-hook
add-zsh-hook preexec _tp_preexec
add-zsh-hook precmd _tp_precmd

typeset -g _TERMPLETE_ZSH_LOADED=1
