"""Focused PTY checks for stopping and restarting loaded shell integrations."""

import errno
import json
import os
import select
import shlex
import shutil
import subprocess
import time
import unittest
from pathlib import Path

from terminal import ANSI, BIN, Session


PROMPT = b"PAUSE_READY> "
BASH = shutil.which("bash") or "/bin/bash"


def bash_version():
    version = subprocess.check_output(
        [BASH, "-c", 'printf "%s.%s" "$((BASH_VERSINFO[0]))" "$((BASH_VERSINFO[1]))"'],
        text=True,
    )
    return tuple(int(part) for part in version.split("."))


def action_log(session):
    path = Path(session.home.name, "tp-actions")
    return path.read_text().splitlines() if path.exists() else []


def wait_for_action(session, action, timeout=3, after=0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if action in action_log(session)[after:]:
            return True
        time.sleep(0.03)
    return False


def wait_for_no_action(session, action, timeout=0.7, after=0):
    deadline = time.monotonic() + timeout
    while True:
        if action in action_log(session)[after:]:
            return False
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return True
        time.sleep(min(0.03, remaining))


def visible_terminal_bytes(raw):
    visible = bytearray()
    raw_ends = []
    raw_position = 0
    while raw_position < len(raw):
        escape = ANSI.match(raw, raw_position)
        if escape:
            raw_position = escape.end()
            continue
        char = raw[raw_position]
        raw_position += 1
        if char != 13:
            visible.append(char)
            raw_ends.append(raw_position)
    return bytes(visible), raw_ends


def read_line(session, line, after=0, timeout=4):
    marker = b"\n" + line + b"\n"
    deadline = time.monotonic() + timeout
    while True:
        visible, raw_ends = visible_terminal_bytes(session.data[after:])
        position = visible.find(marker)
        if position >= 0:
            return after + raw_ends[position + len(marker) - 1]
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(f"missing full output line {line!r}")
        if select.select([session.fd], [], [], remaining)[0]:
            try:
                session.data += os.read(session.fd, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
    raise AssertionError(f"missing full output line {line!r}")


def zsh_rc():
    real_bin = shlex.quote(BIN)
    return (
        'PROMPT="PAUSE_READY> "\n'
        'cat >"$HOME/tp-proxy" <<\'TP_PROXY\'\n'
        '#!/bin/sh\n'
        'case "$1" in\n'
        '  suggest|record|feedback) printf \'%s\\n\' "$1" >>"$HOME/tp-actions" ;;\n'
        'esac\n'
        f'exec {real_bin} "$@"\n'
        'TP_PROXY\n'
        'chmod +x "$HOME/tp-proxy"\n'
        'export _TP_BIN="$HOME/tp-proxy"\n'
        f'eval "$({real_bin} init --print zsh)"\n'
    )


def bash_rc():
    real_bin = shlex.quote(BIN)
    shell = shlex.quote(BASH)
    return (
        "unset PROMPT_COMMAND\n"
        f"export SHELL={shell}\n"
        "PS1='PAUSE_READY> '\n"
        'cat >"$HOME/tp-proxy" <<\'TP_PROXY\'\n'
        '#!/bin/sh\n'
        'case "$1" in\n'
        '  suggest|record|feedback) printf \'%s\\n\' "$1" >>"$HOME/tp-actions" ;;\n'
        'esac\n'
        f'exec {real_bin} "$@"\n'
        'TP_PROXY\n'
        'chmod +x "$HOME/tp-proxy"\n'
        'export _TP_BIN="$HOME/tp-proxy"\n'
        f'eval "$({real_bin} init --print bash)"\n'
    )


def run_command(session, command, output_line, timeout=4):
    boundary = session.send_menu((command + "\r").encode())
    marker_end = read_line(session, output_line, after=boundary, timeout=timeout)
    session.read_until(PROMPT, after=marker_end, timeout=timeout)


def cancel_line(session):
    boundary = session.send_menu(b"\x03")
    session.read_until(PROMPT, after=boundary)


class ShellPauseTests(unittest.TestCase):
    def test_stop_pauses_and_start_resumes_an_already_loaded_shell(self):
        session = Session(
            ["-i"], executable="/bin/zsh", rc=zsh_rc(),
            config='provider = "terminal-test"\n[daemon]\nidle_shutdown_secs = 5\n',
        )
        try:
            session.read_until(PROMPT)
            run_command(
                session, f"{shlex.quote(BIN)} stop",
                b"TermPlete paused for new and refreshed shell integrations.",
            )
            # Let any request started while typing the stop command finish
            # before taking the paused-state baseline.
            time.sleep(0.7)
            paused_actions = action_log(session)

            boundary = session.send_menu(b"printf 'PAUSED_COMMAND\\n'\r")
            marker_end = read_line(session, b"PAUSED_COMMAND", after=boundary)
            session.read_until(PROMPT, after=marker_end)
            session.send_menu(b"git")
            time.sleep(0.7)
            cancel_line(session)
            paused_actions_after = action_log(session)[len(paused_actions):]
            self.assertFalse(
                {"suggest", "record", "feedback"} & set(paused_actions_after),
                paused_actions_after,
            )
            self.assertNotIn(b"TermPlete", session.data[boundary:])

            self.assert_resume_preserves_local_toggle(session)
        finally:
            session.close()

    def assert_resume_preserves_local_toggle(self, session):
        # Confirm the local toggle survives the global resume before typing.
        session.send_menu(b"\x18\x18")
        run_command(
            session, f"{shlex.quote(BIN)} start",
            b"TermPlete resumed; existing integrations can use it now.",
            timeout=10,
        )
        run_command(
            session, 'printf \'LOCAL_ENABLED=%s\\n\' "$_TP_ENABLED"',
            b"LOCAL_ENABLED=0",
        )
        local_off_actions = action_log(session)
        session.send_menu(b"git")
        self.assertTrue(
            wait_for_no_action(session, "suggest", after=len(local_off_actions)),
            action_log(session)[len(local_off_actions):],
        )
        cancel_line(session)
        self.assertNotIn("suggest", action_log(session)[len(local_off_actions):])
        session.send_menu(b"\x18\x18")
        local_on_actions = len(action_log(session))
        session.send_menu(b"git")
        self.assertTrue(wait_for_action(session, "suggest", after=local_on_actions))
        cancel_line(session)

    def test_bash_stop_pauses_and_start_resumes_history_capture(self):
        session = Session(
            ["-i"], executable=BASH, rc=bash_rc(),
            config='provider = "terminal-test"\n[daemon]\nidle_shutdown_secs = 5\n',
        )
        try:
            session.read_until(PROMPT)
            run_command(
                session, f"{shlex.quote(BIN)} stop",
                b"TermPlete paused for new and refreshed shell integrations.",
            )
            paused_actions = action_log(session)
            run_command(session, "printf 'PAUSED_COMMAND\\n'", b"PAUSED_COMMAND")
            paused_actions_after = action_log(session)[len(paused_actions):]
            self.assertFalse(
                {"record"} & set(paused_actions_after),
                paused_actions_after,
            )

            run_command(
                session, f"{shlex.quote(BIN)} start",
                b"TermPlete resumed; existing integrations can use it now.",
                timeout=10,
            )
            resumed_actions = len(action_log(session))
            run_command(session, "printf 'RESUMED_COMMAND\\n'", b"RESUMED_COMMAND")
            self.assertTrue(wait_for_action(session, "record", after=resumed_actions))
        finally:
            session.close()

    def test_bash_ctrl_t_preview_resumes_after_global_pause(self):
        version = bash_version()
        if version < (4, 0):
            self.skipTest(
                f"Ctrl-T preview requires Bash 4.0+ Readline buffer variables; found {version[0]}.{version[1]}"
            )
        session = Session(
            ["-i"], executable=BASH, rc=bash_rc(),
            config='provider = "terminal-test"\n[daemon]\nidle_shutdown_secs = 5\n',
        )
        try:
            session.read_until(PROMPT)
            run_command(
                session, f"{shlex.quote(BIN)} stop",
                b"TermPlete paused for new and refreshed shell integrations.",
            )
            run_command(
                session, f"{shlex.quote(BIN)} start",
                b"TermPlete resumed; existing integrations can use it now.",
                timeout=10,
            )
            resumed_actions = len(action_log(session))
            session.send_menu(b"git\x14")
            self.assertTrue(wait_for_action(session, "suggest", after=resumed_actions))
            cancel_line(session)
        finally:
            session.close()

    def test_bash_default_tab_completion_resumes_after_global_pause(self):
        version = bash_version()
        if version < (4, 1):
            self.skipTest(
                f"default Tab completion requires Bash 4.1+ complete -D; found {version[0]}.{version[1]}"
            )
        session = Session(
            ["-i"], executable=BASH, rc=bash_rc(),
            config='provider = "terminal-test"\n[daemon]\nidle_shutdown_secs = 5\n',
        )
        try:
            session.read_until(PROMPT)
            run_command(
                session, f"{shlex.quote(BIN)} stop",
                b"TermPlete paused for new and refreshed shell integrations.",
            )
            paused_actions = len(action_log(session))
            # The default compspec handles arguments; bare `git` uses command-name completion.
            session.send_menu(b"git st\t")
            time.sleep(0.2)
            self.assertNotIn("suggest", action_log(session)[paused_actions:])
            cancel_line(session)

            run_command(
                session, f"{shlex.quote(BIN)} start",
                b"TermPlete resumed; existing integrations can use it now.",
                timeout=10,
            )
            resumed_actions = len(action_log(session))
            session.send_menu(b"git st\t")
            self.assertTrue(wait_for_action(session, "suggest", after=resumed_actions))
            cancel_line(session)
        finally:
            session.close()

    def test_capture_output_does_not_carry_paused_output_across_resume(self):
        session = Session(
            ["-i"], executable="/bin/zsh",
            rc=('PROMPT="PAUSE_READY> "\n'
                f'eval "$({shlex.quote(BIN)} init --print zsh)"\n'),
            capture_output=True,
            config=('provider = "terminal-test"\n'
                    '[context]\nimport_shell_history = false\ncapture_output = true\n'
                    '[daemon]\nidle_shutdown_secs = 5\n'),
        )
        history = Path(session.home.name, "termplete/history.json")
        try:
            session.read_until(PROMPT)
            run_command(
                session, f"{shlex.quote(BIN)} stop",
                b"TermPlete paused for new and refreshed shell integrations.",
            )
            run_command(
                session, "printf 'CAPTURE_PAUSED_SECRET\\n'", b"CAPTURE_PAUSED_SECRET",
            )
            run_command(
                session, f"{shlex.quote(BIN)} start",
                b"TermPlete resumed; existing integrations can use it now.",
                timeout=10,
            )
            command = "printf 'CAPTURE_RESUMED_MARK\\n'"
            run_command(session, command, b"CAPTURE_RESUMED_MARK")

            deadline = time.monotonic() + 3
            entries = []
            while time.monotonic() < deadline:
                if history.exists():
                    entries = json.loads(history.read_text())
                    if any(item.get("command") == command for item in entries):
                        break
                time.sleep(0.05)
            entry = next(item for item in entries if item.get("command") == command)
            self.assertIn("CAPTURE_RESUMED_MARK", entry["output"])
            self.assertFalse(any(
                "CAPTURE_PAUSED_SECRET" in item.get("output", "") for item in entries
            ))
            session.send_menu(b"exit 0\r")
            self.assertEqual(session.wait(), 0)
        finally:
            session.close()


if __name__ == "__main__":
    unittest.main()
