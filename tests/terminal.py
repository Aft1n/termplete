"""Real PTY regression checks. Run after `cargo build` with python3 tests/terminal.py."""

import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unittest


BIN = os.environ.get("TERMPLETE_TEST_BIN", str(Path(__file__).resolve().parents[1] / "target/debug/tp"))
ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")


def selected_rows(colors):
    return [row for row in colors.splitlines() if re.search(r"\bbg=(?!default\b)", row)]


class Session:
    def __init__(self, args, terminal="xterm-256color", rows=24, cols=80, no_color=False, config=None, executable=None, rc=None, terminal_app=None, history=None, capture_output=False):
        self.home = tempfile.TemporaryDirectory(prefix="tp-terminal-")
        # Host startup files must not prompt or change these isolated sessions.
        (Path(self.home.name) / ".zshenv").write_text("unsetopt GLOBAL_RCS\n")
        config_dir = Path(self.home.name) / "termplete"
        config_dir.mkdir()
        (config_dir / "config.toml").write_text(config or 'provider = "terminal-test"\n[daemon]\nidle_shutdown_secs = 1\n')
        if history is not None:
            (Path(self.home.name) / ".zsh_history").write_text(history)
        if rc is not None:
            (Path(self.home.name) / ".zshrc").write_text(rc)
            (Path(self.home.name) / ".bashrc").write_text(rc)
        executable = executable or BIN
        self.is_zsh = Path(executable).name == "zsh"
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            env = dict(os.environ, HOME=self.home.name, ZDOTDIR=self.home.name, _TP_BIN=BIN,
                       SHELL="/bin/zsh",
                       TERMPLETE_HOME=self.home.name, TERM=terminal,
                       TERMPLETE_CAPTURE_OUTPUT="1" if capture_output else "0",
                       TERMPLETE_SOCKET=self.home.name + "/daemon.sock")
            env.pop("TERMPLETE_CAPTURE_ACTIVE", None)
            env.pop("TERMPLETE_CAPTURE_WRAPPED", None)
            for key in ("TERM_PROGRAM", "WARP_IS_LOCAL_SHELL_SESSION", "KITTY_PID", "ALACRITTY_SOCKET", "HISTFILE", "TERMPLETE_HISTFILE"):
                env.pop(key, None)
            if terminal_app is not None:
                env["TERM_PROGRAM"] = terminal_app
            if no_color:
                env["NO_COLOR"] = "1"
            else:
                env.pop("NO_COLOR", None)
            env.pop("LINES", None)
            env.pop("COLUMNS", None)
            os.execve(executable, [executable] + args, env)
        self.resize(rows, cols)
        self.data = b""
        self.status = None

    def resize(self, rows, cols):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def send(self, text):
        # A terminal driver may flush bytes queued with SIGINT.
        if text.startswith(b"\x03") and len(text) > 1:
            start = self.send_menu(b"\x03")
            if self.is_zsh:
                self.read_until(b"\x1b[?2004h", after=start)
            text = text[1:]
        os.write(self.fd, text)

    def send_menu(self, text):
        # Drain pending redraws before marking the next cancellation boundary;
        # an unread old prompt must not satisfy the fresh-prompt wait below.
        while select.select([self.fd], [], [], 0)[0]:
            try:
                self.data += os.read(self.fd, 65536)
            except OSError as e:
                if e.errno != errno.EIO:
                    raise
                break
        start = len(self.data)
        self.send(text)
        deadline = time.monotonic() + .12
        while time.monotonic() < deadline:
            if select.select([self.fd], [], [], .005)[0]:
                try:
                    self.data += os.read(self.fd, 65536)
                except OSError as e:
                    if e.errno != errno.EIO:
                        raise
                    break
        return start

    def read_until(self, needle, timeout=4, after=0):
        def contains():
            # ZLE may redraw colored text one character at a time, resetting
            # colors between characters. Match visible text independently of
            # those style codes; keep escape-sequence assertions exact.
            data = self.data[after:]
            return needle in data or (b"\x1b" not in needle and needle in ANSI.sub(b"", data))
        deadline = time.monotonic() + timeout
        while not contains() and time.monotonic() < deadline:
            if select.select([self.fd], [], [], .05)[0]:
                try:
                    self.data += os.read(self.fd, 65536)
                except OSError as e:
                    if e.errno != errno.EIO:
                        raise
                    break
        if not contains():
            raise AssertionError(f"missing {needle!r}; terminal: {self.data!r}")
        return self.data

    def wait(self, timeout=4):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if select.select([self.fd], [], [], .01)[0]:
                try:
                    self.data += os.read(self.fd, 65536)
                except OSError as e:
                    if e.errno != errno.EIO:
                        raise
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = status
                return os.waitstatus_to_exitcode(status)
            time.sleep(.03)
        raise AssertionError(f"terminal process did not exit; terminal tail: {self.data[-1200:]!r}")

    def close(self):
        if self.status is None:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(self.pid, 0)
        # Stop only the daemon belonging to this isolated test home.
        try:
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(.5)
                connection.connect(self.home.name + "/daemon.sock")
                connection.sendall(b'{"op":"shutdown","id":1}\n')
                connection.recv(4096)
            deadline = time.monotonic() + 2
            while Path(self.home.name, "daemon.sock").exists() and time.monotonic() < deadline:
                time.sleep(.03)
            time.sleep(.05)
        except OSError:
            pass
        os.close(self.fd)
        # A detached record worker can finish daemon startup just as the shell
        # exits. Shut down that isolated daemon before retrying cleanup.
        for attempt in range(6):
            try:
                self.home.cleanup()
                break
            except OSError as error:
                if error.errno != errno.ENOTEMPTY or attempt == 5:
                    raise
                try:
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.settimeout(.2)
                        connection.connect(self.home.name + "/daemon.sock")
                        connection.sendall(b'{"op":"shutdown","id":1}\n')
                        connection.recv(4096)
                except OSError:
                    pass
                time.sleep(.15)


class TerminalTests(unittest.TestCase):
    def session(self, *args, **kwargs):
        s = Session(*args, **kwargs)
        self.addCleanup(s.close)
        return s

    def test_directory_completion_uses_current_folder_and_never_appends_invented_suffix(self):
        rc = ('cd "$HOME"\nmkdir -p nuxt-local-model other-project\nPROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
              '"${_TP_CANDIDATES[@]}" > "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                         config='[context]\nai_select_history = true\n')
        s.read_until(b"READY> ")
        snapshot = Path(s.home.name, "state")
        for prefix, expected in (("cd nuxt-lo", "cd nuxt-local-model/"),
                                 ("cd other-p", "cd other-project/")):
            s.send_menu(prefix.encode())
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                s.send_menu(b"\x18b")
                if expected in snapshot.read_text().splitlines()[2:]:
                    break
            state = snapshot.read_text().splitlines()
            self.assertEqual(state, [prefix, expected[len(prefix):], expected])
            s.send_menu(b"\x03")
        s.send_menu(b"cd nonexistent")
        time.sleep(.7)
        s.send_menu(b"\x18b")
        self.assertEqual(snapshot.read_text().splitlines(), ["cd nonexistent", ""])
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def assert_fresh_capture_startup(self, args):
        # Bound a regression to three startup attempts instead of spawning
        # wrappers forever and leaving the terminal blank.
        rc = ('export TP_TEST_STARTUPS=$(( ${TP_TEST_STARTUPS:-0} + 1 ))\n'
              'if (( TP_TEST_STARTUPS > 2 )); then print RECURSIVE_STARTUP; exit 70; fi\n'
              'PROMPT="FRESH_READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              'print -r -- "STARTUP_DIR:$PWD"\n')
        s = self.session(args, executable="/bin/zsh", rc=rc, capture_output=True)
        s.read_until(b"FRESH_READY> ", timeout=3)
        self.assertNotIn(b"RECURSIVE_STARTUP", s.data)
        self.assertIn(("STARTUP_DIR:" + os.getcwd()).encode(), s.data)
        s.data = b""
        s.send_menu(b"print -r -- STARTUP_COUNT:$TP_TEST_STARTUPS\r")
        s.read_until(b"STARTUP_COUNT:2")
        s.read_until(b"FRESH_READY> ")
        s.data = b""
        s.send_menu(b"/bin/sh -c 'test -z \"${TERMPLETE_CAPTURE_WRAPPED-}\" && printf \"GUARD_NOT_EXPORTED\\n\"'\r")
        s.read_until(b"\r\nGUARD_NOT_EXPORTED\r\n")
        s.read_until(b"FRESH_READY> ")
        s.send_menu(b"exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_fresh_zsh_capture_reaches_prompt_without_recursive_startup_or_directory_change(self):
        self.assert_fresh_capture_startup(["-i"])

    def test_fresh_login_zsh_capture_reaches_prompt_and_preserves_directory(self):
        self.assert_fresh_capture_startup(["-l", "-i"])

    def test_capture_preserves_default_native_history_and_live_import(self):
        for capture in (False, True):
            with self.subTest(capture=capture):
                # Like Oh My Zsh, keep HISTFILE if the system already chose it.
                # macOS initializes it from ZDOTDIR before reading .zshrc.
                rc = ('PROMPT="HISTORY_READY> "\nHISTSIZE=1000\nSAVEHIST=1000\n'
                      '[[ -n $HISTFILE ]] || HISTFILE="$HOME/.zsh_history"\n'
                      'setopt share_history\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$HISTFILE" '
                      '> "$HOME/state"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                                 capture_output=capture,
                                 config='provider="terminal-test"\nmode="on-demand"\n',
                                 history=': 10:0;echo SHARED_OLD_COMMAND\n')
                s.read_until(b"HISTORY_READY> ")
                s.send_menu(b"\x1b[A\x18b")
                history = Path(s.home.name, ".zsh_history")
                snapshot = Path(s.home.name, "state")
                self.assertEqual(snapshot.read_text().splitlines(),
                                 ["echo SHARED_OLD_COMMAND", str(history)])
                s.send_menu(b"\x03")
                # Simulate another terminal app appending to the shared file.
                with history.open("a") as file:
                    file.write(f': {int(time.time())}:0;echo FROM_OTHER_TERMINAL\n')
                s.data = b""
                s.send_menu(b":\r")
                s.read_until(b"HISTORY_READY> ")
                s.send_menu(b"\x1b[A\x1b[A\x18b")
                self.assertEqual(snapshot.read_text().splitlines(),
                                 ["echo FROM_OTHER_TERMINAL", str(history)])

    def test_native_output_capture_is_automatic_streams_immediately_and_hides_frames(self):
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, capture_output=True)
        s.read_until(b"READY> ")
        s.data = b""
        command = "printf 'CAPTURE_RESULT\\n'; sleep 1; printf 'CAPTURE_END\\n'"
        s.send((command + "\r").encode())
        s.read_until(b"\r\nCAPTURE_RESULT\r\n", timeout=.8)
        s.read_until(b"CAPTURE_END\r\n", timeout=3)
        s.read_until(b"READY> ")
        history = Path(s.home.name, "termplete/history.json")
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            if history.exists():
                entries = json.loads(history.read_text())
                if any(e.get("command") == command for e in entries):
                    break
            s.send_menu(b"")
            time.sleep(.05)
        entry = next(e for e in json.loads(history.read_text()) if e.get("command") == command)
        self.assertIn("CAPTURE_RESULT", entry["output"])
        self.assertIn("CAPTURE_END", entry["output"])
        self.assertEqual(entry["exit_code"], 0)
        self.assertNotIn(b"]777;termplete;", s.data)
        self.assertNotIn(b"[exit ", s.data)
        s.send_menu(b"exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_ai_completion_receives_previous_command_output_and_failure_status(self):
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                body = b'{"choices":[{"message":{"content":"tool --recover"}}]}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        config = ('provider = "local-test"\nmodel = "test-model"\nmode = "on-demand"\n'
                  '[context]\nimport_shell_history = false\n[[providers]]\n'
                  f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                  'no_auth = true\n')
        rc = ('cd "$HOME"\nmkdir context-folder\nPROMPT="READY> "\n'
              + f'eval "$(\"{BIN}\" init --print zsh)"\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config, capture_output=True)
        worker.start()
        s.read_until(b"READY> ")
        s.data = b""
        s.send_menu(b"printf 'NEXT_FLAG=--recover\\n'; false\r")
        s.read_until(b"\r\nNEXT_FLAG=--recover\r\n")
        s.read_until(b"READY> ")
        time.sleep(.3)
        s.data = b""
        s.send_menu(b"tool \x07")
        s.read_until(b"--recover")
        self.assertEqual(len(requests), 1)
        prompt = requests[0]["messages"][-1]["content"]
        self.assertIn("NEXT_FLAG=--recover", prompt)
        self.assertIn("[exit 1]", prompt)
        self.assertIn("context-folder", prompt)
        self.assertIn(s.home.name, prompt)
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_context_refinement_uses_omp_resume_output_and_defers_execution(self):
        resume = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        output = f"Resume this session with\n{resume}"
        for mode in ("automatic", "on-demand"):
            with self.subTest(mode=mode):
                requests = []

                class Handler(http.server.BaseHTTPRequestHandler):
                    def do_POST(self):
                        self.server.request_log.append(
                            json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                        body = json.dumps({"choices": [{"message": {
                            "content": f"omp update\n{resume}"}}]}).encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)

                    def log_message(self, *args):
                        pass

                server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
                server.request_log = requests
                self.addCleanup(server.server_close)
                self.addCleanup(server.shutdown)
                worker = threading.Thread(target=server.serve_forever, daemon=True)
                config = (f'provider = "local-test"\nmodel = "test-model"\nmode = "{mode}"\n'
                          '[context]\nimport_shell_history = true\n[[providers]]\n'
                          f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                          'no_auth = true\n[daemon]\nidle_shutdown_secs = 30\n')
                rc = ('cd "$HOME"\nPROMPT="READY> "\nHISTFILE="$HOME/.zsh_history"\n'
                      'omp() {\n'
                      '  print -r -- "$*" >> "$HOME/omp.calls"\n'
                      '  if [[ $1 == --resume ]]; then\n'
                      '    print -r -- "RESUME_EXECUTED"\n'
                      '  else\n'
                      '    print -r -- "Resume this session with"\n'
                      f'    print -r -- "{resume}"\n'
                      '  fi\n'
                      '}\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
                      '"${#_TP_CANDIDATES}" "${_TP_CANDIDATES[@]}" '
                      '"$_TP_MENU_OPEN" "$_TP_CANDIDATE_INDEX" '
                      '"$_TP_CONTEXT_BUFFER" > "$HOME/state"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config,
                                 history=": 10:0;omp update\n", capture_output=True)
                worker.start()
                s.read_until(b"READY> ")
                s.data = b""
                s.send_menu(b"omp\r")
                s.read_until(b"Resume this session with\r\n")
                s.read_until(resume.encode() + b"\r\n")
                s.read_until(b"READY> ")

                history = Path(s.home.name, "termplete/history.json")
                entries = json.loads(history.read_text())
                entry = next((item for item in entries if item.get("command") == "omp"), None)
                self.assertIsNotNone(entry)
                self.assertEqual(entry["output"][:2], output.splitlines())

                s.send_menu(b"omp")
                snapshot = Path(s.home.name, "state")

                def wait_for_candidates(expected):
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        s.send_menu(b"\x18b")
                        time.sleep(.05)
                        state = snapshot.read_text().splitlines()
                        count = int(state[2]) if len(state) > 2 else 0
                        if len(state) >= 3 + count and expected in state[3:3 + count]:
                            return state
                    self.fail(f"timed out waiting for {expected!r}; state={state!r}; requests={requests!r}")

                def wait_for_model_result():
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        s.send_menu(b"\x18b")
                        time.sleep(.05)
                        state = snapshot.read_text().splitlines()
                        count = int(state[2]) if len(state) > 2 else 0
                        candidates = state[3:3 + count]
                        if len(requests) and "omp update" in candidates and resume in candidates:
                            return state
                    self.fail(f"timed out waiting for model candidates; state={state!r}; requests={requests!r}")

                state = wait_for_candidates("omp update")
                if mode == "on-demand":
                    self.assertEqual(requests, [])
                    self.assertEqual(state[5 + int(state[2])], "omp", msg=f"state={state!r}")
                    s.send_menu(b"\x07")
                state = wait_for_model_result()
                self.assertEqual(state[0], "omp")
                self.assertGreaterEqual(len(requests), 1)
                prompts = ["\n".join(message["content"] for message in request["messages"]
                                       if isinstance(message.get("content"), str))
                           for request in requests]
                self.assertTrue(any(output in prompt for prompt in prompts), msg=prompts)

                # Open the selector, choose the returned resume command, and
                # verify selector Enter only edits the buffer.
                s.send_menu(b"\t\t\x18b")
                time.sleep(.05)
                state = snapshot.read_text().splitlines()
                count = int(state[2])
                for _ in range(count):
                    candidates = state[3:3 + count]
                    selected = int(state[4 + count])
                    if candidates[selected - 1] == resume:
                        break
                    s.send_menu(b"\t\x18b")
                    time.sleep(.05)
                    state = snapshot.read_text().splitlines()
                candidates = state[3:3 + count]
                selected = int(state[4 + count])
                self.assertEqual(candidates[selected - 1], resume, msg=f"state={state!r}")
                s.send_menu(b"\r\x18b")
                time.sleep(.05)
                state = snapshot.read_text().splitlines()
                self.assertEqual(state[0], resume)
                self.assertEqual(Path(s.home.name, "omp.calls").read_text().splitlines(), [""])
                self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))

                start = s.send_menu(b"\r")
                s.read_until(b"RESUME_EXECUTED", after=start)
                s.read_until(b"\x1b[?2004h", after=start)
                self.assertEqual(Path(s.home.name, "omp.calls").read_text().splitlines(), ["", "--resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"])
                s.send_menu(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)
                server.shutdown()
                server.server_close()
                worker.join(timeout=1)

    def test_native_ssh_history_is_fast_ranked_and_can_cycle_without_execution(self):
        history = ': 10:0;ssh root@203.0.113.7\n' * 8 + ': 20:0;ssh test@203.0.113.9\n'
        rc = ('PROMPT="READY> "\nHISTFILE="$HOME/.zsh_history"\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -r -- "$BUFFER" > "$HOME/buffer"; '
              'print -r -- "$_TP_GHOST" >> "$HOME/buffer"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        config = ('provider = "terminal-test"\n[modes]\naggressive_debounce_ms = 2000\n'
                  '[daemon]\nidle_shutdown_secs = 30\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config, history=history)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        s.data = b""
        s.send_menu(b"ssh ")
        # A 2-second AI delay must not delay exact local history.
        s.read_until(b"root@203.0.113.7", timeout=.8)
        s.send_menu(b"\x18b")
        time.sleep(.08)
        self.assertEqual((Path(s.home.name) / "buffer").read_text(), "ssh \nroot@203.0.113.7\n")
        s.send_menu(b"\t\t\x18b")
        time.sleep(.08)
        self.assertEqual((Path(s.home.name) / "buffer").read_text(), "ssh \ntest@203.0.113.9\n")
        s.send_menu(b"\x1b[C\x18b")
        time.sleep(.08)
        self.assertEqual((Path(s.home.name) / "buffer").read_text(), "ssh test@203.0.113.9\n\n")
        self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_short_command_executes_full_name_and_preserves_cli_arguments(self):
        s = self.session(["shell", "--shell", "/bin/bash", "-c",
                          "printf 'IDENTITY_READY'; sleep 1"])
        s.read_until(b"IDENTITY_READY", timeout=.8)
        command = subprocess.check_output(["/bin/ps", "-p", str(s.pid), "-o", "comm="], text=True).strip()
        self.assertEqual(Path(command).name, "termplete")
        self.assertEqual(s.wait(), 0)
        full_name = str(Path(BIN).with_name("termplete"))
        for args in (["--help"], ["unknown-command"]):
            short = subprocess.run([BIN] + args, capture_output=True)
            full = subprocess.run([full_name] + args, capture_output=True)
            self.assertEqual((short.returncode, short.stdout, short.stderr),
                             (full.returncode, full.stdout, full.stderr))

    def test_home_config_migration_and_explicit_directory_overrides(self):
        with tempfile.TemporaryDirectory(prefix="tp-config-migration-") as home:
            root = Path(home)
            legacy = root / ("Library/Application Support/termplete" if sys.platform == "darwin"
                             else ".config/termplete")
            legacy.mkdir(parents=True)
            files = {"config.toml": 'disabled_terminals = ["warp"]\n',
                     "daemon.log": "previous logs\n", "history.json": "[]"}
            for name, value in files.items():
                (legacy / name).write_text(value)
            env = dict(os.environ, HOME=home, TERMPLETE_SOCKET=str(root / "daemon.sock"))
            env.pop("TERMPLETE_HOME", None)
            env.pop("XDG_CONFIG_HOME", None)
            result = subprocess.run([BIN, "config", "path"], env=env,
                                    capture_output=True, check=True)
            self.assertEqual(result.stdout.decode().strip(), str(root / ".termplete/config.toml"))
            self.assertFalse(legacy.exists())
            for name, value in files.items():
                self.assertEqual((root / ".termplete" / name).read_text(), value)
            for variable in ("TERMPLETE_HOME", "XDG_CONFIG_HOME"):
                override = root / variable.lower()
                result = subprocess.run([BIN, "config", "path"], env=dict(env, **{variable: str(override)}),
                                        capture_output=True, check=True)
                self.assertEqual(result.stdout.decode().strip(), str(override / "termplete/config.toml"))

    def test_excluded_terminal_guards_cli_commands_and_init_output(self):
        with tempfile.TemporaryDirectory(prefix="tp-disabled-terminal-") as home:
            config_dir = Path(home) / "termplete"
            config_dir.mkdir()
            (config_dir / "config.toml").write_text(
                'provider = "local-test"\nmodel = "test-model"\n'
                'disabled_terminals = ["warp"]\n'
                '[[providers]]\nname = "local-test"\n'
                'base_url = "http://127.0.0.1:1/v1"\nno_auth = true\n'
                '[daemon]\nidle_shutdown_secs = 30\n'
            )
            env = dict(os.environ, HOME=home, TERMPLETE_HOME=home,
                       TERMPLETE_SOCKET=str(Path(home) / "daemon.sock"))
            for key in ("TERM_PROGRAM", "WARP_IS_LOCAL_SHELL_SESSION", "KITTY_PID", "ALACRITTY_SOCKET"):
                env.pop(key, None)
            env["TERM_PROGRAM"] = "WarpTerminal"

            for shell in ("zsh", "bash"):
                with self.subTest(shell=shell):
                    result = subprocess.run([BIN, "init", "--print", shell], env=env,
                                            capture_output=True, check=True)
                    self.assertEqual(result.stdout, b"")

            suggest = subprocess.run([BIN, "suggest", "--json", "--", "git status"], env=env,
                                     capture_output=True, check=True)
            self.assertEqual(json.loads(suggest.stdout)["source"], "disabled")
            record = subprocess.run([BIN, "record", "--", "git status"], env=env,
                                    capture_output=True, check=True)
            self.assertEqual(record.returncode, 0)
            shell = subprocess.run([BIN, "shell", "--shell", "/bin/bash", "-c",
                                    "printf 'ORDINARY_OUTPUT'; exit 7"], env=env,
                                   capture_output=True, timeout=2)
            self.assertEqual(shell.stdout, b"ORDINARY_OUTPUT")
            self.assertEqual(shell.returncode, 7)
            self.assertFalse(Path(env["TERMPLETE_SOCKET"]).exists())
            self.assertFalse(any("history" in path.name.lower()
                                 for path in Path(home).rglob("*") if path.is_file()))

            enabled_env = dict(env, TERM_PROGRAM="iTerm.app")
            enabled = subprocess.run([BIN, "init", "--print", "zsh"], env=enabled_env,
                                     capture_output=True, check=True)
            self.assertTrue(enabled.stdout.strip())

            local_shell_env = dict(env)
            local_shell_env.pop("TERM_PROGRAM", None)
            local_shell_env["WARP_IS_LOCAL_SHELL_SESSION"] = "1"
            local_shell = subprocess.run([BIN, "init", "--print", "zsh"], env=local_shell_env,
                                         capture_output=True, check=True)
            self.assertEqual(local_shell.stdout, b"")

    def test_zsh_direct_plugin_source_preserves_tab_and_ctrl_g_when_excluded(self):
        home_config = ('provider = "local-test"\nmodel = "test-model"\n'
                       'disabled_terminals = ["warp"]\n')
        plugin = Path(__file__).resolve().parents[1] / "shells/termplete.plugin.zsh"
        rc = ("PROMPT='READY> '\nHISTFILE=/dev/null\n"
              '_normal_tab() { LBUFFER+=T; }\n'
              '_normal_ctrl_g() { LBUFFER+=G; }\n'
              "zle -N _normal_tab; bindkey '^I' _normal_tab\n"
              "zle -N _normal_ctrl_g; bindkey '^G' _normal_ctrl_g\n"
              f'source "{plugin}"\n'
              'print -r -- "${+_TERMPLETE_ZSH_LOADED}" > "$HOME/plugin-loaded"\n'
              'print -r -- "${+functions[_tp_line_init]}" > "$HOME/plugin-hook"\n'
              '_tp_snapshot() { print -r -- "$BUFFER" > "$HOME/buffer"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                         config=home_config, terminal_app="WarpTerminal")
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        s.send(b"x\t\x18b")
        time.sleep(.1)
        self.assertEqual((Path(s.home.name) / "buffer").read_text(), "xT\n")
        s.send(b"\x03")
        s.data = b""
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        s.send(b"x\x07\x18b")
        time.sleep(.1)
        self.assertEqual((Path(s.home.name) / "buffer").read_text(), "xG\n")
        self.assertEqual((Path(s.home.name) / "plugin-loaded").read_text(), "0\n")
        self.assertEqual((Path(s.home.name) / "plugin-hook").read_text(), "0\n")
        s.send(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)
        self.assertFalse(Path(s.home.name, "daemon.sock").exists())
        self.assertFalse(any("history" in path.name.lower()
                             for path in Path(s.home.name).rglob("*") if path.is_file()))

    def test_bash_direct_plugin_source_returns_before_installing_when_excluded(self):
        config = 'provider = "local-test"\ndisabled_terminals = ["warp"]\n'
        plugin = Path(__file__).resolve().parents[1] / "shells/termplete.plugin.bash"
        rc = f"""PS1='READY> '
HISTFILE=/dev/null
bind '\"\\C-i\": \"T\"'
source "{plugin}"
declare -F _tp_complete > "$HOME/plugin-function"
"""
        s = self.session(["--noprofile", "-i"], executable="/bin/bash", rc=rc,
                         config=config, terminal_app="WarpTerminal")
        s.read_until(b"READY> ")
        self.assertEqual((Path(s.home.name) / "plugin-function").read_text(), "")
        # A custom Tab binding still edits the buffer, producing xT as a command.
        s.send(b"x\t\r")
        s.read_until(b"xT: command not found")
        s.send(b"exit 0\r")
        self.assertEqual(s.wait(), 0)
        self.assertFalse(Path(s.home.name, "daemon.sock").exists())

    def test_bash_records_starting_directory_without_recounting_empty_lines(self):
        rc = ('cd "$HOME"\nmkdir project\nPS1="READY> "\n'
              f'eval "$(\"{BIN}\" init --print bash)"\n')
        config = 'mode = "off"\n[context]\nimport_shell_history = false\n'
        s = self.session(["--noprofile", "-i"], executable="/bin/bash", rc=rc, config=config)
        s.read_until(b"READY> ")
        start = len(s.data)
        s.send_menu(b"cd project\r")
        s.read_until(b"READY> ", after=start)
        memory_path = Path(s.home.name, "termplete", "commands.json")
        destination = str(Path(s.home.name, "project").resolve())
        deadline = time.monotonic() + 4
        memory = {}
        while time.monotonic() < deadline:
            if memory_path.exists():
                memory = json.loads(memory_path.read_text())
                if memory.get("directories", {}).get(destination, {}).get("uses") == 1:
                    break
            time.sleep(.03)
        self.assertEqual(memory["directories"][destination]["uses"], 1)
        history = json.loads(Path(s.home.name, "termplete", "history.json").read_text())
        entry = next(entry for entry in history if entry["command"] == "cd project")
        self.assertEqual(entry["cwd"], str(Path(s.home.name).resolve()))
        s.send_menu(b"\r")
        time.sleep(.15)
        self.assertEqual(json.loads(memory_path.read_text())["directories"][destination]["uses"], 1)
        s.send_menu(b"exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_terminal_app_picker_saves_warp_exclusion(self):
        s = self.session(["settings"])
        s.read_until(b"1/6")
        s.data = b""
        s.send(b"\x1b[B\x1b[B\x1b[B\r")
        s.read_until(b"Terminal apps:")
        s.data = b""
        s.send(b"\r")
        s.read_until(b"Warp  [on]")
        s.data = b""
        s.send(b"\r")
        s.read_until(b"Warp  [off]")
        s.data = b""
        s.send(b"q")
        s.read_until(b"Terminal apps: 1 disabled")
        s.data = b""
        s.send(b"q")
        s.read_until(b"1/6")
        s.data = b""
        s.send(b"\x1b[F\r")
        self.assertEqual(s.wait(), 0)
        saved = (Path(s.home.name) / "termplete/config.toml").read_text()
        self.assertIn('disabled_terminals = ["warp"]', saved)

    def test_global_enable_setting_can_pause_and_resume_termplete(self):
        for initially_on in (True, False):
            with self.subTest(initially_on=initially_on):
                config = "enabled = " + str(initially_on).lower() + "\n"
                s = self.session(["settings"], config=config)
                s.read_until(b"1/6")
                s.send(b"\x1b[B" * 3 + b"\r")
                s.read_until(b"1/10")
                s.data = b""
                s.send(b"\x1b[B\r")
                s.read_until(b"1/2" if initially_on else b"2/2")
                s.send(b"\x1b[B\r" if initially_on else b"\x1b[A\r")
                s.read_until(b"TermPlete enabled: " + (b"off" if initially_on else b"on"))
                s.send(b"q")
                s.read_until(b"1/6")
                s.send(b"\x1b[F\r")
                self.assertEqual(s.wait(), 0)
                saved = Path(s.home.name, "termplete/config.toml").read_text()
                self.assertIn("enabled = " + str(not initially_on).lower(), saved)

    def test_history_picker_can_save_optional_ai_selection(self):
        s = self.session(["settings"])
        s.read_until(b"1/6")
        s.data = b""
        s.send(b"\x1b[B\x1b[B\x1b[B\r")
        s.read_until(b"1/10")
        s.read_until(b"History matches: Instant")
        s.data = b""
        s.send(b"\x1b[B\x1b[B\r")
        s.read_until(b"Instant local history")
        s.data = b""
        s.send(b"\x1b[B\r")
        s.read_until(b"History matches: AI choice")
        s.data = b""
        s.send(b"q")
        s.read_until(b"1/6")
        s.send(b"\x1b[F\r")
        self.assertEqual(s.wait(), 0)
        saved = (Path(s.home.name) / "termplete/config.toml").read_text()
        self.assertIn('ai_select_history = true', saved)

    def test_output_capture_picker_explains_and_saves_on_and_off(self):
        for terminal in ("xterm-256color", "dumb"):
            for initially_on in (True, False):
                with self.subTest(terminal=terminal, initially_on=initially_on):
                    config = ('[context]\ncapture_output = '
                              + str(initially_on).lower() + '\n')
                    s = self.session(["settings"], terminal=terminal, config=config)
                    interactive = terminal != "dumb"
                    s.read_until(b"1/6" if interactive else b"Choose 1-6")
                    s.data = b""
                    s.send(b"\x1b[B" * 3 + b"\r" if interactive else b"4\n")
                    s.read_until(b"1/10" if interactive else b"Choose 1-10")
                    s.data = b""
                    s.send(b"\x1b[B" * 4 + b"\r" if interactive else b"5\n")
                    s.read_until(b"1/2" if initially_on and interactive else
                                 b"2/2" if interactive else b"Choose 1-2")
                    self.assertIn(b"saved locally and may be sent to your AI provider", s.data)
                    self.assertIn(b"Applies to new terminal sessions", s.data)
                    visible = ANSI.sub(b"", s.data)
                    self.assertIn(b"On", visible)
                    self.assertIn(b"Off", visible)
                    self.assertNotIn(b"capture_output [", visible)
                    # Cancelling leaves the current choice unchanged.
                    s.data = b""
                    s.send(b"q" if interactive else b"q\n")
                    s.read_until(b"Capture terminal output: "
                                 + (b"on" if initially_on else b"off"))
                    s.read_until(b"1/10" if interactive else b"Choose 1-10")
                    s.data = b""
                    s.send(b"\x1b[B" * 4 + b"\r" if interactive else b"5\n")
                    s.read_until(b"1/2" if initially_on and interactive else
                                 b"2/2" if interactive else b"Choose 1-2")
                    s.data = b""
                    s.send(b"\x1b[B\r" if interactive else
                           b"2\n" if initially_on else b"1\n")
                    s.read_until(b"Capture terminal output: "
                                 + (b"off" if initially_on else b"on"))
                    s.read_until(b"1/10" if interactive else b"Choose 1-10")
                    s.data = b""
                    s.send(b"q" if interactive else b"10\n")
                    s.read_until(b"1/6" if interactive else b"Choose 1-6")
                    s.send(b"\x1b[F\r" if interactive else b"6\n")
                    self.assertEqual(s.wait(), 0)
                    self.assertIn(b"Open a new terminal session", s.data)
                    saved = Path(s.home.name, "termplete/config.toml").read_text()
                    self.assertIn('capture_output = ' + str(not initially_on).lower(), saved)

    def test_settings_text_input_displays_default_once_and_keeps_it_on_enter(self):
        s = self.session(["settings"], config='[modes]\nmin_chars = 4\n')
        s.read_until(b"1/6")
        s.data = b""
        s.send(b"\x1b[B" * 3 + b"\r")
        s.read_until(b"1/10")
        s.data = b""
        s.send(b"\x1b[B" * 5 + b"\r")
        s.read_until(b"min_chars [4]:")
        visible = ANSI.sub(b"", s.data)
        self.assertEqual(visible.count(b"[4]"), 1)
        s.data = b""
        s.send(b"\r")
        s.read_until(b"1/10")
        s.data = b""
        s.send(b"q")
        s.read_until(b"1/6")
        s.send(b"\x1b[F\r")
        self.assertEqual(s.wait(), 0)
        saved = Path(s.home.name, "termplete/config.toml").read_text()
        self.assertIn('min_chars = 4', saved)

    def choose_custom_provider_in_settings(self, s, base_url, name, model):
        s.read_until(b"Choose 1-6")
        s.data = b""
        s.send(b"2\n")
        s.read_until(b"(q to cancel): ")
        s.data = b""
        s.send(b"1\n")
        s.read_until(b"Custom OpenAI-compatible provider")
        s.read_until(b"(q to cancel): ", after=s.data.index(b"Custom OpenAI-compatible provider"))
        self.assertIn(b"Custom OpenAI-compatible provider", s.data)
        for removed in (b"llamacpp", b"perplexity", b"cerebras", b"gw"):
            self.assertNotIn(removed, s.data)
        last = re.findall(rb"Choose 1-(\d+)", s.data)[-1]
        for answer, prompt in ((last + b"\n", b"base URL (e.g."),
                               (base_url + b"\n", b"provider name (e.g."),
                               (name + b"\n", b"Environment variable for the key"),
                               (b"\n", b"Type the model id")):
            s.data = b""
            s.send(answer)
            s.read_until(prompt)
        s.data = b""
        s.send(model + b"\n")

    def test_settings_provider_switch_keeps_models_across_saved_sessions(self):
        config = ('provider="aaa-first"\nmodel="first-selected"\n'
                  '[[providers]]\nname="aaa-first"\nbase_url="http://127.0.0.1:9998/v1"\n'
                  'no_auth=true\n[[providers]]\nname="aaa-second"\n'
                  'base_url="http://127.0.0.1:9999/v1"\nno_auth=true\n')
        s = self.session(["settings"], terminal="dumb", config=config)
        s.read_until(b"Choose 1-6")
        s.data = b""
        s.send(b"2\n")
        s.read_until(b"Choose 1-4")
        s.data = b""
        s.send(b"1\n")
        s.read_until(b"(q to cancel): ")
        s.data = b""
        s.send(b"2\n")  # The two custom providers sort before the built-ins.
        s.read_until(b"Provider: aaa-second")
        s.read_until(b"Choose 1-4")
        s.data = b""
        s.send(b"2\n")
        s.read_until(b"model:")
        s.data = b""
        s.send(b"second-selected\n")
        s.read_until(b"Model: second-selected")
        s.send(b"q\n")
        s.read_until(b"Choose 1-6")
        s.send(b"6\n")
        self.assertEqual(s.wait(), 0)
        saved = Path(s.home.name, "termplete/config.toml").read_text()

        # A new Settings process must restore the first provider's selection.
        s = self.session(["settings"], terminal="dumb", config=saved)
        s.read_until(b"Choose 1-6")
        s.data = b""
        s.send(b"2\n")
        s.read_until(b"Choose 1-4")
        s.data = b""
        s.send(b"1\n")
        s.read_until(b"(q to cancel): ")
        s.data = b""
        s.send(b"1\n")
        s.read_until(b"Provider: aaa-first")
        s.read_until(b"Model: first-selected")
        s.send(b"q\n")
        s.read_until(b"Choose 1-6")
        s.send(b"6\n")
        self.assertEqual(s.wait(), 0)
        saved = Path(s.home.name, "termplete/config.toml").read_text()
        self.assertIn('provider = "aaa-first"', saved)
        self.assertIn('model = "first-selected"', saved)
        self.assertIn('aaa-second = "second-selected"', saved)

    def test_settings_adds_custom_openai_endpoint_and_saves_local_model(self):
        s = self.session(["settings"], terminal="dumb")
        self.choose_custom_provider_in_settings(s, b"http://127.0.0.1:9999/v1", b"my-local", b"my-model")
        s.read_until(b"Custom provider configured")
        self.assertIn(b"no API key required", s.data)
        s.data = b""
        s.send(b"q\n")
        s.read_until(b"Choose 1-6")
        s.send(b"6\n")
        self.assertEqual(s.wait(), 0)
        saved = Path(s.home.name, "termplete/config.toml").read_text()
        self.assertIn('provider = "my-local"', saved)
        self.assertIn('model = "my-model"', saved)
        self.assertIn('name = "my-local"', saved)
        self.assertIn('base_url = "http://127.0.0.1:9999/v1"', saved)
        self.assertIn('wire = "openai-compat"', saved)
        self.assertIn('no_auth = true', saved)

    def test_settings_remote_custom_key_is_masked_and_discarded_with_draft(self):
        s = self.session(["settings"], terminal="dumb")
        config_path = Path(s.home.name, "termplete/config.toml")
        original = config_path.read_text()
        self.choose_custom_provider_in_settings(s, b"https://example.invalid/v1", b"my-remote", b"my-model")
        s.read_until(b"Input is masked")
        s.data = b""
        s.send_menu(b"synthetic-custom-api-key\r")
        s.read_until(b"Custom provider configured")
        self.assertIn(b"API key received", s.data)
        self.assertNotIn(b"synthetic-custom-api-key", s.data)
        self.assertEqual(config_path.read_text(), original)
        s.data = b""
        s.send(b"q\n")
        s.read_until(b"Choose 1-6")
        s.send(b"q\n")
        self.assertEqual(s.wait(), 0)
        self.assertEqual(config_path.read_text(), original)
        self.assertNotIn(b"Stored the API key", s.data)

    def test_decision_settings_are_reachable_and_save_strategy_protocol_and_privacy(self):
        s = self.session(["settings"], terminal="dumb")
        s.read_until(b"Choose 1-6")
        s.data = b""
        s.send(b"2\n")
        s.read_until(b"Choose 1-4")
        s.data = b""
        s.send(b"3\n")
        s.read_until(b"Choose 1-10")
        for field, choice, title in ((b"1\n", b"3\n", b"Decision strategy"),
                                     (b"2\n", b"2\n", b"Decision request protocol"),
                                     (b"7\n", b"2\n", b"Private context")):
            s.data = b""
            s.send(field)
            s.read_until(title)
            s.data = b""
            s.send(choice)
            s.read_until(b"Choose 1-10")
        s.data = b""
        s.send(b"10\n")
        s.read_until(b"Choose 1-4")
        s.data = b""
        s.send(b"4\n")
        s.read_until(b"Choose 1-6")
        s.send(b"6\n")
        self.assertEqual(s.wait(), 0)
        saved = Path(s.home.name, "termplete/config.toml").read_text()
        self.assertIn('strategy = "hybrid"', saved)
        self.assertIn('protocol = "system-one"', saved)
        self.assertIn('local_only = false', saved)

    def test_decision_switch_disables_without_discarding_the_model_or_strategy(self):
        config = '[decision]\nstrategy = "decision"\nmodel = "retained-model"\n'
        s = self.session(["settings"], terminal="dumb", config=config)
        s.read_until(b"Choose 1-6")
        s.data = b""
        s.send(b"2\n")
        s.read_until(b"Choose 1-4")
        s.data = b""
        s.send(b"3\n")
        s.read_until(b"Choose 1-10")
        s.data = b""
        s.send(b"9\n")
        s.read_until(b"Use decision helper")
        s.data = b""
        s.send(b"2\n")
        s.read_until(b"Use decision helper: off")
        s.data = b""
        s.send(b"10\n")
        s.read_until(b"Choose 1-4")
        s.data = b""
        s.send(b"4\n")
        s.read_until(b"Choose 1-6")
        s.send(b"6\n")
        self.assertEqual(s.wait(), 0)
        saved = Path(s.home.name, "termplete/config.toml").read_text()
        self.assertIn('enabled = false', saved)
        self.assertIn('strategy = "decision"', saved)
        self.assertIn('model = "retained-model"', saved)

    def test_sensitive_context_picker_explains_fixed_exclusions_and_saves_local_review(self):
        s = self.session(["settings"], terminal="dumb")

        def select_item(label):
            match = re.search(
                rb"(?m)^[ \t]*(\d+)\. " + re.escape(label) + rb"\r?$",
                s.data,
            )
            self.assertIsNotNone(match, f"missing {label!r}; terminal: {s.data!r}")
            s.data = b""
            s.send(match.group(1) + b"\n")

        s.read_until(b"Choose 1-6")
        s.data = b""
        s.send(b"4\n")
        s.read_until(b"Sensitive context: Block")
        s.read_until(b"(q to cancel): ")
        select_item(b"Sensitive context: Block")
        s.read_until(b"Choose 1-2")
        self.assertIn(b"always stay blocked", s.data)
        self.assertIn(b"remote prompts remain filtered", s.data)
        s.data = b""
        s.send(b"2\n")
        s.read_until(b"Sensitive context: Review with a local decision model")
        # The updated row can arrive before the remaining Preferences rows on a busy PTY.
        s.read_until(b"Choose 1-10 (q to cancel): ")
        select_item(b"Back")
        s.read_until(b"Choose 1-6")
        s.send(b"6\n")
        self.assertEqual(s.wait(), 0)
        saved = Path(s.home.name, "termplete/config.toml").read_text()
        self.assertIn('[privacy]', saved)
        self.assertIn('sensitive_context = "decision"', saved)

    def test_zsh_decision_strategy_ranks_options_instead_of_short_circuiting_to_local(self):
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append((self.path, request))
                answers = {
                    "share_remote": {"type": "noul", "noul": .99},
                    "omit_context": {"type": "noul", "noul": .01},
                }
                if "best_command" in request["questions"]:
                    answers["best_command"] = {
                        "type": "choice", "choice": "1", "confidence": .9,
                        "probabilities": {"0": .1, "1": .85, "2": .04, "none": .01},
                    }
                body = json.dumps({"answers": answers}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        config = ('[decision]\nstrategy = "decision"\nprovider = "local-test"\nmodel = "test"\n'
                  '[[providers]]\nname = "local-test"\nwire = "jev"\nno_auth = true\n'
                  f'base_url = "http://127.0.0.1:{server.server_port}/v1"\n')
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
              '"${#_TP_CANDIDATES}" > "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", config=config, rc=rc)
        worker.start()
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name, "state")
        s.send_menu(b"docker")
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            if snapshot.exists() and snapshot.read_text().splitlines()[1:2] == [" images"]:
                break
            time.sleep(.05)
        self.assertEqual(snapshot.read_text().splitlines(), ["docker", " images", "3"],
                         f"decision requests: {requests}; log: {Path(s.home.name, 'termplete/daemon.log').read_text()}")
        request_count = len(requests)
        self.assertGreater(request_count, 0)
        self.assertEqual(requests[-1][0], "/v1/systemone")
        self.assertIn("best_command", requests[-1][1]["questions"])
        s.send_menu(b"\t\t\x1b[A\x1b[B")
        self.assertEqual(len(requests), request_count)
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_appearance_settings_are_reachable_and_save_animation_and_colors(self):
        s = self.session(["settings"], terminal="dumb")
        s.read_until(b"Choose 1-6")
        s.data = b""
        s.send(b"5\n")
        s.read_until(b"Appearance")
        for index, prompt, value in ((1, b"animations: ", b"off"),
                                     (2, b"ghost_color: ", b"#808080"),
                                     (3, b"automatic_color: ", b"#00ff00")):
            s.data = b""
            s.send(str(index).encode() + b"\n")
            s.read_until(prompt)
            s.data = b""
            s.send(value + b"\n")
            s.read_until(b"Choose 1-4")
        s.data = b""
        s.send(b"4\n")
        s.read_until(b"Settings")
        s.send(b"6\n")
        self.assertEqual(s.wait(), 0)
        saved = Path(s.home.name, "termplete/config.toml").read_text()
        self.assertIn('animations = false', saved)
        self.assertIn('ghost_color = "#808080"', saved)
        self.assertIn('automatic_color = "#00ff00"', saved)

    def test_on_demand_bare_tool_refines_from_context_once_after_reveal(self):
        for reveal in (b"\x07", b"\t"):
            with self.subTest(reveal=reveal):
                requests = []

                class Handler(http.server.BaseHTTPRequestHandler):
                    def do_POST(self, requests=requests):
                        requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                        body = b'{"choices":[{"message":{"content":"ffmpeg -i clip.webm preview.mp4"}}]}'
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)

                    def log_message(self, *args):
                        pass

                server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
                self.addCleanup(server.server_close)
                self.addCleanup(server.shutdown)
                config = ('provider = "local-test"\nmodel = "test-model"\nmode = "on-demand"\n'
                          '[[providers]]\nname = "local-test"\n'
                          f'base_url = "http://127.0.0.1:{server.server_port}/v1"\nno_auth = true\n')
                rc = ('cd "$HOME"\ntouch clip.webm\nPROMPT="READY> "\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "${#_TP_CANDIDATES}" '
                      '"${_TP_CANDIDATES[@]}" "$_TP_CANDIDATE_INDEX" '
                      '"$_TP_SELECTION_EXPLICIT" "$_TP_FULL" > "$HOME/state"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
                threading.Thread(target=server.serve_forever, daemon=True).start()
                s.read_until(b"READY> ")
                s.send_menu(b"ffmpeg")
                snapshot = Path(s.home.name, "state")
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    s.send_menu(b"\x18b")
                    if snapshot.read_text().splitlines()[1] == "3":
                        break
                self.assertEqual(requests, [])
                s.data = b""
                s.send_menu(reveal)
                s.read_until(b"clip.webm")
                s.send_menu(b"\x18b")
                contextual = snapshot.read_text().splitlines()
                self.assertEqual(contextual[2], "ffmpeg -i clip.webm preview.mp4")
                self.assertEqual(contextual[-3:], ["1", "0", "ffmpeg -i clip.webm preview.mp4"])
                self.assertEqual(len(requests), 1)
                self.assertIn("clip.webm", requests[0]["messages"][-1]["content"])
                s.send_menu(b"\t\x1b[B\x1b[A\x07\x18b")
                self.assertEqual(len(requests), 1)
                self.assertEqual(snapshot.read_text().splitlines()[0], "ffmpeg")
                s.send_menu(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)
            self.doCleanups()

    def test_help_lists_all_commands_and_options_without_terminal_escapes(self):
        result = subprocess.run([BIN, "--help"], capture_output=True, check=True)
        help_text = result.stdout.decode()
        commands = (
            "init", "auth", "setup", "settings", "stop", "start", "service",
            "suggest", "daemon", "shell", "context", "status", "record",
            "feedback", "frame", "config", "decision",
        )
        for command in commands:
            with self.subTest(command=command):
                self.assertRegex(help_text, rf"(?m)^  {command}\s")

        help_outputs = [result.stdout + result.stderr]
        for command, option in (("record", "--pending"), ("suggest", "--shell-reply")):
            command_help = subprocess.run(
                [BIN, command, "--help"], capture_output=True, check=True
            )
            help_outputs.append(command_help.stdout + command_help.stderr)
            self.assertIn(option.encode(), command_help.stdout)
        for output in help_outputs:
            self.assertNotIn(b"\x1b", output)

    def test_capture_forwarding_respects_configured_connection_timeout(self):
        config = ('[context]\nimport_shell_history = false\n'
                  '[daemon]\nautostart = false\nconnect_timeout_ms = 2000\n')
        s = self.session(["shell", "--shell", "/bin/zsh", "--command",
                          "sleep .5; printf 'SLOW_CAPTURE_ACK\\n'"], config=config)
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(Path(s.home.name, "daemon.sock")))
        listener.listen(4)
        listener.settimeout(5)
        self.addCleanup(listener.close)
        acknowledged = []

        def respond():
            for _ in range(2):
                connection, _ = listener.accept()
                with connection:
                    data = bytearray()
                    while not data.endswith(b"\n"):
                        data.extend(connection.recv(65536))
                    request = json.loads(data)
                    if request["op"] == "record":
                        time.sleep(.8)  # Longer than the old hardcoded 500 ms.
                    try:
                        connection.sendall(json.dumps({"op": "ok", "id": request["id"]}).encode() + b"\n")
                        acknowledged.append(request)
                    except BrokenPipeError:
                        pass

        worker = threading.Thread(target=respond, daemon=True)
        worker.start()
        s.read_until(b"SLOW_CAPTURE_ACK")
        self.assertEqual(s.wait(), 0)
        worker.join(timeout=3)
        records = [request for request in acknowledged if request["op"] == "record"]
        self.assertEqual(len(records), 1, acknowledged)
        self.assertIn("SLOW_CAPTURE_ACK", records[0]["output"])

    def test_one_shot_streams_before_exit_and_exits_without_input(self):
        s = self.session(["shell", "--shell", "/bin/bash", "-c",
                          "printf 'STREAM_'; printf 'NOW\\n'; sleep 1; printf 'FINISHED\\n'"])
        s.read_until(b"STREAM_NOW", timeout=.8)
        self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
        s.read_until(b"FINISHED\r\n")
        self.assertEqual(s.wait(), 0)
        # The outer terminal is restored even though stdin is still open.
        flags = termios.tcgetattr(s.fd)[3]
        self.assertTrue(flags & termios.ICANON)
        self.assertTrue(flags & termios.ECHO)

    def test_shell_output_interrupt_resize_and_exit(self):
        for shell in ("/bin/bash", "/bin/zsh"):
            with self.subTest(shell=shell):
                s = self.session(["shell", "--shell", shell], rc='PS1="READY> "\nPROMPT="READY> "\n')
                s.read_until(b"READY> ")
                s.data = b""
                s.send(b"printf 'VISIBLE_'; printf 'NOW\\n'\r")
                s.read_until(b"VISIBLE_NOW\r\n")
                if shell.endswith("zsh"):
                    s.data = b""
                    s.send(b"printf 'ERROR_'; printf 'VISIBLE\\n' >&2")
                    # Let an autocomplete worker finish and close its FIFO.
                    time.sleep(.8)
                    s.send(b"\r")
                    s.read_until(b"VISIBLE\r\n")
                s.resize(18, 61)
                time.sleep(.12)
                s.data = b""
                s.send(b"stty size\r")
                s.read_until(b"18 61")
                s.data = b""
                s.send(b"sleep 10\r")
                time.sleep(.15)
                s.send(b"\x03")
                s.read_until(b"READY> " if shell.endswith("zsh") else b"[exit 130]", timeout=2)
                self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
                s.send(b"exit\r")
                self.assertEqual(s.wait(), 0)

    def test_zsh_reveal_and_right_arrow_accept_without_execution(self):
        for mode in ("automatic", "on-demand"):
            for key in (b"\x1b[C", b"\x1bOC"):
                with self.subTest(mode=mode, key=key):
                    s = self.session(["-i"], executable="/bin/zsh",
                                     rc=f'PROMPT="READY> "\neval "$(\"{BIN}\" init --print zsh)"\n',
                                     config=f'provider = "terminal-test"\nmode = "{mode}"\n[daemon]\nidle_shutdown_secs = 30\n')
                    s.read_until(b"READY> ")
                    s.read_until(b"\x1b[?2004h")
                    snapshot = Path(s.home.name) / "buffer"
                    install = (f'_tp_snapshot() {{ print -r -- "$BUFFER" > {snapshot}; '
                               f'print -r -- "$_TP_GHOST" > {snapshot}.ghost; '
                               f'print -r -- "$_TP_PENDING" > {snapshot}.pending; }}; '
                               'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot')
                    s.data = b""
                    s.send(install.encode() + b"\r")
                    s.read_until(b"READY> ")
                    s.read_until(b"\x1b[?2004h")
                    s.data = b""
                    s.send(b"git l")
                    deadline = time.monotonic() + 4
                    while time.monotonic() < deadline:
                        s.send_menu(b"\x18b")
                        if Path(str(snapshot) + ".pending").read_text() == "og\n":
                            break
                    self.assertEqual(Path(str(snapshot) + ".pending").read_text(), "og\n")
                    self.assertEqual(Path(str(snapshot) + ".ghost").read_text(),
                                     "og\n" if mode == "automatic" else "\n")
                    # Hidden suggestions cannot be accepted accidentally.
                    if mode == "on-demand":
                        s.send(b"\x1b[C\x18b")
                        time.sleep(.1)
                        self.assertEqual(snapshot.read_text(), "git l\n")
                    s.send(b"\x07")
                    deadline = time.monotonic() + 4
                    while time.monotonic() < deadline:
                        s.send_menu(b"\x18b")
                        if Path(str(snapshot) + ".ghost").read_text() == "og\n":
                            break
                    self.assertEqual(snapshot.read_text(), "git l\n")
                    self.assertEqual(Path(str(snapshot) + ".ghost").read_text(), "og\n")
                    s.send(key + b"\x18b")
                    time.sleep(.15)
                    self.assertEqual(snapshot.read_text(), "git log\n")
                    # Accept only edits the buffer; the shell is still editing.
                    self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
                    s.send(b"\x03exit 0\r")
                    self.assertEqual(s.wait(), 0)

    def test_zsh_tab_skips_file_completion_and_right_arrow_moves_normally(self):
        rc = ('PROMPT="READY> "\n'
              '_normal_tab() { LBUFFER+=".completed"; }\n'
              'zle -N _normal_tab; bindkey "^I" _normal_tab\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name) / "buffer"
        s.data = b""
        s.send((f'_tp_snapshot() {{ print -r -- "$BUFFER:$CURSOR" > {snapshot}; }}; '
                'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\r').encode())
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        s.send_menu(b"x\t")
        time.sleep(.2)
        s.send_menu(b"\x18b")
        time.sleep(.2)
        self.assertEqual(snapshot.read_text(), "x:1\n")
        s.send(b"\x03")
        s.data = b""
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        s.send_menu(b"x\x1b[D\x1b[C\x18b")
        time.sleep(.15)
        self.assertEqual(snapshot.read_text(), "x:1\n")
        s.send(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_clearing_input_discards_visible_and_hidden_suggestions(self):
        for mode, prefix, clear, remaining in (
                ("automatic", b"git l", b"\x7f" * 5, ""),
                ("on-demand", b"git l", b"\x7f" * 5, ""),
                ("automatic", b"git l", b"\x15", ""),
                ("automatic", b"  git l", b"\x7f" * 5, "  ")):
            with self.subTest(mode=mode, clear=clear, remaining=remaining):
                rc = ('PROMPT="READY> "\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
                      '"$_TP_GHOST" "$_TP_PENDING" "$_TP_FULL" "$_TP_FD" '
                      '"$_TP_DIRTY" "${#_TP_CANDIDATES}" > "$HOME/buffer"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                config = f'provider = "terminal-test"\nmode = "{mode}"\n'
                s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                snapshot = Path(s.home.name) / "buffer"
                s.send_menu(prefix)
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline:
                    s.send_menu(b"\x18b")
                    time.sleep(.08)
                    if snapshot.read_text().splitlines()[3] == "og":
                        break
                self.assertEqual(snapshot.read_text().splitlines()[3], "og")
                s.send_menu(clear + b"\x18b")
                time.sleep(.08)
                empty = [remaining, "", "", "", "", "", "0", "0"]
                self.assertEqual(snapshot.read_text().splitlines(), empty)
                # Reveal, cycle, and acceptance cannot resurrect cleared text.
                s.send_menu(b"\x07\x1b[Z\x1b[C\x18b")
                time.sleep(.15)
                self.assertEqual(snapshot.read_text().splitlines(), empty)
                s.send_menu(b"git l\x07")
                time.sleep(.6)
                s.send_menu(b"\x18b")
                time.sleep(.08)
                self.assertEqual(snapshot.read_text().splitlines()[:2],
                                 [remaining + "git l", "og"])
                s.send(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_zsh_late_reply_cannot_restore_suggestion_after_clearing_input(self):
        started = threading.Event()
        release = threading.Event()
        finished = threading.Event()

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                started.set()
                release.wait(4)
                body = b'{"choices":[{"message":{"content":"docker ps"}}]}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                finished.set()

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        config = ('provider = "local-test"\nmodel = "test-model"\n[[providers]]\n'
                  f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                  'no_auth = true\n')
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
              '"$_TP_GHOST" "$_TP_PENDING" "$_TP_FULL" "$_TP_FD" '
              '"$_TP_DIRTY" "${#_TP_CANDIDATES}" > "$HOME/buffer"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
        worker.start()
        self.addCleanup(release.set)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name) / "buffer"
        s.send_menu(b"do")
        self.assertTrue(started.wait(4))
        s.send_menu(b"\x7f\x7f\x18b")
        time.sleep(.08)
        empty = ["", "", "", "", "", "", "0", "0"]
        self.assertEqual(snapshot.read_text().splitlines(), empty)
        release.set()
        self.assertTrue(finished.wait(4))
        time.sleep(.4)
        s.send_menu(b"\x07\x1b[Z\x1b[C\x18b")
        time.sleep(.08)
        self.assertEqual(snapshot.read_text().splitlines(), empty)
        s.send_menu(b"do")
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            if snapshot.read_text().splitlines()[:2] == ["do", "cker ps"]:
                break
        self.assertEqual(snapshot.read_text().splitlines()[:2], ["do", "cker ps"])
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_pending_text_shimmers_without_editing_and_stops_on_reply_or_backspace(self):
        for no_color, animations, cancel in ((False, True, False), (False, True, True),
                                              (True, True, False), (False, False, False)):
            with self.subTest(no_color=no_color, animations=animations, cancel=cancel):
                started = threading.Event()
                release = threading.Event()

                class Handler(http.server.BaseHTTPRequestHandler):
                    def do_POST(self, started=started, release=release):
                        self.rfile.read(int(self.headers["Content-Length"]))
                        started.set()
                        release.wait(4)
                        body = b'{"choices":[{"message":{"content":"docker ps"}}]}'
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)

                    def log_message(self, *args):
                        pass

                server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
                worker = threading.Thread(target=server.serve_forever, daemon=True)
                self.addCleanup(server.server_close)
                self.addCleanup(server.shutdown)
                config = ('provider = "local-test"\nmodel = "test-model"\n[[providers]]\n'
                          f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                          f'no_auth = true\n[style]\nanimations = {str(animations).lower()}\n')
                rc = ('PROMPT="READY> "\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -r -- "$BUFFER:$CURSOR" > "$HOME/buffer"; '
                      'print -r -- "$POSTDISPLAY" > "$HOME/display"; '
                      'print -rl -- "$_TP_GLOW_PHASE" "$_TP_GLOW_FD" "$_TP_FD" > "$HOME/animation"; '
                      'print -rl -- "${region_highlight[@]}" > "$HOME/highlights"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                                 config=config, no_color=no_color)
                worker.start()
                self.addCleanup(release.set)
                s.read_until(b"READY> ")
                s.send_menu(b"do")
                self.assertTrue(started.wait(4))
                s.send_menu(b"\t\x18b")
                home = Path(s.home.name)
                self.assertIn("Thinking...", (home / "display").read_text())
                before = (home / "animation").read_text().splitlines()
                time.sleep(.4)
                s.send_menu(b"\x18b")
                after = (home / "animation").read_text().splitlines()
                self.assertEqual((home / "buffer").read_text(), "do:2\n")
                if animations and not no_color:
                    self.assertGreater(int(after[0]), int(before[0]))
                    self.assertTrue(after[1])
                    self.assertIn("fg=white", (home / "highlights").read_text())
                else:
                    self.assertEqual(after[0], "0")
                    self.assertEqual(after[1], "")
                if cancel:
                    s.send_menu(b"\x7f\x18b")
                    self.assertEqual((home / "buffer").read_text(), "d:1\n")
                    self.assertEqual((home / "display").read_text(), "\n")
                    self.assertEqual((home / "animation").read_text().splitlines()[1:], ["", ""])
                    release.set()
                    time.sleep(.3)
                    s.send_menu(b"\x18b")
                    self.assertEqual((home / "display").read_text(), "\n")
                else:
                    release.set()
                    s.read_until(b"cker ps")
                    # The loading shimmer ends, then the new suggestion's
                    # short appearance fade settles and releases its timer.
                    deadline = time.monotonic() + 4
                    while time.monotonic() < deadline:
                        s.send_menu(b"\x18b")
                        if (home / "animation").read_text().splitlines()[1:] == ["", ""]:
                            break
                        time.sleep(.05)
                    self.assertEqual((home / "animation").read_text().splitlines()[1:], ["", ""])
                    self.assertNotIn("fg=white", (home / "highlights").read_text())
                    self.assertEqual((home / "buffer").read_text(), "do:2\n")
                s.send_menu(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)
                self.doCleanups()

    def test_zsh_reuses_reply_when_user_continues_typing(self):
        started = threading.Event()
        release = threading.Event()
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                started.set()
                release.wait(4)
                body = b'{"choices":[{"message":{"content":"docker ps"}}]}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        config = ('provider = "local-test"\nmodel = "test-model"\n[[providers]]\n'
                  f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                  'no_auth = true\n')
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -r -- "$BUFFER" > "$HOME/buffer"; '
              'print -r -- "$_TP_GHOST" >> "$HOME/buffer"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
        worker.start()
        self.addCleanup(release.set)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        s.send(b"do")
        self.assertTrue(started.wait(4))
        s.send(b"cker p")
        time.sleep(.05)
        release.set()
        time.sleep(.4)
        s.send(b"\x18b")
        time.sleep(.1)
        self.assertEqual((Path(s.home.name) / "buffer").read_text(), "docker p\ns\n")
        self.assertEqual(len(requests), 1)
        s.send(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_dirty_reply_refetches_the_extended_live_prefix(self):
        started = threading.Event()
        release = threading.Event()
        second_started = threading.Event()
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                if len(requests) == 1:
                    started.set()
                    release.wait(4)
                    command = "omp update"
                else:
                    second_started.set()
                    command = "omp resume"
                body = json.dumps({"choices": [{"message": {"content": command}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.addCleanup(release.set)
        config = ('provider = "local-test"\nmodel = "test-model"\n[[providers]]\n'
                  f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                  'no_auth = true\n')
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_FULL" "$_TP_FD" '
              '"$_TP_DIRTY" "${#_TP_CANDIDATES}" "${_TP_CANDIDATES[@]}" > "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
        worker.start()
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name, "state")

        s.send_menu(b"om")
        self.assertTrue(started.wait(4))
        s.send_menu(b"p\x18b")
        state = snapshot.read_text().splitlines()
        self.assertEqual(state[:2], ["omp", ""])
        self.assertTrue(state[2])
        self.assertEqual(state[3], "1")

        release.set()
        self.assertTrue(second_started.wait(4))
        second_prompt = "\n".join(
            message["content"] for message in requests[1]["messages"]
            if isinstance(message.get("content"), str)
        )
        self.assertIn("$ omp", second_prompt)
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            state = snapshot.read_text().splitlines()
            if len(state) > 1 and state[1] == "omp resume":
                break
        self.assertEqual(state[:2], ["omp", "omp resume"])
        self.assertEqual(len(requests), 2)
        s.send(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_cycle_key_encodings_and_custom_binding_in_both_keymaps(self):
        defaults = (b"\x1b[Z", b"\x1b[1;2Z", b"\x1b[9;2u", b"\x1b[27;2;9~")
        variants = (" -i input.mp4 output.mp4", " -i input.mp4 -vn output.mp3",
                    " -i input.mp4 -vf scale=1280:-2 output.mp4")
        for map_name, custom in (("emacs", False), ("viins", False),
                                 ("emacs", True), ("viins", True)):
            with self.subTest(keymap=map_name, custom=custom):
                config = 'provider = "terminal-test"\n'
                if custom:
                    config += '[keys]\ncycle = "^N"\n'
                rc = ('PROMPT="READY> "\n'
                      + ('bindkey -v\n' if map_name == "viins" else 'bindkey -e\n')
                      + f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
                      '"${#_TP_CANDIDATES}" > "$HOME/buffer"; }\n'
                      f'zle -N _tp_snapshot; bindkey -M {map_name} "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                snapshot = Path(s.home.name) / "buffer"
                s.send_menu(b"ffmpeg")
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline:
                    s.send_menu(b"\x18b")
                    time.sleep(.08)
                    if snapshot.read_text().splitlines()[1] == variants[0]:
                        break
                self.assertEqual(snapshot.read_text().splitlines(), ["ffmpeg", variants[0], "3"])
                keys = (b"\x0e",) * 4 if custom else defaults
                for i, key in enumerate(keys, 1):
                    s.send_menu(key + b"\x18b")
                    time.sleep(.08)
                    self.assertEqual(snapshot.read_text().splitlines(),
                                     ["ffmpeg", variants[-i % 3], "3"])
                s.send_menu(b"\x1b[C\x18b")
                time.sleep(.08)
                self.assertEqual(snapshot.read_text().splitlines()[:2],
                                 ["ffmpeg" + variants[2], ""])
                self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
                s.send_menu(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_zsh_tab_selector_and_right_arrow_in_both_modes_and_no_color(self):
        for mode, no_color in (("automatic", False), ("on-demand", False),
                               ("automatic", True)):
            with self.subTest(mode=mode, no_color=no_color):
                rc = ('PROMPT="READY> "\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
                      '"$_TP_PENDING" "$_TP_MENU_OPEN" "$_TP_CANDIDATE_INDEX" '
                      '> "$HOME/buffer"; print -r -- "$POSTDISPLAY" > "$HOME/menu"; '
                      'print -rl -- "${region_highlight[@]}" > "$HOME/colors"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                config = f'provider = "terminal-test"\nmode = "{mode}"\n'
                s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config,
                                 no_color=no_color, cols=56, rows=9)
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                snapshot = Path(s.home.name) / "buffer"
                menu = Path(s.home.name) / "menu"
                s.send_menu(b"git l")
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline:
                    s.send_menu(b"\x18b")
                    if snapshot.read_text().splitlines()[2] == "og":
                        break
                before = snapshot.read_text().splitlines()
                self.assertEqual(before[0], "git l")
                self.assertEqual(before[1], "og" if mode == "automatic" else "")
                s.send_menu(b"\t\x18b")
                self.assertEqual(snapshot.read_text().splitlines(), ["git l", "og", "og", "1", "1"])
                shown = menu.read_text()
                self.assertIn("TermPlete  1/3", shown)
                self.assertIn("> git log\n", shown)
                self.assertIn("git log --oneline", shown)
                self.assertTrue(all(len(row) <= 54 for row in shown.splitlines()))
                colors = (Path(s.home.name) / "colors").read_text()
                if no_color:
                    self.assertNotIn("bg=", colors)
                else:
                    self.assertIn("bg=", colors)
                s.send_menu(b"\t\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[0:2], ["git l", "og --oneline"])
                self.assertIn("> git log --oneline", menu.read_text())
                colors = (Path(s.home.name) / "colors").read_text()
                self.assertEqual(len(selected_rows(colors)), 0 if no_color else 1)
                if not no_color:
                    selected = selected_rows(colors)[0]
                    start, end = map(int, selected.split()[:2])
                    selected_row = "> git log --oneline"
                    expected = len("git l") + menu.read_text().index(selected_row)
                    self.assertEqual((start, end), (expected, expected + len(selected_row)))
                s.send_menu(b"\x1b[Z\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[0:2], ["git l", "og"])
                colors = (Path(s.home.name) / "colors").read_text()
                self.assertEqual(len(selected_rows(colors)), 0 if no_color else 1)
                s.send_menu(b"\x1b[C\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[:4], ["git log", "", "", "0"])
                self.assertEqual(menu.read_text(), "\n")
                self.assertNotIn("memo=termplete", (Path(s.home.name) / "colors").read_text())
                self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
                s.send(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_zsh_alt_right_accepts_words_and_keeps_the_remaining_suggestion(self):
        command = "printf résumé ~/developer/my-project --wrong"
        for keymap, mode, key in (("emacs", "automatic", b"\x1b[1;3C"),
                                 ("viins", "on-demand", b"\x1bf"),
                                 ("emacs", "on-demand", b"\x1b[1;3C")):
            with self.subTest(keymap=keymap, mode=mode, key=key):
                rc = ('PROMPT="READY> "\ncd "$HOME"\n'
                      f'bindkey -{"v" if keymap == "viins" else "e"}\n'
                      'autoload -Uz add-zsh-hook\n'
                      '_test_preexec() { print -r -- "$1" > "$HOME/executed"; }\n'
                      'add-zsh-hook preexec _test_preexec\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_test_bin() { if [[ $1 == feedback ]]; then print -rl -- "$@" >> "$HOME/feedback"; '
                      f'else "{BIN}" "$@"; fi; }}\n_TP_BIN=_test_bin\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" "$CURSOR" '
                      '"$_TP_MENU_OPEN" > "$HOME/state"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                config = f'provider = "terminal-test"\nmode = "{mode}"\n[daemon]\nidle_shutdown_secs = 30\n'
                s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config,
                                 history=f": 10:0;{command}\n: 20:0;printf alternative\n")
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                snapshot = Path(s.home.name, "state")
                feedback = Path(s.home.name, "feedback")
                executed = Path(s.home.name, "executed")
                executed.unlink(missing_ok=True)
                s.send_menu(b"pri")
                if mode == "on-demand":
                    s.send_menu(key + b"\x18b")
                    self.assertEqual(snapshot.read_text().splitlines()[:2], ["pri", ""])
                s.send_menu(b"\t")
                s.read_until(command.encode())
                s.send_menu(b"\t")
                for prefix in ("printf ", "printf résumé ", "printf résumé ~/developer/my-project "):
                    s.send_menu(key + b"\x18b")
                    self.assertEqual(snapshot.read_text().splitlines(),
                                     [prefix, command[len(prefix):], str(len(prefix)), "0"])
                    self.assertFalse(feedback.exists(), "partial acceptance must not learn the full command")
                    self.assertFalse(executed.exists())
                s.send_menu((key if mode == "on-demand" else b"\x1b[C") + b"\x18b")
                self.assertEqual(snapshot.read_text().splitlines(), [command, "", str(len(command)), "0"])
                deadline = time.monotonic() + 2
                while not feedback.exists() and time.monotonic() < deadline:
                    time.sleep(.02)
                self.assertEqual(feedback.read_text().splitlines().count("feedback"), 1)
                self.assertFalse(executed.exists())
                s.send(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_zsh_alt_right_can_edit_or_run_only_the_accepted_prefix_and_preserves_native_motion(self):
        command = "printf good wrong-argument"
        rc = ('PROMPT="READY> "\ncd "$HOME"\n'
              'autoload -Uz add-zsh-hook\n'
              '_test_preexec() { print -r -- "$1" > "$HOME/executed"; }\n'
              'add-zsh-hook preexec _test_preexec\n'
              '_test_forward_word() { print -r -- called >> "$HOME/motion"; zle .forward-word; }\n'
              'zle -N _test_forward_word\n'
              'bindkey -M emacs "^[[1;3C" _test_forward_word\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_install\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" "$CURSOR" '
              '"$_TP_MENU_OPEN" > "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        for edit in (False, True):
            s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                             history=f": 10:0;{command}\n")
            s.read_until(b"READY> ")
            s.read_until(b"\x1b[?2004h")
            snapshot = Path(s.home.name, "state")
            executed = Path(s.home.name, "executed")
            executed.unlink(missing_ok=True)
            s.send_menu(b"pri")
            s.read_until(b"good wrong-argument")
            s.send_menu(b"\t\x1b[1;3C\x1b[1;3C\x18b")
            self.assertEqual(snapshot.read_text().splitlines(), ["printf good ", "wrong-argument", "12", "0"])
            if edit:
                s.send_menu(b"replacement\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[:2], ["printf good replacement", ""])
            start = len(s.data)
            s.send_menu(b"\r")
            s.read_until(b"READY> ", after=start)
            self.assertEqual(executed.read_text().strip(), "printf good replacement" if edit else "printf good")
            s.send_menu(b"alpha beta\x01\x1b[1;3C\x18b")
            self.assertEqual(snapshot.read_text().splitlines()[:3], ["alpha beta", "", "6"])
            self.assertEqual(Path(s.home.name, "motion").read_text(), "called\n")
            s.send(b"\x03exit 0\r")
            self.assertEqual(s.wait(), 0)

    def test_zsh_alt_right_can_accept_a_generated_task_command_word_by_word(self):
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_test_task_result() { _tp_cancel_inflight; '
              '_TP_INTENT_BUFFER=$BUFFER; _TP_FULL="printf good wrong"; '
              '_TP_CANDIDATES=("$_TP_FULL"); _TP_ALL_CANDIDATES=("$_TP_FULL"); '
              '_TP_CANDIDATE_INDEX=1; _TP_MENU_OPEN=1; _TP_REVEALED=1; _tp_render; }\n'
              'zle -N _test_task_result; bindkey "^Xt" _test_task_result\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" "$CURSOR" '
              '"$_TP_MENU_OPEN" "$_TP_INTENT_BUFFER" > "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name, "state")
        s.send_menu(b"# print something\x18t\x1b[1;3C\x18b")
        self.assertEqual(snapshot.read_text().splitlines(), ["printf ", "good wrong", "7", "0", ""])
        s.send_menu(b"\x1b[1;3C\x18b")
        self.assertEqual(snapshot.read_text().splitlines(), ["printf good ", "wrong", "12", "0", ""])
        self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
        s.send(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_corrects_a_failed_command_then_enters_the_created_directory(self):
        rc = ('PROMPT="READY> "\ncd "$HOME"\nmkdir nuxtmodulefork\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_FULL" > "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                         history=": 10:0;mkdir nuxtmodulefork\n: 20:0;cd nuxtmodulefork\n")
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        start = len(s.data)
        s.send_menu(b"mnkdir orch-test\r")
        s.read_until(b"command not found: mnkdir", after=start)
        s.read_until(b"READY> ", after=start)
        start = len(s.data)
        s.send_menu(b"mkdir\t")
        s.read_until(b"mkdir orch-test", after=start)
        s.send_menu(b"\x18b")
        self.assertEqual(Path(s.home.name, "state").read_text().splitlines(),
                         ["mkdir", "mkdir orch-test"])
        start = len(s.data)
        s.send_menu(b"\x1b[C\r")
        s.read_until(b"READY> ", after=start)
        self.assertTrue(Path(s.home.name, "orch-test").is_dir())
        start = len(s.data)
        s.send_menu(b"cd \t")
        s.read_until(b"cd orch-test/", after=start)
        s.send_menu(b"\x18b")
        self.assertEqual(Path(s.home.name, "state").read_text().splitlines(),
                         ["cd ", "cd orch-test/"])
        s.send(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_enter_accepts_selector_without_execution_then_executes_on_next_enter(self):
        for accept_key in (b"\x1b[C", b"\r"):
            with self.subTest(accept_key=accept_key):
                rc = ('PROMPT="READY> "\ncd "$HOME"\n'
                      'autoload -Uz add-zsh-hook\n'
                      '_tp_test_preexec() { print -r -- "$1" > "$HOME/executed"; }\n'
                      'add-zsh-hook preexec _tp_test_preexec\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
                      '"$_TP_MENU_OPEN" "$_TP_CANDIDATE_INDEX" > "$HOME/state"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc)
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                executed = Path(s.home.name, "executed")
                executed.unlink(missing_ok=True)
                snapshot = Path(s.home.name, "state")
                s.send_menu(b"git l")
                s.read_until(b"git log")
                s.send_menu(b"\t\t\x18b")
                self.assertEqual(snapshot.read_text().splitlines(),
                                 ["git l", "og --oneline", "1", "2"])

                s.send_menu(accept_key + b"\x18b")
                self.assertEqual(snapshot.read_text().splitlines(),
                                 ["git log --oneline", "", "0", "1"])
                self.assertFalse(executed.exists())
                self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))

                start = len(s.data)
                s.send_menu(b"\r")
                s.read_until(b"READY> ", after=start)
                deadline = time.monotonic() + 2
                while not executed.exists() and time.monotonic() < deadline:
                    time.sleep(.02)
                self.assertEqual(executed.read_text(), "git log --oneline\n")
                s.send_menu(b"exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_zsh_enter_with_closed_ghost_executes_typed_buffer_unchanged(self):
        rc = ('PROMPT="READY> "\ncd "$HOME"\n'
              'autoload -Uz add-zsh-hook\n'
              '_tp_test_preexec() { print -r -- "$1" > "$HOME/executed"; }\n'
              'add-zsh-hook preexec _tp_test_preexec\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" "$_TP_MENU_OPEN" '
              '> "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        executed = Path(s.home.name, "executed")
        executed.unlink(missing_ok=True)
        snapshot = Path(s.home.name, "state")
        s.send_menu(b"git l")
        s.read_until(b"og")
        s.send_menu(b"\x18b")
        self.assertEqual(snapshot.read_text().splitlines(), ["git l", "og", "0"])

        start = len(s.data)
        s.send_menu(b"\r")
        s.read_until(b"READY> ", after=start)
        self.assertEqual(executed.read_text(), "git l\n")
        s.send_menu(b"exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_feedback_and_record_receive_execution_cwd(self):
        global BIN
        real_bin = BIN
        quoted_bin = real_bin.replace("'", "'\\''")
        wrapper_home = tempfile.TemporaryDirectory(prefix="tp-bin-wrapper-")
        self.addCleanup(wrapper_home.cleanup)
        wrapper = Path(wrapper_home.name, "tp")
        wrapper.write_text(
            "#!/bin/sh\n"
            'case "$1" in feedback|record) '
            'printf "%s\\n" "$@" >> "$HOME/cli-args"; '
            'printf "%s\\n" "---" >> "$HOME/cli-args" ;; esac\n'
            f"exec '{quoted_bin}' \"$@\"\n"
        )
        wrapper.chmod(0o755)
        BIN = str(wrapper)
        try:
            rc = ('PROMPT="READY> "\ncd "$HOME"\n'
                  f'eval "$(\"{BIN}\" init --print zsh)"\n')
            s = self.session(["-i"], executable="/bin/zsh", rc=rc)
        finally:
            BIN = real_bin
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        calls = Path(s.home.name, "cli-args")

        def wait_for_call(name, command):
            deadline = time.monotonic() + 4
            while time.monotonic() < deadline:
                if calls.exists():
                    blocks = [block.splitlines() for block in calls.read_text().split("---\n")
                              if block.strip()]
                    for block in blocks:
                        if block and block[0] == name and command in block:
                            return block
                time.sleep(.02)
            self.fail(f"missing {name} call for {command!r}: {calls.read_text() if calls.exists() else ''}")

        s.send_menu(b"git l")
        s.read_until(b"git log")
        s.send_menu(b"\t\x1b[C")
        feedback = wait_for_call("feedback", "git log")
        self.assertEqual(feedback[feedback.index("--cwd") + 1], s.home.name)

        # The record hook runs after cd has changed PWD, so it must use the
        # directory captured by preexec.
        s.send_menu(b"\x03")
        start = len(s.data)
        s.send_menu(b"cd /\r")
        s.read_until(b"READY> ", after=start)
        record = wait_for_call("record", "cd /")
        self.assertEqual(record[record.index("--cwd") + 1], s.home.name)
        s.send_menu(b"exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_enter_does_not_submit_pending_or_empty_open_selector(self):
        rc = ('PROMPT="READY> "\ncd "$HOME"\n'
              'autoload -Uz add-zsh-hook\n'
              '_tp_test_preexec() { print -r -- "$1" > "$HOME/executed"; }\n'
              'add-zsh-hook preexec _tp_test_preexec\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_pending_empty() {\n'
              '  BUFFER="missing_termplete_command"; CURSOR=$#BUFFER\n'
              '  _TP_CANDIDATES=(); _TP_ALL_CANDIDATES=(); _TP_CANDIDATE_INDEX=1\n'
              '  _TP_MENU_OPEN=1; _TP_REVEALED=1\n'
              '  (( _TP_REQ_ID++ )); local id=$_TP_REQ_ID fifo="$_TP_TMP/reply.$_TP_REQ_ID"\n'
              '  mkfifo -m 600 "$fifo"\n'
              '  { exec {_TP_FD}<>"$fifo" } 2>/dev/null || return 0\n'
              '  _TP_INFLIGHT[$_TP_FD]=$id; zle -F -w "$_TP_FD" _tp_on_reply\n'
              '  ( sleep .6; print -r -- $\'E\\tNo suggestions\' > "$fifo" ) '
              '>/dev/null 2>&1 &!\n'
              '  _tp_render\n'
              '}\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_MENU_OPEN" '
              '"$#_TP_CANDIDATES" "$_TP_FD" "$_TP_ISSUE" > "$HOME/state"; }\n'
              'zle -N _tp_pending_empty; zle -N _tp_snapshot\n'
              'bindkey "^Xp" _tp_pending_empty; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        executed = Path(s.home.name, "executed")
        executed.unlink(missing_ok=True)
        snapshot = Path(s.home.name, "state")
        s.send_menu(b"\x18p\r\x18b")
        pending = snapshot.read_text().splitlines()
        self.assertEqual(pending[:3], ["missing_termplete_command", "1", "0"])
        self.assertTrue(pending[3])
        self.assertFalse(executed.exists())
        self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))

        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            state = snapshot.read_text().splitlines()
            if len(state) == 5 and not state[3] and state[4] == "No suggestions":
                break
        self.assertEqual(state, ["missing_termplete_command", "1", "0", "", "No suggestions"])
        s.send_menu(b"\r\x18b")
        self.assertEqual(snapshot.read_text().splitlines(),
                         ["missing_termplete_command", "1", "0", "", "No suggestions"])
        self.assertFalse(executed.exists())
        self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_tab_selection_survives_late_reply(self):
        rc = ('PROMPT="READY> "\ncd "$HOME"\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_pending_ready() {\n'
              '  BUFFER="git l"; CURSOR=$#BUFFER\n'
              '  _TP_ALL_CANDIDATES=("git log" "git log --oneline" "git log -1")\n'
              '  _TP_CANDIDATES=("git log" "git log --oneline" "git log -1")\n'
              '  _TP_CANDIDATE_INDEX=1; _TP_MENU_OPEN=1; _TP_REVEALED=1\n'
              '  _tp_select_candidate\n'
              '  (( _TP_REQ_ID++ )); local id=$_TP_REQ_ID fifo="$_TP_TMP/reply.$_TP_REQ_ID"\n'
              '  mkfifo -m 600 "$fifo"\n'
              '  { exec {_TP_FD}<>"$fifo" } 2>/dev/null || return 0\n'
              '  _TP_INFLIGHT[$_TP_FD]=$id; zle -F -w "$_TP_FD" _tp_on_reply\n'
              '  ( sleep .6; print -r -- $\'C\\tgit log\\tgit log -1\' '
              '> "$fifo" ) >/dev/null 2>&1 &!\n'
              '  _tp_render\n'
              '}\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" "$_TP_MENU_OPEN" '
              '"$_TP_CANDIDATE_INDEX" "$_TP_FD" "$_TP_FULL" '
              '"$_TP_SELECTION_EXPLICIT" > "$HOME/state"; }\n'
              'zle -N _tp_pending_ready; zle -N _tp_snapshot\n'
              'bindkey "^Xp" _tp_pending_ready; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name, "state")
        s.send_menu(b"\x18p\t\x18b")
        selected = snapshot.read_text().splitlines()
        self.assertEqual(selected[:4], ["git l", "og --oneline", "1", "2"])
        self.assertTrue(selected[4])
        self.assertEqual(selected[6], "1")

        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            state = snapshot.read_text().splitlines()
            if len(state) == 6 and not state[4]:
                break
        self.assertEqual(state[:4], ["git l", "og --oneline", "1", "3"])
        self.assertEqual(state[5], "git log --oneline")
        self.assertEqual(state[6], "1")
        s.send_menu(b"\r\x18b")
        self.assertEqual(snapshot.read_text().splitlines()[:4],
                         ["git log --oneline", "", "0", "1"])
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_acceptance_learns_without_counting_browsing(self):
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n')
        config = 'provider = "terminal-test"\n[daemon]\nidle_shutdown_secs = 30\n'
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        s.send_menu(b"ffmpeg")
        s.read_until(b"output.mp4")
        s.send_menu(b"\t\t")
        s.read_until(b"TermPlete")
        memory = Path(s.home.name) / "termplete/commands.json"
        preferred = "ffmpeg -i input.mp4 -vn output.mp3"
        if memory.exists():
            self.assertNotIn(preferred, memory.read_text())
        s.send_menu(b"\x1b[C")
        env = dict(os.environ, HOME=s.home.name, TERMPLETE_HOME=s.home.name,
                   TERMPLETE_SOCKET=s.home.name + "/daemon.sock", TERM_PROGRAM="terminal-test")
        for key in ("HISTFILE", "TERMPLETE_HISTFILE", "WARP_IS_LOCAL_SHELL_SESSION"):
            env.pop(key, None)
        deadline = time.monotonic() + 3
        reply = {}
        while time.monotonic() < deadline:
            result = subprocess.run([BIN, "suggest", "--local-only", "--json", "ffmpeg"],
                                    env=env, capture_output=True, check=True)
            reply = json.loads(result.stdout)
            if reply["candidates"] and reply["candidates"][0] == preferred:
                break
            time.sleep(.05)
        self.assertEqual(reply["candidates"][0], preferred)
        self.assertIn(preferred, memory.read_text())
        # Clear the accepted line, then learn the same choice again without running it.
        s.data = b""
        s.send_menu(b"\x15ffmpeg")
        s.read_until(b"output.mp3")
        s.send_menu(b"\x1b[C")
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            choices = json.loads(memory.read_text()).get("choices", {}).get("ffmpeg", {})
            if choices.get(preferred, {}).get("uses") == 2:
                break
            time.sleep(.05)
        self.assertEqual(choices[preferred]["uses"], 2)
        subprocess.run([BIN, "service", "stop"], env=env, capture_output=True, check=True)
        deadline = time.monotonic() + 3
        while Path(s.home.name, "daemon.sock").exists() and time.monotonic() < deadline:
            time.sleep(.03)
        result = subprocess.run([BIN, "suggest", "--local-only", "--json", "ffmpeg"],
                                env=env, capture_output=True, check=True)
        self.assertEqual(json.loads(result.stdout)["candidates"][0], preferred)
        self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
        s.send(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_backspace_dismisses_selector_and_suggestion(self):
        for vi in (False, True):
            with self.subTest(vi=vi):
                rc = ('PROMPT="READY> "\n'
                      + ('bindkey -v\n' if vi else 'bindkey -e\n')
                      + f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$POSTDISPLAY" '
                      '"$_TP_GHOST" "$_TP_PENDING" "$_TP_MENU_OPEN" "$_TP_FD" '
                      '"${#_TP_CANDIDATES}" > "$HOME/buffer"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc)
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                s.send_menu(b"ffmpeg")
                s.read_until(b"output.mp4")
                s.send_menu(b"\t")
                s.read_until(b"TermPlete")
                s.send_menu(b"\x7f\x18b")
                snapshot = Path(s.home.name) / "buffer"
                self.assertEqual(snapshot.read_text().splitlines(),
                                 ["ffmpe", "", "", "", "0", "", "0"])
                time.sleep(.4)
                s.send(b"\x18b")
                time.sleep(.08)
                self.assertEqual(snapshot.read_text().splitlines(),
                                 ["ffmpe", "", "", "", "0", "", "0"])
                s.send(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_blank_line_removes_third_variant_counter_and_every_owned_visual(self):
        for keymap in ("emacs", "viins"):
            for clear in (b"\x7f" * 5, b"\x15", b"\x18e"):
                with self.subTest(keymap=keymap, clear=clear):
                    rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                          'PROMPT="READY> "\n_TP_TEST_MESSAGE=""\n'
                          'zle() { [[ $1 == -M ]] && _TP_TEST_MESSAGE="$2"; builtin zle "$@"; }\n'
                          f'eval "$(\"{BIN}\" init --print zsh)"\n'
                          '_tp_old_counter() { zle -M "TermPlete Suggestion 3/3"; }\n'
                          '_tp_external_clear() { BUFFER=""; POSTDISPLAY=""; _TP_HIGHLIGHTS=(); '
                          'region_highlight+=("0 5 bg=red memo=termplete"); }\n'
                          '_tp_snapshot() { print -rl -- "$BUFFER" "$POSTDISPLAY" "$_TP_GHOST" '
                          '"$_TP_PENDING" "$_TP_FULL" "$_TP_MENU_OPEN" "${#_TP_CANDIDATES}" '
                          '"$_TP_GLOW_FD" "$_TP_FD" "$_TP_PULSE" "$_TP_TEST_MESSAGE" > "$HOME/buffer"; '
                          'print -r -- "${(M)region_highlight:#*memo=termplete}" > "$HOME/colors"; }\n'
                          'zle -N _tp_snapshot; zle -N _tp_old_counter; zle -N _tp_external_clear\n'
                          f'bindkey -M {keymap} "^Xb" _tp_snapshot\n'
                          f'bindkey -M {keymap} "^Xq" _tp_old_counter\n'
                          f'bindkey -M {keymap} "^Xe" _tp_external_clear\n')
                    s = self.session(["-i"], executable="/bin/zsh", rc=rc)
                    s.read_until(b"READY> ")
                    s.send_menu(b"git l")
                    s.read_until(b"og")
                    s.send_menu(b"\t\t\t\x18b")
                    self.assertIn("TermPlete  3/3", Path(s.home.name, "buffer").read_text())
                    s.send_menu(b"\x18q\x18b")
                    snapshot = Path(s.home.name, "buffer")
                    self.assertIn("3/3", snapshot.read_text())
                    s.send_menu(clear + b"\x18b")
                    expected = ["", "", "", "", "", "0", "0", "", "", "0", ""]
                    self.assertEqual(snapshot.read_text().splitlines(), expected)
                    self.assertEqual(Path(s.home.name, "colors").read_text(), "\n")
                    # Empty-input shortcuts, toggling, and delayed ticks cannot
                    # reintroduce a counter, spinner, footer, or highlight.
                    s.send_menu(b"\x07\t\x1b[Z\x1b[C\x18\x18\x18\x18")
                    time.sleep(.4)
                    s.send_menu(b"\x18b")
                    self.assertEqual(snapshot.read_text().splitlines(), expected)
                    self.assertEqual(Path(s.home.name, "colors").read_text(), "\n")
                    s.send_menu(b"\x03exit 0\r")
                    self.assertEqual(s.wait(), 0)

    def test_configured_theme_colors_and_short_selection_fades(self):
        for terminal, animations, no_color in (("xterm-256color", True, False),
                                                ("xterm-256color", False, False),
                                                ("xterm-256color", True, True),
                                                ("vt100", True, False)):
            with self.subTest(terminal=terminal, animations=animations, no_color=no_color):
                config = ('provider = "terminal-test"\n[style]\n'
                          'automatic_color = "#00ff00"\nghost_color = "#808080"\n'
                          f'animations = {str(animations).lower()}\n')
                rc = ('PROMPT="READY> "\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER:$CURSOR" "$_TP_PULSE" "$_TP_GLOW_FD" '
                      '"$_TP_ACCENT_COLOR" "$_TP_MUTED_COLOR" > "$HOME/state"; '
                      'print -rl -- "${_TP_HIGHLIGHTS[@]}" > "$HOME/colors"; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config,
                                 terminal=terminal, no_color=no_color)
                s.read_until(b"READY> ")
                s.send_menu(b"git l")
                s.read_until(b"og")
                s.send_menu(b"\t\t\x18b")
                home = Path(s.home.name)
                state = (home / "state").read_text().splitlines()
                colors = (home / "colors").read_text()
                self.assertEqual(state[0], "git l:5")
                if no_color:
                    self.assertEqual(colors, "\n")
                    self.assertEqual(state[1:3], ["0", ""])
                else:
                    self.assertEqual(len(selected_rows(colors)), 1)
                    self.assertIn("fg=yellow,bold", colors)
                    if terminal == "xterm-256color":
                        self.assertEqual(state[3:], ["46", "244"])
                        self.assertIn("fg=16,bg=51,bold", colors)
                    else:
                        self.assertEqual(state[3:], ["cyan", "8"])
                        self.assertIn("bg=cyan", colors)
                    if animations:
                        self.assertGreater(int(state[1]), 0)
                        self.assertTrue(state[2])
                    else:
                        self.assertEqual(state[1:3], ["0", ""])
                time.sleep(.5)
                s.send_menu(b"\x18b")
                settled = (home / "state").read_text().splitlines()
                self.assertEqual(settled[:3], ["git l:5", "0", ""])
                if terminal == "xterm-256color" and not no_color:
                    self.assertIn("fg=244 memo=termplete", (home / "colors").read_text())
                s.send_menu(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_selector_overrides_inherited_gray_foreground_and_purple_background(self):
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_other_highlight() { region_highlight+=("${#BUFFER} '
              '$(( ${#BUFFER} + ${#POSTDISPLAY} )) fg=8,bg=magenta memo=other"); }\n'
              '_tp_snapshot() { print -rl -- "${region_highlight[@]}" > "$HOME/colors"; }\n'
              'zle -N _other_highlight; zle -N _tp_snapshot\n'
              'bindkey "^Xg" _other_highlight; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc)
        s.read_until(b"READY> ")
        s.send_menu(b"git l")
        s.read_until(b"og")
        s.send_menu(b"\t")
        s.send_menu(b"\x18g")
        s.send_menu(b"\x18b")
        colors = Path(s.home.name, "colors").read_text().splitlines()
        other = next(i for i, row in enumerate(colors) if "memo=other" in row)
        # ZLE canonicalizes fg=default,bg=default to 'none'.
        reset = next((i for i, row in enumerate(colors)
                      if "none memo=termplete" in row or "fg=default,bg=default" in row), None)
        self.assertIsNotNone(reset, colors)
        selected = next(i for i, row in enumerate(colors) if "fg=16,bg=51,bold" in row)
        self.assertLess(other, reset)
        self.assertLess(reset, selected)
        # Verify emitted terminal colors, not only our internal style strings.
        self.assertIn(b"\x1b[38;5;16m", s.data)
        self.assertIn(b"\x1b[48;5;51m", s.data)
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_arrows_preserve_each_encoding_after_reinstall(self):
        for keymap in ("emacs", "viins"):
            with self.subTest(keymap=keymap):
                rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                      'PROMPT="READY> "\n'
                      '_csi_up() { LBUFFER+="CSI_UP"; }\n'
                      '_ss3_up() { LBUFFER+="SS3_UP"; }\n'
                      '_csi_down() { LBUFFER+="CSI_DOWN"; }\n'
                      '_ss3_down() { LBUFFER+="SS3_DOWN"; }\n'
                      'zle -N _csi_up; zle -N _ss3_up; zle -N _csi_down; zle -N _ss3_down\n'
                      f'bindkey -M {keymap} "^[[A" _csi_up\n'
                      f'bindkey -M {keymap} "^[OA" _ss3_up\n'
                      f'bindkey -M {keymap} "^[[B" _csi_down\n'
                      f'bindkey -M {keymap} "^[OB" _ss3_down\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_install; _tp_install\n'
                      f'_select_map() {{ zle -K {keymap}; }}\n'
                      f'zle -N _select_map; bindkey -M {keymap} "^Xm" _select_map\n'
                      '_tp_snapshot() { print -r -- "$BUFFER" > "$HOME/state"; }\n'
                      f'zle -N _tp_snapshot; bindkey -M {keymap} "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                                 config='mode="on-demand"\nprovider="terminal-test"\n')
                s.read_until(b"READY> ")
                # Exercise both the main alias and the explicit editing map.
                for select_map in (b"", b"\x18m"):
                    s.send_menu(select_map + b"\x1b[A\x1bOA\x1b[B\x1bOB\x18b")
                    self.assertEqual(Path(s.home.name, "state").read_text().strip(),
                                     "CSI_UPSS3_UPCSI_DOWNSS3_DOWN")
                    s.data = b""
                    s.send_menu(b"\x03")
                    s.read_until(b"READY> ")

    def test_zsh_empty_selector_arrows_return_to_history(self):
        for keymap in ("emacs", "viins"):
            with self.subTest(keymap=keymap):
                rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                      'PROMPT="READY> "\nHISTSIZE=1000\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      'print -s -- "echo HISTORY_ONE"; print -s -- "echo HISTORY_TWO"\n'
                      '_pending_menu() { _tp_dismiss; BUFFER="echo "; CURSOR=${#BUFFER}; '
                      '_TP_MENU_OPEN=1; }\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_MENU_OPEN" > "$HOME/state"; }\n'
                      'zle -N _pending_menu; zle -N _tp_snapshot\n'
                      f'bindkey -M {keymap} "^Xp" _pending_menu\n'
                      f'bindkey -M {keymap} "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                                 config='mode="on-demand"\nprovider="terminal-test"\n')
                s.read_until(b"READY> ")
                s.send_menu(b"\x18p\x1b[A\x18b")
                self.assertEqual(Path(s.home.name, "state").read_text().splitlines(),
                                 ["echo HISTORY_TWO", "0"])

    def test_zsh_native_history_arrows_with_capture_and_search_widgets(self):
        for keymap in ("emacs", "viins"):
            for capture in (False, True):
                for up, down in ((b"\x1b[A", b"\x1b[B"), (b"\x1bOA", b"\x1bOB")):
                    with self.subTest(keymap=keymap, capture=capture, up=up):
                        rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                              'PROMPT="READY> "\nHISTSIZE=1000\n'
                              'autoload -Uz up-line-or-beginning-search down-line-or-beginning-search\n'
                              'zle -N up-line-or-beginning-search; zle -N down-line-or-beginning-search\n'
                              f'bindkey -M {keymap} "^[[A" up-line-or-beginning-search\n'
                              f'bindkey -M {keymap} "^[[B" down-line-or-beginning-search\n'
                              f'bindkey -M {keymap} "^[OA" up-line-or-beginning-search\n'
                              f'bindkey -M {keymap} "^[OB" down-line-or-beginning-search\n'
                              f'eval "$(\"{BIN}\" init --print zsh)"\n'
                              'print -s -- "echo HISTORY_ONE"; print -s -- "echo HISTORY_TWO"\n'
                              '_tp_snapshot() { print -r -- "$BUFFER" > "$HOME/state"; }\n'
                              f'zle -N _tp_snapshot; bindkey -M {keymap} "^Xb" _tp_snapshot\n')
                        s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                                         config='mode="on-demand"\nprovider="terminal-test"\n',
                                         capture_output=capture)
                        s.read_until(b"READY> ")
                        # Snapshot only after navigation: a snapshot widget between
                        # presses would interrupt zsh's native prefix search state.
                        s.send_menu(up + up + down + b"\x18b")
                        self.assertEqual(Path(s.home.name, "state").read_text().strip(),
                                         "echo HISTORY_TWO")

    def test_zsh_populated_selector_arrows_return_to_history(self):
        for keymap in ("emacs", "viins"):
            for capture in (False, True):
                for up, down in ((b"\x1b[A", b"\x1b[B"), (b"\x1bOA", b"\x1bOB")):
                    with self.subTest(keymap=keymap, capture=capture, up=up):
                        rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                              'PROMPT="READY> "\nHISTSIZE=1000\n'
                              'HISTFILE="$HOME/.zsh_history"\n'
                              'autoload -Uz up-line-or-beginning-search down-line-or-beginning-search\n'
                              'zle -N up-line-or-beginning-search; zle -N down-line-or-beginning-search\n'
                              f'bindkey -M {keymap} "^[[A" up-line-or-beginning-search\n'
                              f'bindkey -M {keymap} "^[[B" down-line-or-beginning-search\n'
                              f'bindkey -M {keymap} "^[OA" up-line-or-beginning-search\n'
                              f'bindkey -M {keymap} "^[OB" down-line-or-beginning-search\n'
                              f'eval "$(\"{BIN}\" init --print zsh)"\n'
                              # A ready suggestion reply with a populated selector.
                              '_ready_menu() { BUFFER="echo "; CURSOR=${#BUFFER}; '
                              '_TP_ALL_CANDIDATES=("echo SUGGESTION_ONE" "echo SUGGESTION_TWO"); '
                              '_TP_CANDIDATES=("${_TP_ALL_CANDIDATES[@]}"); '
                              '_TP_CANDIDATE_INDEX=1; _TP_MENU_OPEN=1; _TP_REVEALED=1; '
                              '_tp_select_candidate; }\n'
                              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_MENU_OPEN" '
                              '"$_TP_FULL" > "$HOME/state"; }\n'
                              'zle -N _ready_menu; zle -N _tp_snapshot\n'
                              f'bindkey -M {keymap} "^Xp" _ready_menu\n'
                              f'bindkey -M {keymap} "^Xb" _tp_snapshot\n')
                        s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                                         config='mode="on-demand"\nprovider="terminal-test"\n',
                                         capture_output=capture,
                                         history=': 10:0;echo HISTORY_ONE\n: 20:0;echo HISTORY_TWO\n')
                        s.read_until(b"READY> ")
                        # Keep Up/Down consecutive so native prefix search can
                        # retain its original cursor across history entries.
                        s.send_menu(b"\x18p" + up + up + down + b"\x18b")
                        self.assertEqual(Path(s.home.name, "state").read_text().splitlines(),
                                         ["echo HISTORY_TWO", "0", ""])

    def test_tab_selector_browsing_preserves_shell_arrows(self):
        for keymap in ("emacs", "viins"):
            with self.subTest(keymap=keymap):
                rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                      'PROMPT="READY> "\n'
                      '_original_up() { LBUFFER+="UP"; }\n'
                      '_original_down() { LBUFFER+="DOWN"; }\n'
                      'zle -N _original_up; zle -N _original_down\n'
                      f'bindkey -M {keymap} "^[[A" _original_up\n'
                      f'bindkey -M {keymap} "^[[B" _original_down\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$CURSOR" "$_TP_FULL" '
                      '"$_TP_MENU_OPEN" "$_TP_CANDIDATE_INDEX" > "$HOME/state"; '
                      'print -rl -- "${_TP_HIGHLIGHTS[@]}" > "$HOME/colors"; }\n'
                      f'zle -N _tp_snapshot; bindkey -M {keymap} "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc)
                s.read_until(b"READY> ")
                s.send_menu(b"git l")
                s.read_until(b"og")
                s.send_menu(b"\t")
                snapshot = Path(s.home.name, "state")
                for keys, index in ((b"\t", "2"), (b"\t", "3"),
                                    (b"\x1b[Z", "2"), (b"\x1b[Z", "1")):
                    s.send_menu(keys + b"\x18b")
                    state = snapshot.read_text().splitlines()
                    self.assertEqual(state[:2], ["git l", "5"])
                    self.assertEqual(state[3:], ["1", index])
                    colors = Path(s.home.name, "colors").read_text()
                    self.assertEqual(len(selected_rows(colors)), 1)
                    self.assertIn("fg=16,bg=51,bold", colors)
                # Up exits the populated selector and invokes the user's widget.
                s.send_menu(b"\x1b[A\x1b[B\x18b")
                state = snapshot.read_text().splitlines()
                self.assertEqual(state[0], "git lUPDOWN")
                self.assertEqual(state[2:4], ["", "0"])
                s.send_menu(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_hash_description_generates_on_enter_and_requires_explicit_acceptance(self):
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                body = json.dumps({"choices": [{"message": {"content":
                    "$ printf GENERATED_ONE > generated-marker\n$ printf GENERATED_TWO > generated-marker"}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        config = ('provider = "local-test"\nmodel = "test-model"\n[[providers]]\n'
                  f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                  'no_auth = true\n')
        rc = ('PROMPT="READY> "\ncd "$HOME"\n'
              'setopt INTERACTIVE_COMMENTS\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" "$_TP_FULL" '
              '"$_TP_MENU_OPEN" "${#_TP_CANDIDATES}" > "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        s.read_until(b"READY> ")
        start = len(s.data)
        s.send_menu(b"\x1b[200~# ordinary pasted shell comment\x1b[201~\r")
        s.read_until(b"READY> ", after=start)
        s.send_menu(b"# create a marker file")
        time.sleep(.5)
        s.send_menu(b"\x18b")
        self.assertEqual(len(requests), 0)
        snapshot = Path(s.home.name, "state")
        self.assertEqual(snapshot.read_text().splitlines()[3:], ["0", "0"])
        s.send_menu(b"\r")
        s.read_until(b"GENERATED_TWO")
        s.send_menu(b"\t\x18b")
        state = snapshot.read_text().splitlines()
        self.assertEqual(state, ["# create a marker file", "",
                                "printf GENERATED_TWO > generated-marker", "1", "2"])
        self.assertEqual(len(requests), 1)
        self.assertIn("create a marker file", json.dumps(requests[0]))
        self.assertFalse(Path(s.home.name, "generated-marker").exists())
        s.send_menu(b"\r\x18b")
        self.assertEqual(snapshot.read_text().splitlines(),
                         ["printf GENERATED_TWO > generated-marker", "", "", "0", "0"])
        self.assertFalse(Path(s.home.name, "generated-marker").exists())
        s.send_menu(b"\r")
        s.read_until(b"READY> ")
        deadline = time.monotonic() + 2
        while not Path(s.home.name, "generated-marker").exists() and time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            time.sleep(.05)
        self.assertEqual(Path(s.home.name, "generated-marker").read_text(), "GENERATED_TWO")
        s.send_menu(b"exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_pasted_comment_and_installation_execute_as_shell_input(self):
        for keymap in ("emacs", "viins"):
            with self.subTest(keymap=keymap):
                rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                      'PROMPT="READY> "\ncd "$HOME"\nsetopt INTERACTIVE_COMMENTS\n'
                      'brew() { print -r -- "$*" > "$HOME/installed"; }\n'
                      '_test_paste() { print called >> "$HOME/paste-widget"; zle .bracketed-paste; print -r -- PASTE_DONE; }\n'
                      'zle -N bracketed-paste _test_paste\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_install\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_MENU_OPEN" "$_TP_INTENT_BUFFER" > "$HOME/state"; print -r -- SNAPSHOT_READY; }\n'
                      'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc)
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                paste_start = s.send_menu(b"\x1b[200~# Installs the latest release\nbrew install jj\x1b[201~")
                s.read_until(b"PASTE_DONE", after=paste_start)
                snapshot_start = s.send_menu(b"\x18b")
                s.read_until(b"SNAPSHOT_READY", after=snapshot_start)
                self.assertEqual(Path(s.home.name, "state").read_text().splitlines(),
                                 ["# Installs the latest release", "brew install jj", "0", ""])
                self.assertFalse(Path(s.home.name, "installed").exists())
                self.assertEqual(Path(s.home.name, "paste-widget").read_text(), "called\n")
                start = len(s.data)
                s.send_menu(b"\r")
                s.read_until(b"READY> ", after=start)
                self.assertEqual(Path(s.home.name, "installed").read_text(), "install jj\n")
                self.assertNotIn(b"Enter a non-empty task description", s.data)
                s.send(b"exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_cancelled_pasted_command_is_suggested_ahead_of_older_history(self):
        rc = ('PROMPT="READY> "\ncd "$HOME"\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_test_paste() { _tp_bracketed_paste; print -r -- PASTE_DONE; }\n'
              'zle -N bracketed-paste _test_paste\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_FULL" > "$HOME/state"; print -r -- SNAPSHOT_READY; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                         history=": 10:0;brew install tmux\n")
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        paste_start = s.send_menu(b"\x1b[200~# Installs the latest release\nbrew install jj\x1b[201~")
        s.read_until(b"PASTE_DONE", after=paste_start)
        start = len(s.data)
        s.send_menu(b"\x03")
        s.read_until(b"READY> ", after=start)
        start = len(s.data)
        s.send_menu(b"brew \t")
        s.read_until(b"brew install jj", after=start)
        snapshot_start = s.send_menu(b"\x18b")
        s.read_until(b"SNAPSHOT_READY", after=snapshot_start)
        self.assertEqual(Path(s.home.name, "state").read_text().splitlines(), ["brew ", "brew install jj"])
        s.send(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_magic_space_keeps_suggestions_across_words_in_both_keymaps(self):
        for keymap, ai_history in (("emacs", False), ("emacs", True),
                                   ("viins", False), ("viins", True)):
            with self.subTest(keymap=keymap, ai_history=ai_history):
                rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                      f'bindkey -M {keymap} " " magic-space\n'
                      'PROMPT="READY> "\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
                      '"${#_TP_CANDIDATES}" > "$HOME/state"; }\n'
                      f'zle -N _tp_snapshot; bindkey -M {keymap} "^Xb" _tp_snapshot\n')
                history = ': 10:0;docker compose ps --all\n'
                config = ('provider = "terminal-test"\n[context]\n'
                          f'ai_select_history = {str(ai_history).lower()}\n')
                rc = 'HISTFILE="$HOME/.zsh_history"\n' + rc
                s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config, history=history)
                s.read_until(b"READY> ")
                s.send_menu(b"docker")
                s.read_until(b" compose ps --all")
                snapshot = Path(s.home.name, "state")
                for keys, buffer, suffix in ((b" ", "docker ", "compose ps --all"),
                                            (b"compose ", "docker compose ", "ps --all"),
                                            (b"ps ", "docker compose ps ", "--all")):
                    s.send_menu(keys + b"\x18b")
                    state = snapshot.read_text().splitlines()
                    self.assertEqual(state[:2], [buffer, suffix])
                    self.assertGreater(int(state[2]), 0)
                s.send_menu(b"\x1b[C\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[:2],
                                 ["docker compose ps --all", ""])
                self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
                s.send_menu(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_ai_history_mode_refreshes_bare_docker_with_context_and_multiple_options(self):
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                prompt = requests[-1]["messages"][-1]["content"]
                command = "docker compose -f project-compose.yml ps" if "\n$ docker\n" in prompt else "docker ps"
                body = json.dumps({"choices": [{"message": {"content": command}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        config = ('provider = "local-test"\nmodel = "test-model"\n[context]\n'
                  'ai_select_history = true\n[[providers]]\n'
                  f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                  'no_auth = true\n')
        rc = ('cd "$HOME"\ntouch project-compose.yml\nPROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "${#_TP_CANDIDATES}" '
              '"${_TP_CANDIDATES[@]}" > "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        s.read_until(b"READY> ")
        s.send_menu(b"do")
        s.read_until(b"cker ps")
        s.send_menu(b"\x18b")
        snapshot = Path(s.home.name, "state")
        self.assertEqual(snapshot.read_text().splitlines(), ["do", "1", "docker ps"])
        s.send_menu(b"cker")
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            if snapshot.read_text().splitlines()[1] == "3":
                break
            time.sleep(.05)
        self.assertEqual(snapshot.read_text().splitlines(),
                          ["docker", "3", "docker compose -f project-compose.yml ps", "docker ps", "docker images"])
        self.assertEqual(len(requests), 2)
        self.assertIn("project-compose.yml", requests[-1]["messages"][-1]["content"])
        s.send_menu(b"\t\x1b[B\x18b")
        self.assertEqual(snapshot.read_text().splitlines()[0], "docker")
        self.assertEqual(len(requests), 2)
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_selector_displays_provider_failure_and_bare_docker_works_offline(self):
        config = ('provider = "local-test"\nmodel = "test-model"\n[[providers]]\n'
                  'name = "local-test"\nbase_url = "http://127.0.0.1:1/v1"\nno_auth = true\n')
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_ISSUE" "$POSTDISPLAY" '
              '> "$HOME/state"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
        s.read_until(b"READY> ")
        s.send_menu(b"do\t")
        snapshot = Path(s.home.name, "state")
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            if snapshot.read_text().splitlines()[1]:
                break
            time.sleep(.05)
        state = Path(s.home.name, "state").read_text().splitlines()
        self.assertTrue(state[1])
        self.assertIn("No suggestions", "\n".join(state[2:]))
        self.assertIn(state[1][:78], "\n".join(state[2:]))
        s.send_menu(b"\x15docker\t\x18b")
        deadline = time.monotonic() + 2
        while "docker ps" not in Path(s.home.name, "state").read_text() and time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            time.sleep(.05)
        state = Path(s.home.name, "state").read_text()
        self.assertIn("docker ps", state)
        self.assertIn("docker images", state)
        self.assertIn("docker compose ps", state)
        self.assertEqual(state.splitlines()[1], "")
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_cycle_reports_when_only_one_variant_matches(self):
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
              '"${#_TP_CANDIDATES}" > "$HOME/buffer"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc)
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name) / "buffer"
        s.send_menu(b"git stat")
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            time.sleep(.08)
            if snapshot.read_text().splitlines()[1] == "us":
                break
        s.data = b""
        s.send_menu(b"\x1b[Z\x18b")
        self.assertIn(b"TermPlete  1/1", ANSI.sub(b"", s.data))
        time.sleep(.08)
        self.assertEqual(snapshot.read_text().splitlines(), ["git stat", "us", "1"])
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_zsh_cycles_ai_variants_from_one_request_without_changing_input(self):
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                body = json.dumps({"choices": [{"message": {"content":
                    "docker ps\ndocker images\ndocker compose up"}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        config = ('provider = "local-test"\nmodel = "test-model"\n[[providers]]\n'
                  f'name = "local-test"\nbase_url = "http://127.0.0.1:{server.server_port}/v1"\n'
                  'no_auth = true\n')
        rc = ('PROMPT="READY> "\n'
              f'eval "$(\"{BIN}\" init --print zsh)"\n'
              '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
              '"${#_TP_CANDIDATES}" > "$HOME/buffer"; }\n'
              'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, config=config)
        worker.start()
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name) / "buffer"
        s.send_menu(b"do")
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            time.sleep(.08)
            if snapshot.read_text().splitlines()[1] == "cker ps":
                break
        s.send_menu(b"\t")
        for suffix in ("cker ps", "cker images", "cker compose up", "cker ps"):
            s.send_menu(b"\x18b")
            time.sleep(.08)
            self.assertEqual(snapshot.read_text().splitlines(), ["do", suffix, "3"])
            s.send_menu(b"\t")
        self.assertEqual(len(requests), 1)
        s.send_menu(b"\x1b[C\x18b")
        time.sleep(.08)
        self.assertEqual(snapshot.read_text().splitlines()[:2], ["docker images", ""])
        self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_ffmpeg_proposes_and_cycles_useful_commands(self):
        s = self.session(["-i"], executable="/bin/zsh",
                         rc=f'PROMPT="READY> "\neval "$(\"{BIN}\" init --print zsh)"\n')
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        snapshot = Path(s.home.name) / "buffer"
        s.data = b""
        s.send_menu((f'_tp_snapshot() {{ print -r -- "$BUFFER" > {snapshot}; '
                f'print -r -- "$_TP_GHOST" >> {snapshot}; }}; '
                'zle -N _tp_snapshot; bindkey "^Xb" _tp_snapshot\r').encode())
        s.read_until(b"READY> ")
        s.read_until(b"\x1b[?2004h")
        s.send_menu(b"ffmpeg")
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            s.send_menu(b"\x18b")
            if snapshot.read_text() == "ffmpeg\n -i input.mp4 output.mp4\n":
                break
        self.assertEqual(snapshot.read_text(), "ffmpeg\n -i input.mp4 output.mp4\n")
        s.send_menu(b"\t")
        for suffix in (" -i input.mp4 output.mp4", " -i input.mp4 -vn output.mp3",
                       " -i input.mp4 -vf scale=1280:-2 output.mp4", " -i input.mp4 output.mp4"):
            s.send_menu(b"\x18b")
            time.sleep(.1)
            self.assertEqual(snapshot.read_text(), f"ffmpeg\n{suffix}\n")
            s.send_menu(b"\t")
        s.send_menu(b"\x1b[C\x18b")
        time.sleep(.1)
        self.assertEqual(snapshot.read_text(), "ffmpeg -i input.mp4 -vn output.mp3\n\n")
        self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
        s.send_menu(b"\x03exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_interrupt_during_async_reply_or_animation_cancels_input(self):
        cases = [(handler, phase, True) for handler in ("_tp_on_reply", "_tp_on_glow")
                 for phase in ("render", "finished")]
        cases += [("_tp_on_reply", phase, False) for phase in ("render", "finished")]
        for handler, phase, animations in cases:
            with self.subTest(handler=handler, phase=phase, animations=animations):
                interrupt = (f'if [[ $WIDGET == {handler} && $_tp_test_interrupt == 1 ]]; then '
                             '_tp_test_interrupt=0; kill -INT $$; fi;')
                hook = ('_tp_render() { ' + interrupt + ' _tp_test_render; }\n'
                        if phase == "render" else
                        f'functions[_tp_test_callback]=$functions[{handler}]\n'
                        f'{handler}() {{ _tp_test_callback "$@"; ' + interrupt + '\n}\n')
                user_trap = ('trap "print -u2 -- USER_INTERRUPT" INT\n' if phase == "render" else
                             'TRAPINT() { print -u2 -- USER_INTERRUPT; }\n')
                rc = (user_trap + 'PROMPT="READY> "\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      f'_TP_ANIMATIONS={str(animations).lower()}\n'
                      'functions[_tp_test_render]=$functions[_tp_render]\n'
                      '_tp_test_trap() { trap > "$HOME/trap"; '
                      'print -r -- "INJECTED:$_tp_test_interrupt" >> "$HOME/trap"; }; zle -N _tp_test_trap\n'
                      'bindkey "^Xb" _tp_test_trap\n'
                      '_tp_test_interrupt=1\n' + hook)
                s = self.session(["-i"], executable="/bin/zsh", rc=rc)
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                self.assertNotIn(b"parse error", s.data)
                s.data = b""
                s.send_menu(b"git l")
                s.read_until(b"READY> ")
                s.send_menu(b"\x18b")
                self.assertIn("USER_INTERRUPT", Path(s.home.name, "trap").read_text())
                self.assertIn("INJECTED:0", Path(s.home.name, "trap").read_text())
                s.send_menu(b"print -r -- INTERRUPT_OK\r")
                s.read_until(b"\r\nINTERRUPT_OK\r\n")
                s.send_menu(b"exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_interrupt_during_fifo_creation_cancels_input_and_restores_trap(self):
        for stage in ("request", "render"):
            with self.subTest(stage=stage):
                condition = ('[[ $* == *reply.* ]]' if stage == "request" else
                             '[[ $WIDGET == _tp_test_render_interrupt && $* == */glow ]]')
                rc = ('PROMPT="READY> "\ntrap "print -u2 -- USER_INTERRUPT" INT\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_test_interrupt=1\n'
                      f'mkfifo() {{ command mkfifo "$@"; if {condition} && '
                      '(( _tp_test_interrupt )); then _tp_test_interrupt=0; kill -INT $$; fi; }\n'
                      '_tp_test_render_interrupt() { _tp_stop_glow; '
                      '_TP_PULSE=4; _TP_PULSE_UNTIL=$(( EPOCHREALTIME + 0.3 )); _tp_render; }\n'
                      'zle -N _tp_test_render_interrupt; bindkey "^Xt" _tp_test_render_interrupt\n'
                      '_tp_test_trap() { trap > "$HOME/trap"; }; zle -N _tp_test_trap\n'
                      'bindkey "^Xb" _tp_test_trap\n')
                s = self.session(["-i"], executable="/bin/zsh", rc=rc)
                s.read_until(b"READY> ")
                s.read_until(b"\x1b[?2004h")
                start = s.send_menu(b"gi" if stage == "request" else b"git l")
                if stage == "render":
                    s.read_until(b"og", after=start)
                    start = s.send_menu(b"\x18t")
                s.read_until(b"READY> ", after=start)
                s.send_menu(b"\x18b")
                self.assertIn("USER_INTERRUPT", Path(s.home.name, "trap").read_text())
                s.send_menu(b"print -r -- INTERRUPT_OK\r")
                s.read_until(b"\r\nINTERRUPT_OK\r\n", after=start)
                s.send(b"exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_older_zsh_returns_before_installing_or_wrapping(self):
        plugin = Path(__file__).resolve().parents[1] / "shells/termplete.plugin.zsh"
        rc = (f'ZSH_VERSION=5.8.1\nsource "{plugin}"\n'
              'print -r -- "LEGACY_LOADED:${_TERMPLETE_ZSH_LOADED:-0}"\n'
              'PROMPT="READY> "\n')
        s = self.session(["-i"], executable="/bin/zsh", rc=rc, capture_output=True)
        s.read_until(b"READY> ")
        self.assertIn(b"requires zsh 5.9 or newer", s.data)
        self.assertIn(b"LEGACY_LOADED:0", s.data)
        s.send(b"exit 0\r")
        self.assertEqual(s.wait(), 0)

    def test_setup_enables_all_future_zsh_sessions(self):
        config = ('provider = "setup-test"\n[[providers]]\n'
                  'name = "setup-test"\nbase_url = "http://127.0.0.1:1/v1"\n'
                  'no_auth = true\n')
        s = self.session(["setup"], config=config)
        s.read_until(b"1/15")
        s.send(b"\r")
        s.read_until(b"Press enter to keep")
        s.send(b"\r")
        s.read_until(b"model id:")
        s.send(b"test-model\r")
        self.assertEqual(s.wait(), 0)
        rc = (Path(s.home.name) / ".zshrc").read_text()
        self.assertIn('eval "$(tp init --print zsh)"', rc)
        self.assertIn(b"Autocomplete enabled for new zsh sessions", s.data)

    def test_picker_color_paging_and_cancellation(self):
        for terminal, no_color in (("xterm-256color", False), ("vt100", False),
                                    ("screen", False), ("linux", False),
                                    ("tmux-256color", False), ("xterm", True)):
            with self.subTest(terminal=terminal, no_color=no_color):
                s = self.session(["settings"], terminal=terminal, rows=12, cols=35, no_color=no_color)
                s.read_until(b"1/6")
                self.assertEqual(b"\x1b[1;36m" in s.data, not no_color)
                for line in ANSI.sub(b"", s.data).splitlines():
                    # These labels contain a Unicode dash; its three UTF-8
                    # bytes occupy one cell in the terminal.
                    self.assertLessEqual(len(line.decode()), 34)
                s.send(b"\r")
                s.data = b""
                s.read_until(b"1/2")
                self.assertNotIn(b"Off -", s.data)
                s.data = b""
                s.send(b"q")
                s.read_until(b"1/6")
                s.send(b"q")
                self.assertEqual(s.wait(), 0)
                self.assertTrue(termios.tcgetattr(s.fd)[3] & termios.ICANON)

    def test_zsh_saved_shortcuts_reload_and_restore_previous_bindings(self):
        for keymap in ("emacs", "viins"):
            with self.subTest(keymap=keymap):
                rc = (f'bindkey -{"e" if keymap == "emacs" else "v"}\n'
                      'PROMPT="READY> "\n'
                      '_old_key() { LBUFFER+="OLD"; }; zle -N _old_key\n'
                      f'bindkey -M {keymap} "^N" _old_key\n'
                      f'eval "$(\"{BIN}\" init --print zsh)"\n'
                      '_tp_snapshot() { print -rl -- "$BUFFER" "$_TP_GHOST" '
                      '"$_TP_PENDING" "$_TP_CANDIDATE_INDEX" > "$HOME/buffer"; '
                      f'bindkey -M {keymap} "^N" > "$HOME/old-binding"; '
                      f'bindkey -M {keymap} "^Z" > "$HOME/z-binding"; }}\n'
                      f'zle -N _tp_snapshot; bindkey -M {keymap} "^Xb" _tp_snapshot\n')
                base = 'provider = "terminal-test"\nmode = "on-demand"\n'
                s = self.session(["-i"], executable="/bin/zsh", rc=rc,
                                 config=base + '[keys]\nreveal = "^N"\n')
                s.read_until(b"READY> ")
                path = Path(s.home.name, "termplete/config.toml")
                # Same process, a changed configuration, then a fresh prompt.
                path.write_text(base + '[keys]\nreveal = "^Z"\ncycle = "^[[27;6;90~"\n')
                s.data = b""
                s.send_menu(b"\r")
                s.read_until(b"READY> ")
                s.send_menu(b"git l")
                snapshot = Path(s.home.name, "buffer")
                for _ in range(30):
                    s.send_menu(b"\x18b")
                    if snapshot.exists() and snapshot.read_text().splitlines()[2] == "og":
                        break
                self.assertEqual(snapshot.read_text().splitlines()[:3], ["git l", "", "og"])
                s.send_menu(b"\x1a\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[:2], ["git l", "og"])
                s.send_menu(b"\x1b[27;6;90~\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[3], "3")
                self.assertIn("_old_key", Path(s.home.name, "old-binding").read_text())
                self.assertIn("_tp_reveal", Path(s.home.name, "z-binding").read_text())
                s.send_menu(b"\x15\x0e\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[0], "OLD")
                # Reset removes the old custom bindings as well as restoring defaults.
                path.write_text(base)
                s.data = b""
                s.send_menu(b"\x03")
                s.read_until(b"READY> ")
                s.send_menu(b"git l")
                for _ in range(30):
                    s.send_menu(b"\x18b")
                    if snapshot.read_text().splitlines()[2] == "og":
                        break
                s.send_menu(b"\x07\x18b")
                self.assertEqual(snapshot.read_text().splitlines()[:2], ["git l", "og"])
                self.assertNotIn("_tp_reveal", Path(s.home.name, "z-binding").read_text())
                s.send_menu(b"\x03exit 0\r")
                self.assertEqual(s.wait(), 0)

    def test_shortcut_editor_captures_control_and_full_escape_sequences(self):
        for pressed, stored, label in (
            (b"\x1a", "^Z", b"Ctrl+Z"),
            (b"\x1b[27;6;90~", "^[[27;6;90~", b"Ctrl+Shift+Z"),
            (b"\x1b[122;6u", "^[[122;6u", b"Ctrl+Shift+Z"),
        ):
            with self.subTest(pressed=pressed):
                s = self.session(["settings"])
                s.read_until(b"1/6")
                s.data = b""
                s.send_menu(b"\x1b[B\x1b[B\r")
                s.read_until(b"1/7")
                self.assertIn(b"Right Arrow", s.data)
                self.assertIn(b"Shift+Tab", s.data)
                self.assertIn(b"Ctrl+X Ctrl+X", s.data)
                self.assertNotIn(b"^[[C", s.data)
                s.data = b""
                s.send_menu(b"\r")
                s.read_until(b"Press the shortcut now")
                s.data = b""
                s.send_menu(pressed)
                s.read_until(b"1/7")
                self.assertIn(b"Reveal: " + label, s.data)
                self.assertEqual(os.waitpid(s.pid, os.WNOHANG), (0, 0))
                # Capture is a draft until Save and exit.
                self.assertNotIn('reveal =', Path(s.home.name, "termplete/config.toml").read_text())
                s.data = b""
                s.send_menu(b"q")
                s.read_until(b"1/6")
                s.send_menu(b"\x1b[F\r")
                self.assertEqual(s.wait(), 0)
                saved = Path(s.home.name, "termplete/config.toml").read_text()
                self.assertIn(f'reveal = "{stored}"', saved)
                self.assertTrue(termios.tcgetattr(s.fd)[3] & termios.ICANON)
                self.assertTrue(termios.tcgetattr(s.fd)[3] & termios.ISIG)

    def test_shortcut_editor_rejects_incomplete_sequence_without_changing_key(self):
        s = self.session(["settings"])
        s.read_until(b"1/6")
        s.data = b""
        s.send_menu(b"\x1b[B\x1b[B\r")
        s.read_until(b"1/7")
        s.data = b""
        s.send_menu(b"\r")
        s.read_until(b"Press the shortcut now")
        s.data = b""
        s.send_menu(b"\x1b[27;6;")
        s.read_until(b"shortcut not changed")
        s.read_until(b"1/7")
        self.assertIn(b"Reveal: Ctrl+G", s.data)
        s.send_menu(b"q")
        s.read_until(b"1/6")
        s.send_menu(b"q")
        self.assertEqual(s.wait(), 0)

    def test_shortcuts_reset_all_defaults_and_can_be_discarded(self):
        config = ('provider = "terminal-test"\nmode = "on-demand"\n[keys]\n'
                  'reveal = "^N"\naccept = "^Y"\ncycle = "^P"\n'
                  'dismiss = "^D"\ntoggle = "^T"\n')
        for save in (False, True):
            with self.subTest(save=save):
                s = self.session(["settings"], config=config)
                s.read_until(b"1/6")
                s.data = b""
                s.send_menu(b"\x1b[B\x1b[B\r")
                s.read_until(b"1/7")
                s.data = b""
                s.send_menu(b"\x1b[B" * 5 + b"\r")
                s.read_until(b"Shortcuts reset to defaults")
                s.read_until(b"1/7")
                for label in (b"Reveal: Ctrl+G", b"Right Arrow", b"Shift+Tab", b"Ctrl+X Ctrl+X"):
                    self.assertIn(label, s.data)
                self.assertEqual(Path(s.home.name, "termplete/config.toml").read_text(), config)
                s.data = b""
                s.send_menu(b"q")
                s.read_until(b"1/6")
                s.send_menu(b"\x1b[F\r" if save else b"q")
                self.assertEqual(s.wait(), 0)
                saved = Path(s.home.name, "termplete/config.toml").read_text()
                if save:
                    for key, value in (("reveal", "^G"), ("accept", "^[[C"), ("cycle", "^[[Z"),
                                       ("dismiss", "^C"), ("toggle", "^X^X")):
                        self.assertIn(f'{key} = "{value}"', saved)
                    self.assertIn('mode = "on-demand"', saved)
                else:
                    self.assertEqual(saved, config)

    def test_dumb_terminal_uses_numbered_picker_without_escapes(self):
        s = self.session(["settings"], terminal="dumb")
        s.read_until(b"Choose 1-6")
        self.assertNotIn(b"\x1b", s.data)
        s.send(b"q\n")
        self.assertEqual(s.wait(), 0)

    def test_provider_picker_pages_in_a_short_terminal(self):
        s = self.session(["setup"], rows=10, cols=40)
        s.read_until(b"1/14")
        s.data = b""
        s.send(b"\x1b[F")
        s.read_until(b"14/14")
        self.assertIn(b"Custom", s.data)
        s.send(b"q")
        self.assertEqual(s.wait(), 1)

    def test_mode_picker_saves_on_demand(self):
        s = self.session(["settings"])
        s.read_until(b"1/6")
        s.data = b""
        s.send(b"\r")
        s.read_until(b"1/2")
        s.data = b""
        s.send(b"\x1b[B")
        s.read_until(b"2/2")
        s.data = b""
        s.send(b"\r")
        s.read_until(b"1/6")
        self.assertIn(b"[On demand]", s.data)
        s.data = b""
        s.send(b"\x1b[F")
        s.read_until(b"6/6")
        s.send(b"\r")
        self.assertEqual(s.wait(), 0)
        saved = (Path(s.home.name) / "termplete/config.toml").read_text()
        self.assertIn('mode = "on-demand"', saved)

    def test_escape_control_c_and_control_d_restore_terminal(self):
        for key in (b"\x1b", b"\x03", b"\x04"):
            with self.subTest(key=key):
                s = self.session(["settings"])
                s.read_until(b"1/6")
                s.send(key)
                self.assertEqual(s.wait(), 0)
                self.assertTrue(termios.tcgetattr(s.fd)[3] & termios.ICANON)

    def test_api_key_typing_and_paste_are_masked_in_setup_and_login(self):
        config = ('provider = "mask-feedback-test"\n[[providers]]\n'
                  'name = "mask-feedback-test"\nbase_url = "https://example.invalid/v1"\n')
        for args, terminal in ((["setup"], "xterm-256color"),
                               (["auth", "login", "--provider", "mask-feedback-test"], "xterm-256color"),
                               (["auth", "login", "--provider", "mask-feedback-test"], "dumb")):
            with self.subTest(args=args, terminal=terminal):
                s = self.session(args, terminal=terminal, config=config, cols=35)
                if args == ["setup"]:
                    s.read_until(b"1/15")
                    s.data = b""
                    s.send(b"\r")
                s.read_until(b"API key: ")
                s.data = b""
                s.send(b"s")
                s.read_until(b"API key: *")
                self.assertNotIn(b"API key: s", s.data)
                s.data = b""
                s.send(b"k-test-private-input")
                s.read_until(b"API key: ********************")
                self.assertNotIn(b"sk-test-private-input", s.data)
                s.data = b""
                s.send(b"\x15")
                s.read_until(b"API key: ")
                s.send(b"\x03")
                self.assertEqual(s.wait(), 1)
                self.assertTrue(termios.tcgetattr(s.fd)[3] & termios.ECHO)

    def test_key_submission_confirms_receipt_without_echoing_secret(self):
        config = ('provider = "mask-feedback-test"\n[[providers]]\n'
                  'name = "mask-feedback-test"\nbase_url = "https://example.invalid/v1"\n')
        s = self.session(["setup"], config=config)
        s.read_until(b"1/15")
        s.send(b"\r")
        s.read_until(b"API key: ")
        s.data = b""
        s.send(b"fake-private-key\r")
        s.read_until(b"API key received.")
        self.assertNotIn(b"fake-private-key", s.data)
        # Stop before configuration is applied or a credential is stored.
        s.read_until(b"[https://example.invalid/v1]")
        s.send(b"\x03")
        self.assertIn(s.wait(), (1, 130))
        self.assertTrue(termios.tcgetattr(s.fd)[3] & termios.ECHO)

    def test_cli_provider_switch_remembers_models_across_processes(self):
        with tempfile.TemporaryDirectory(prefix="tp-provider-models-") as directory:
            env = dict(os.environ, TERMPLETE_HOME=directory,
                       TERMPLETE_SOCKET=directory + "/socket")

            def config(*args):
                return subprocess.run([BIN, "config", *args], env=env,
                                      capture_output=True, timeout=4,
                                      check=True).stdout.decode().strip()

            config("set", "--key", "provider", "--value", "openai")
            config("set", "--key", "model", "--value", "saved-openai-model")
            config("set", "--key", "provider", "--value", "ollama")
            self.assertEqual(config("get", "--key", "model"), "")
            config("set", "--key", "model", "--value", "saved-local-model")
            config("set", "--key", "provider", "--value", "openai")
            self.assertEqual(config("get", "--key", "model"), "saved-openai-model")
            config("set", "--key", "provider", "--value", "ollama")
            self.assertEqual(config("get", "--key", "model"), "saved-local-model")

    def test_pipe_eof_cancels_instead_of_hanging(self):
        with tempfile.TemporaryDirectory() as directory:
            env = dict(os.environ, TERMPLETE_HOME=directory,
                       TERMPLETE_SOCKET=directory + "/socket")
            result = subprocess.run([BIN, "settings"], input=b"", capture_output=True,
                                    timeout=2, env=env, check=True)
            self.assertNotIn(b"\x1b", result.stdout + result.stderr)
            result = subprocess.run([BIN, "config", "get", "--key", "mode"],
                                    capture_output=True, env=env, check=True)
            self.assertEqual(result.stdout, b"automatic\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
