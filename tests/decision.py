"""Decision/privacy integration checks. Run after `cargo build --release`."""

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


BIN = Path(os.environ.get(
    "TERMPLETE_TEST_BIN",
    str(Path(__file__).resolve().parents[1] / "target/release/tp"),
))


class JsonServer:
    """A loopback-only fake provider that records every request body."""

    def __init__(self, name, respond, events=None):
        self.name = name
        self.respond = respond
        self.calls = []
        self.lock = threading.Lock()
        self.events = events if events is not None else []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                size = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(size)
                try:
                    request = json.loads(raw)
                except (UnicodeDecodeError, json.JSONDecodeError):
                    request = None
                with owner.lock:
                    owner.calls.append({"path": self.path, "body": request,
                                        "authorization": self.headers.get("Authorization")})
                    owner.events.append(owner.name)

                status, response, delay = owner.respond(self.path, request)
                if delay:
                    time.sleep(delay)
                if isinstance(response, bytes):
                    body = response
                else:
                    body = json.dumps(response, separators=(",", ":")).encode()
                try:
                    self.send_response(status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    # Expected when the daemon's bounded decision timeout wins.
                    pass

            def log_message(self, *args):
                pass

        class Server(ThreadingHTTPServer):
            daemon_threads = True
            allow_reuse_address = True

        self.server = Server(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server.server_port}/v1"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=1)


def systemone_reply(request, selected_index=0, share_remote=0.0,
                    omit_context=0.0, probabilities=None):
    criteria = request["questions"].get("best_command", {}).get("criteria", {})
    candidates = [
        {"id": index, "text": criteria[str(index)]}
        for index in range(len(criteria) - (1 if "none" in criteria else 0))
    ]
    answers = {
        "share_remote": {"type": "noul", "noul": share_remote},
        "omit_context": {"type": "noul", "noul": omit_context},
    }
    if candidates:
        ids = [str(candidate["id"]) for candidate in candidates]
        if probabilities is None:
            if selected_index is None:
                probabilities = [0.0] * len(ids) + [1.0]
            else:
                probabilities = [0.0] * (len(ids) + 1)
                probabilities[selected_index] = 0.95
                probabilities[-1] = 0.05
        selected = "none" if selected_index is None else ids[selected_index]
        answers["best_command"] = {
            "type": "choice",
            "choice": selected,
            "probabilities": dict(zip(ids + ["none"], probabilities)),
            "confidence": max(probabilities),
        }
    return {"answers": answers}


def chat_decision_reply(allow_remote, omit_context, ranking):
    return {
        "choices": [{
            "message": {
                "role": "assistant",
                "content": json.dumps({
                    "allow_remote": allow_remote,
                    "omit_context": omit_context,
                    "ranking": ranking,
                }),
            },
        }],
    }


def completion_reply(command="docker ps --no-trunc"):
    return {
        "choices": [{"message": {"role": "assistant", "content": command}}],
    }


class DecisionTests(unittest.TestCase):
    def setUp(self):
        if not BIN.is_file():
            self.fail(f"missing {BIN}; run `cargo build --release` first")
        self.temp = tempfile.TemporaryDirectory(prefix="tp-decision-")
        self.home = Path(self.temp.name)
        self.config_dir = self.home / "termplete"
        self.config_dir.mkdir()
        self.cwd = self.home / "work"
        self.cwd.mkdir()
        self.socket_path = self.home / "daemon.sock"
        self.env = self._isolated_env()
        self.proc = None
        self.request_id = 1
        self.events = []
        self.servers = []
        self.addCleanup(self._cleanup)

    def _isolated_env(self):
        env = dict(os.environ)
        env.update({
            "HOME": str(self.home),
            "TERMPLETE_HOME": str(self.home),
            "TERMPLETE_SOCKET": str(self.socket_path),
            "TMPDIR": str(self.home),
        })
        for key in list(env):
            upper = key.upper()
            if ("API_KEY" in upper or "API_TOKEN" in upper or "ACCESS_KEY" in upper
                    or "SECRET" in upper or "TOKEN" in upper):
                env.pop(key, None)
        for key in ("HISTFILE", "TERMPLETE_HISTFILE", "ZDOTDIR", "TERMPLETE_CAPTURE_ACTIVE"):
            env.pop(key, None)
        env["TERMPLETE_CAPTURE_OUTPUT"] = "0"
        return env

    def server(self, name, respond):
        server = JsonServer(name, respond, self.events)
        self.servers.append(server)
        return server

    def start_daemon(self, strategy, judge_url, generator_url=None,
                     local_only=True, timeout_ms=600, private_paths=(),
                     protocol="system-one", decision_base_url=None,
                     judge_wire="openai-compat", enabled=True, sensitive_context="block",
                     generator_auth=False, ai_select_history=False):
        lines = [
            'mode = "automatic"',
            "",
            "[context]",
            "import_shell_history = false",
            f"ai_select_history = {str(ai_select_history).lower()}",
            "",
            "[daemon]",
            "idle_shutdown_secs = 0",
            "request_timeout_secs = 2",
            "",
            "[decision]",
            f'enabled = {str(enabled).lower()}',
            f'strategy = "{strategy}"',
            f'protocol = "{protocol}"',
            'provider = "judge"',
            'model = "decision-test-model"',
            f'timeout_ms = {timeout_ms}',
            f'local_only = {str(local_only).lower()}',
            f"private_paths = {json.dumps(list(private_paths))}",
        ]
        if generator_url is not None:
            lines[0:0] = ['provider = "generator"', 'model = "generator-test-model"']
        if decision_base_url is not None:
            lines.append(f'base_url = "{decision_base_url}"')
        lines.extend(["", "[privacy]", f'sensitive_context = "{sensitive_context}"'])
        lines.extend([
            "",
            "[[providers]]",
            'name = "judge"',
            f'base_url = "{judge_url}"',
            f'wire = "{judge_wire}"',
            "no_auth = true",
        ])
        if generator_url is not None:
            lines.extend([
                "",
                "[[providers]]",
                'name = "generator"',
                f'base_url = "{generator_url}"',
                'wire = "openai-compat"',
                f"no_auth = {str(not generator_auth).lower()}",
                'env_key = "TERMPLETE_TEST_GENERATOR_API_KEY"',
            ])
            if generator_auth:
                self.env["TERMPLETE_TEST_GENERATOR_API_KEY"] = "synthetic-test-key"
        (self.config_dir / "config.toml").write_text("\n".join(lines) + "\n")
        self.proc = subprocess.Popen(
            [str(BIN), "daemon", "--foreground"],
            cwd=self.cwd,
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                detail = self.proc.stderr.read().decode(errors="replace")
                self.fail(f"daemon exited during startup: {detail}")
            if self.socket_path.exists():
                self.request({"op": "ping"})
                return
            time.sleep(0.02)
        self.fail("daemon did not create its isolated Unix socket")

    def request(self, payload):
        payload = dict(payload)
        payload["id"] = self.request_id
        self.request_id += 1
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(5)
            connection.connect(str(self.socket_path))
            connection.sendall(json.dumps(payload).encode() + b"\n")
            received = bytearray()
            while not received.endswith(b"\n"):
                chunk = connection.recv(65536)
                if not chunk:
                    break
                received.extend(chunk)
        if not received:
            self.fail("daemon closed the socket without a response")
        return json.loads(received)

    def suggest(self, line):
        return self.request({
            "op": "suggest",
            "line": line,
            "cwd": str(self.cwd),
            "mode": "aggressive",
        })

    def record(self, command, output=None, exit_code=0):
        response = self.request({
            "op": "record",
            "command": command,
            "exit_code": exit_code,
            "output": output,
            "cwd": str(self.cwd),
        })
        self.assertEqual(response["op"], "ok")

    def test_failed_command_repair_is_local_and_generator_keeps_its_arguments(self):
        old = "mkdir nuxtmodulefork"
        expected = "mkdir orch-test"
        generator = self.server("generator", lambda path, body: (200, completion_reply(old), 0))
        self.start_daemon("llm", "http://127.0.0.1:1/v1", generator.base_url,
                          enabled=False, ai_select_history=True)
        self.record(old)
        for _ in range(2):
            self.request({"op": "feedback", "prefix": "mkdir", "command": old, "cwd": str(self.cwd)})
        self.record("mnkdir orch-test", "zsh: command not found: mnkdir", exit_code=127)
        local = self.request({"op": "suggest", "line": "mkdir", "cwd": str(self.cwd), "local_only": True})
        self.assertEqual(local["candidates"][0], expected)
        self.assertEqual(generator.calls, [])
        result = self.suggest("mkdir")
        self.assertEqual(result["candidates"][0], expected)
        self.assertEqual(len(generator.calls), 1)
        prompt = json.dumps(generator.calls[0]["body"])
        self.assertIn("mnkdir orch-test", prompt)
        self.assertIn(expected, prompt)
        user = generator.calls[0]["body"]["messages"][1]["content"]
        self.assertIn('"context_match":true', user)
        self.assertNotIn("Choose EXACTLY one", prompt)

    def test_pending_paste_is_ranked_and_sent_to_model_without_counting_a_use(self):
        generator = self.server("generator", lambda path, body: (200, completion_reply("brew install tmux"), 0))
        self.start_daemon("llm", "http://127.0.0.1:1/v1", generator.base_url,
                          enabled=False, ai_select_history=True)
        self.record("brew install tmux")
        self.request({"op": "observe_input", "input": "# Installs the latest release\nbrew install jj", "cwd": str(self.cwd)})
        result = self.suggest("brew")
        self.assertEqual(result["candidates"][0], "brew install jj")
        user = generator.calls[0]["body"]["messages"][1]["content"]
        self.assertIn("[pending paste; not executed]", user)
        metadata = [json.loads(line) for line in user.splitlines() if line.startswith('{"')]
        pending = next(item for item in metadata if item["command"] == "brew install jj")
        self.assertTrue(pending["context_match"])
        self.assertEqual(pending["uses"], 0)
        self.assertEqual(self.suggest("brew install tm")["candidates"][0], "brew install tmux")

    def test_decision_cannot_replace_a_repair_or_new_directory_with_an_old_target(self):
        def prefer_old(path, body):
            criteria = body["questions"]["best_command"]["criteria"]
            return 200, systemone_reply(body, selected_index=1 if "1" in criteria else 0), 0
        judge = self.server("judge", prefer_old)
        self.start_daemon("decision", judge.base_url)
        for name in ["nuxtmodulefork", "orch-test"]:
            (self.cwd / name).mkdir()
        self.record("mkdir nuxtmodulefork")
        self.record("cd nuxtmodulefork")
        self.record("mnkdir orch-test", exit_code=127)
        repair = self.suggest("mkdir")
        self.assertEqual(repair["candidates"][0], "mkdir orch-test")
        self.assertEqual(repair["source"], "decision")
        self.assertTrue(judge.calls[-1]["body"]["state"]["candidate_evidence"]["0"]["context_match"])
        self.record("mkdir orch-test")
        navigation = self.suggest("cd ")
        self.assertEqual(navigation["candidates"][0], "cd orch-test/")
        self.assertEqual(navigation["source"], "decision")
        self.assertTrue(judge.calls[-1]["body"]["state"]["candidate_evidence"]["0"]["context_match"])
        self.assertEqual(self.suggest("cd nu")["candidates"][0], "cd nuxtmodulefork/")

    def test_exact_printed_resume_hint_is_available_without_a_model(self):
        self.start_daemon("llm", "http://127.0.0.1:1/v1", enabled=False)
        self.record("omp update")
        command = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        self.record("omp", "Resume this session with\n" + command)
        result = self.request({"op": "suggest", "line": "omp", "cwd": str(self.cwd), "local_only": True})
        self.assertEqual(result["candidates"][0], command)
        self.assertEqual(result["source"], "terminal-context")
        result = self.request({"op": "suggest", "line": "omp --r", "cwd": str(self.cwd), "local_only": True})
        self.assertEqual(result["candidates"], [command])

    def test_directory_context_is_refreshed_while_the_judge_is_pending(self):
        started, release = threading.Event(), threading.Event()
        def reply(path, body):
            started.set()
            release.wait(1)
            return 200, systemone_reply(body), 0
        judge = self.server("judge", reply)
        self.start_daemon("decision", judge.base_url)
        (self.cwd / "old").mkdir()
        self.record("cd old")
        results = []
        worker = threading.Thread(target=lambda: results.append(self.suggest("cd ")))
        worker.start()
        self.assertTrue(started.wait(1))
        try:
            (self.cwd / "orch-test").mkdir()
            (self.cwd / "old").rmdir()
            self.record("mkdir orch-test")
        finally:
            release.set()
            worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(results[0]["candidates"], ["cd orch-test/"])

    def test_fresh_output_reaches_generator_despite_trusted_history(self):
        command = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        generator = self.server("generator", lambda path, body: (200, completion_reply(command), 0))
        self.start_daemon("llm", "http://127.0.0.1:1/v1", generator.base_url,
                          enabled=False, ai_select_history=True)
        self.record("omp update")
        for _ in range(2):
            self.request({"op": "feedback", "prefix": "omp", "command": "omp update", "cwd": str(self.cwd)})
        # This wording intentionally does not match the local hint parser.
        self.record("omp", "Finished session. Continue it using:\n" + command)
        result = self.suggest("omp")
        self.assertEqual(result["source"], "llm")
        self.assertEqual(result["candidates"][0], command)
        self.assertEqual(len(generator.calls), 1)
        self.assertIn(command, json.dumps(generator.calls[0]["body"]))

    def test_ai_history_selection_includes_unexecuted_printed_hint(self):
        command = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        generator = self.server("generator", lambda path, body: (200, completion_reply(command), 0))
        self.start_daemon("llm", "http://127.0.0.1:1/v1", generator.base_url,
                          enabled=False, ai_select_history=True)
        self.record("omp update")
        self.record("omp", "Resume this session with\n" + command)
        result = self.suggest("omp")
        self.assertEqual(result["source"], "llm")
        self.assertEqual(result["candidates"][0], command)
        self.assertEqual(len(generator.calls), 1)
        self.assertIn(command, json.dumps(generator.calls[0]["body"]))

    def test_old_resume_from_generator_cannot_override_the_latest_printed_id(self):
        old = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        new = "omp --resume 01a113d4-0766-72cf-b033-71928b1dd054"
        generator = self.server("generator", lambda path, body: (200, completion_reply(old), 0))
        self.start_daemon("llm", "http://127.0.0.1:1/v1", generator.base_url, enabled=False)
        self.record(old, "Resume this session with\n" + old)
        self.request({"op": "feedback", "prefix": "omp", "command": old, "cwd": str(self.cwd)})
        self.record("omp", "Resume this session with\n" + new)
        local = self.request({"op": "suggest", "line": "omp", "cwd": str(self.cwd), "local_only": True})
        self.assertEqual(local["candidates"], [new])
        result = self.suggest("omp")
        self.assertEqual(len(generator.calls), 1)
        self.assertIn(new, json.dumps(generator.calls[0]["body"]))
        self.assertEqual(result["candidates"], [new])
        self.assertIsNone(result.get("diagnostic"))

    def test_retained_resume_context_wins_after_restart_age_and_unrelated_output(self):
        old = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        new = "omp --resume 01a113d4-0766-72cf-b033-71928b1dd054"
        captured_at = int(time.time() * 1000) - 36 * 3600 * 1000
        entries = [{"command": "omp", "cwd": str(self.cwd), "seq": 1,
                    "recorded_at": captured_at, "exit_code": 130,
                    "output": ["tsp;f;{\"path\":\"/work/.cache/private\"}",
                               "Resume this session with", "", new]}]
        entries.extend({"command": f"cargo check {i}", "cwd": str(self.cwd),
                        "seq": i + 2, "recorded_at": captured_at + i + 1,
                        "output": ["unrelated output " * 200]} for i in range(12))
        (self.config_dir / "history.json").write_text(json.dumps(entries))
        generator = self.server("generator", lambda path, body:
                                (200, completion_reply("omp update\n" + old.replace("--resume", "-r")), 0))
        self.start_daemon("llm", "http://127.0.0.1:1/v1", generator.base_url,
                          enabled=False, ai_select_history=True)
        self.record(old)
        self.record("omp update")
        result = self.suggest("omp")
        self.assertEqual(result["candidates"][0], new)
        self.assertNotIn(old.replace("--resume", "-r"), result["candidates"])
        body = json.dumps(generator.calls[0]["body"])
        self.assertIn(new, body)
        self.assertNotIn("/work/.cache/private", body)
        self.assertEqual(self.suggest("omp -r")["candidates"][0], new.replace("--resume", "-r"))

    def test_decision_cannot_put_history_above_exact_current_context(self):
        command = "omp --resume 01a113d4-0766-72cf-b033-71928b1dd054"
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body, selected_index=1), 0))
        self.start_daemon("decision", judge.base_url)
        self.record("omp update")
        self.record("omp", "Resume this session with\n" + command)
        result = self.suggest("omp")
        self.assertEqual(result["candidates"][0], command)
        self.assertIn("omp update", result["candidates"])

    def test_new_capture_supersedes_an_inflight_model_snapshot(self):
        old = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        new = "omp --resume 01a113d4-0766-72cf-b033-71928b1dd054"
        started, release = threading.Event(), threading.Event()
        def reply(path, body):
            started.set()
            release.wait(1)
            return 200, completion_reply(old), 0
        generator = self.server("generator", reply)
        self.start_daemon("llm", "http://127.0.0.1:1/v1", generator.base_url, enabled=False)
        self.record("omp", "Resume this session with\n" + old)
        results = []
        worker = threading.Thread(target=lambda: results.append(self.suggest("omp")))
        worker.start()
        self.assertTrue(started.wait(1))
        try:
            self.record("omp", "Resume this session with\n" + new)
        finally:
            release.set()
            worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(results[0]["candidates"], [new])

    def test_decision_receives_printed_hint_with_fresh_context_evidence(self):
        command = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body), 0))
        self.start_daemon("decision", judge.base_url)
        self.record("omp update")
        self.record("omp", "Resume this session with\n" + command)
        result = self.suggest("omp")
        self.assertEqual(result["source"], "decision")
        self.assertEqual(result["candidates"][0], command)
        body = judge.calls[0]["body"]
        self.assertEqual(body["questions"]["best_command"]["criteria"]["0"], command)
        evidence = body["state"]["candidate_evidence"]["0"]
        self.assertTrue(evidence["context_match"])
        self.assertLess(evidence["context_age_ms"], 5000)
        self.assertEqual(evidence["uses"], 0)
        self.assertIn(command, json.dumps(body["state"]["user_messages"]))

    def test_private_directory_resume_hint_stays_local(self):
        command = "omp --resume 01a113d4-0766-72cf-b033-71928b1dd054"
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(command), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url,
                          private_paths=(str(self.cwd),))
        self.record("omp", "Resume this session with\n" + command)
        self.assertEqual(self.suggest("omp")["candidates"][0], command)
        self.assertEqual(judge.calls, [])
        self.assertEqual(generator.calls, [])

    def test_fresh_context_marks_local_and_hidden_shell_requests_for_refinement(self):
        command = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7"
        generator = self.server("generator", lambda path, body: (200, completion_reply(command), 0))
        self.start_daemon("llm", "http://127.0.0.1:1/v1", generator.base_url, enabled=False)
        self.record("omp update")
        self.record("omp", "Resume this session with\n" + command)
        for local_only in (True, False):
            result = self.request({"op": "suggest", "line": "omp", "cwd": str(self.cwd),
                                   "local_only": local_only, "mode": "on-demand"})
            self.assertEqual(result["source"], "context-refine")
            self.assertEqual(result["candidates"][0], command)
        self.assertEqual(generator.calls, [])
        result = subprocess.run([str(BIN), "suggest", "--local-only", "--shell-reply", "--", "omp"],
                                cwd=self.cwd, env=self.env, capture_output=True, text=True, check=True)
        self.assertTrue(result.stdout.startswith("R\t" + command))

    def seed_docker(self):
        for command in ("docker ps", "docker images", "docker compose ps"):
            self.record(command)

    def _cleanup(self):
        if self.proc is not None and self.proc.poll() is None:
            try:
                self.request({"op": "shutdown"})
                self.proc.wait(timeout=2)
            except (OSError, subprocess.TimeoutExpired):
                self.proc.send_signal(signal.SIGTERM)
                try:
                    self.proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=2)
        for server in reversed(self.servers):
            server.close()
        if self.proc is not None and self.proc.stderr is not None:
            self.proc.stderr.close()
        self.temp.cleanup()

    def test_disabled_helper_never_receives_requests_and_ordinary_ai_still_works(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("decision", judge.base_url, generator.base_url, enabled=False)
        result = self.suggest("do")
        self.assertEqual(result["source"], "llm")
        self.assertEqual(judge.calls, [])
        self.assertEqual(len(generator.calls), 1)
        self.assertEqual(self.events, ["generator"])

    def test_block_policy_keeps_private_metadata_out_of_both_models(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body, selected_index=None, share_remote=.99), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        self.seed_docker()
        self.record("echo /work/.cache-project/METADATA_SENTINEL")
        result = self.suggest("docker")
        self.assertEqual(result["source"], "llm")
        self.assertNotIn("METADATA_SENTINEL", json.dumps(judge.calls + generator.calls))

    def test_local_review_only_exposes_private_metadata_to_the_local_judge(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body, selected_index=None, share_remote=.99), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url, sensitive_context="decision")
        self.seed_docker()
        self.record("echo /work/.cache-project/METADATA_SENTINEL")
        self.record("cat ~/.ssh/config CREDENTIAL_PATH_SENTINEL")
        self.record("echo API_KEY=ACTUAL_CREDENTIAL_SENTINEL")
        result = self.suggest("docker")
        self.assertEqual(result["source"], "llm")
        self.assertIn("METADATA_SENTINEL", json.dumps(judge.calls))
        self.assertNotIn("METADATA_SENTINEL", json.dumps(generator.calls))
        for marker in ["CREDENTIAL_PATH_SENTINEL", "ACTUAL_CREDENTIAL_SENTINEL"]:
            self.assertNotIn(marker, json.dumps(judge.calls + generator.calls))

    def test_disabled_helper_cannot_review_a_private_typed_prefix(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url, enabled=False, sensitive_context="decision")
        result = self.suggest("tool /work/.cache-project/")
        self.assertTrue(result["diagnostic"])
        self.assertEqual(judge.calls + generator.calls, [])

    def test_judge_cannot_approve_a_private_typed_prefix_for_the_generator(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body, selected_index=None, share_remote=.99), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url, sensitive_context="decision")
        result = self.suggest("tool /work/.cache-project/")
        self.assertTrue(result["diagnostic"])
        self.assertEqual(len(judge.calls), 1)
        self.assertEqual(generator.calls, [])

    def test_decision_only_needs_no_generator_and_ranks_three_known_commands(self):
        judge = self.server(
            "judge",
            lambda path, body: (200, systemone_reply(
                body,
                selected_index=1,
                share_remote=0.0,
                omit_context=0.0,
                probabilities=[0.10, 0.70, 0.15, 0.05],
            ), 0),
        )
        # No top-level provider is configured: known choices work on their own.
        self.start_daemon("decision", judge.base_url)
        self.seed_docker()

        first = self.suggest("docker")
        second = self.suggest("docker")

        self.assertEqual(first["source"], "decision")
        self.assertEqual(first["candidates"], second["candidates"])
        self.assertEqual(len(first["candidates"]), 3)
        self.assertEqual(len(judge.calls), 1, "identical prefix should hit the decision cache")
        call = judge.calls[0]
        self.assertEqual(call["path"], "/v1/systemone")
        body = call["body"]
        self.assertEqual(body["model"], "decision-test-model")
        self.assertEqual(body["keep_alive"], "5m")
        self.assertEqual(
            set(body["questions"]),
            {"share_remote", "omit_context", "best_command"},
        )
        candidates = [
            {"id": index, "text": body["questions"]["best_command"]["criteria"][str(index)]}
            for index in range(3)
        ]
        self.assertEqual(len(candidates), 3)
        self.assertEqual(
            body["questions"]["best_command"]["criteria"]["1"],
            candidates[1]["text"],
        )
        expected_ids = [1, 2, 0]
        self.assertEqual(
            first["candidates"],
            [candidates[index]["text"] for index in expected_ids],
        )

    def test_hybrid_judge_can_deny_remote_generation(self):
        judge = self.server(
            "judge",
            lambda path, body: (200, systemone_reply(
                body, selected_index=None, share_remote=0.89, omit_context=0.0,
            ), 0),
        )
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        self.seed_docker()

        result = self.suggest("docker")

        self.assertNotEqual(result["source"], "llm")
        self.assertEqual(len(judge.calls), 1)
        self.assertEqual(generator.calls, [], "a privacy denial must fail closed")

    def test_directory_judge_receives_freshness_and_relevant_terminal_context(self):
        for name in ("a-old", "b-old", "c-old", "z-active"):
            (self.cwd / name).mkdir()
        judge = self.server("judge", lambda path, body: (
            200, systemone_reply(body, selected_index=1), 0,
        ))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        self.record("cd z-active")
        self.record("git status", "working tree clean")
        result = self.suggest("cd ")
        self.assertEqual(result["source"], "decision")
        self.assertEqual(generator.calls, [])
        body = judge.calls[0]["body"]
        criteria = body["questions"]["best_command"]["criteria"]
        self.assertEqual(criteria["0"], "cd z-active/")
        self.assertEqual(result["candidates"][0], criteria["1"])
        evidence = body["state"]["candidate_evidence"]["0"]
        self.assertEqual(evidence["uses"], 1)
        self.assertIsNotNone(evidence["last_used_age_ms"])
        self.assertGreater(evidence["local_score"], 0)
        self.assertIn("working tree clean", json.dumps(body["state"]["user_messages"]))
        self.assertEqual(len(result["candidates"]), 3)

    def test_directory_judge_abstention_never_generates_an_invented_directory(self):
        (self.cwd / "project").mkdir()
        judge = self.server("judge", lambda path, body: (
            200, systemone_reply(body, selected_index=None, share_remote=.99), 0,
        ))
        generator = self.server("generator", lambda path, body: (200, completion_reply("cd invented/"), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        result = self.suggest("cd p")
        self.assertEqual(result["candidates"], ["cd project/"])
        self.assertEqual(result["source"], "filesystem")
        self.assertEqual(len(judge.calls), 1)
        self.assertEqual(generator.calls, [])

    def test_openai_compatible_decision_adapter_uses_the_same_ranking_contract(self):
        judge = self.server(
            "judge", lambda path, body: (200, chat_decision_reply(False, True, [1, 0, 2]), 0),
        )
        self.start_daemon("decision", judge.base_url, protocol="chat")
        self.seed_docker()
        result = self.suggest("docker")
        self.assertEqual(result["source"], "decision")
        self.assertEqual(len(result["candidates"]), 3)
        self.assertEqual(judge.calls[0]["path"], "/v1/chat/completions")
        body = judge.calls[0]["body"]
        self.assertEqual(body["response_format"], {"type": "json_object"})
        self.assertEqual(body["temperature"], 0)
        self.assertNotIn("stop", body)

    def test_anthropic_chat_adapter_preserves_native_messages_transport(self):
        decision = json.dumps({"allow_remote": False, "omit_context": True, "ranking": [1, 0, 2]})
        judge = self.server(
            "judge", lambda path, body: (200, {"content": [{"type": "text", "text": decision}]}, 0),
        )
        self.start_daemon("decision", judge.base_url, protocol="chat", judge_wire="anthropic")
        self.seed_docker()
        result = self.suggest("docker")
        self.assertEqual(result["source"], "decision")
        self.assertEqual(len(result["candidates"]), 3)
        self.assertEqual(judge.calls[0]["path"], "/v1/messages")
        body = judge.calls[0]["body"]
        self.assertIn("ranking", body["system"])
        self.assertEqual(body["temperature"], 0)
        self.assertNotIn("response_format", body)

    def test_chat_decision_plus_ai_generates_hash_task(self):
        judge = self.server("judge", lambda path, body: (200, chat_decision_reply(True, False, []), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply("$ ffmpeg -i dog.mp4 dog.mov"), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url, protocol="chat")
        result = self.request({"op": "generate", "instruction": "# change dog.mp4 into dog.mov with ffmpeg"})
        self.assertEqual(result["source"], "llm")
        self.assertEqual(result["candidates"], ["ffmpeg -i dog.mp4 dog.mov"])
        self.assertEqual(self.events, ["judge", "generator"])
        self.assertEqual(judge.calls[0]["path"], "/v1/chat/completions")
        self.assertEqual(generator.calls[0]["path"], "/v1/chat/completions")

    def test_hybrid_task_descriptions_check_privacy_without_candidate_generation_by_judge(self):
        judge = self.server(
            "judge", lambda path, body: (200, systemone_reply(body, share_remote=.99), 0),
        )
        generator = self.server(
            "generator", lambda path, body: (200, completion_reply("$ find . -type f -size +100M"), 0),
        )
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        result = self.request({"op": "generate", "instruction": "# find large files", "cwd": str(self.cwd)})
        self.assertEqual(result["source"], "llm")
        self.assertEqual(result["candidates"], ["find . -type f -size +100M"])
        self.assertEqual(set(judge.calls[0]["body"]["questions"]), {"share_remote", "omit_context"})
        self.assertEqual(self.events, ["judge", "generator"])

    def test_malformed_required_private_review_fails_closed(self):
        judge = self.server("judge", lambda path, body: (200, b"not-json", 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url, sensitive_context="decision")
        self.seed_docker()

        result = self.suggest("docker")

        self.assertNotEqual(result["source"], "llm")
        self.assertTrue(result["diagnostic"])
        self.assertEqual(len(judge.calls), 1)
        self.assertEqual(generator.calls, [])

    def test_slow_required_private_review_times_out_and_fails_closed(self):
        judge = self.server(
            "judge",
            lambda path, body: (200, systemone_reply(body), 0.7),
        )
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url, timeout_ms=250, sensitive_context="decision")
        self.seed_docker()

        result = self.suggest("docker")

        self.assertNotEqual(result["source"], "llm")
        self.assertTrue(result["diagnostic"])
        self.assertEqual(len(judge.calls), 1)
        self.assertEqual(generator.calls, [])

    def test_hybrid_timeout_generates_hash_task_and_cooldown_does_not_block_ai(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body), 0.7))
        generator = self.server("generator", lambda path, body: (
            200, completion_reply("$ ffmpeg -i dog.mp4 dog.mov"), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url,
                          timeout_ms=250, generator_auth=True)
        self.record("printf OLD_COMMAND_SENTINEL", output="OLD_OUTPUT_SENTINEL")
        for task in ("# change dog.mp4 into dog.mov with ffmpeg",
                     "# convert dog.mp4 to dog.mov using ffmpeg"):
            result = self.request({"op": "generate", "instruction": task, "cwd": str(self.cwd)})
            self.assertEqual(result["source"], "llm")
            self.assertEqual(result["candidates"], ["ffmpeg -i dog.mp4 dog.mov"])
            self.assertIsNone(result.get("diagnostic"))
        self.assertEqual(len(judge.calls), 1, "cooldown must skip another slow helper call")
        self.assertEqual(len(generator.calls), 2)
        for call in generator.calls:
            self.assertEqual(call["path"], "/v1/chat/completions")
            self.assertEqual(call["authorization"], "Bearer synthetic-test-key")
            serialized = json.dumps(call["body"])
            self.assertIn("dog.mp4", serialized)
            for value in ("OLD_COMMAND_SENTINEL", "OLD_OUTPUT_SENTINEL", str(self.cwd)):
                self.assertNotIn(value, serialized)

    def test_hybrid_invalid_helper_falls_back_to_prefix_ai_without_history_restriction(self):
        judge = self.server("judge", lambda path, body: (200, b"not-json", 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        self.seed_docker()
        self.record("printf OLD_CONTEXT_SENTINEL", output="OLD_OUTPUT_SENTINEL")
        result = self.suggest("docker")
        self.assertEqual(result["source"], "llm")
        self.assertIn("docker ps --no-trunc", result["candidates"])
        self.assertIsNone(result.get("diagnostic"))
        self.assertEqual(len(generator.calls), 1)
        serialized = json.dumps(generator.calls[0]["body"])
        for value in ("OLD_CONTEXT_SENTINEL", "OLD_OUTPUT_SENTINEL", str(self.cwd), "docker compose ps"):
            self.assertNotIn(value, serialized)

    def test_busy_helper_does_not_block_another_shells_hash_request(self):
        entered = threading.Event()
        def slow_judge(path, body):
            entered.set()
            return 200, systemone_reply(body), 1.0
        judge = self.server("judge", slow_judge)
        generator = self.server("generator", lambda path, body: (200, completion_reply("$ ffmpeg -i dog.mp4 dog.mov"), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url, timeout_ms=500)
        from concurrent.futures import ThreadPoolExecutor
        with ThreadPoolExecutor(max_workers=1) as pool:
            pending = pool.submit(self.request, {"op": "generate", "instruction": "# convert dog.mp4 to dog.mov"})
            self.assertTrue(entered.wait(2))
            result = self.request({"op": "generate", "instruction": "# change dog.mp4 into dog.mov with ffmpeg"})
            self.assertEqual(result["source"], "llm")
            self.assertEqual(pending.result(timeout=3)["source"], "llm")
        self.assertEqual(len(judge.calls), 1)
        self.assertEqual(len(generator.calls), 2)

    def test_hash_local_review_keeps_soft_metadata_local_and_filters_generator(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body, share_remote=.99), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply("$ ffmpeg -i dog.mp4 dog.mov"), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url, sensitive_context="decision")
        self.record("echo /work/.cache-project/LOCAL_REVIEW_SENTINEL")
        result = self.request({"op": "generate", "instruction": "# convert dog.mp4 to dog.mov", "cwd": str(self.cwd)})
        self.assertEqual(result["source"], "llm")
        self.assertIn("LOCAL_REVIEW_SENTINEL", json.dumps(judge.calls))
        self.assertNotIn("LOCAL_REVIEW_SENTINEL", json.dumps(generator.calls))

    def test_fallback_reports_generator_error_instead_of_helper_error(self):
        judge = self.server("judge", lambda path, body: (200, b"not-json", 0))
        generator = self.server("generator", lambda path, body: (401, {"error": {"message": "Invalid test API key"}}, 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        result = self.request({"op": "generate", "instruction": "# convert dog.mp4 to dog.mov"})
        self.assertTrue(result["diagnostic"])
        self.assertIn("401", result["diagnostic"])
        self.assertNotIn("Decision helper", result["diagnostic"])
        self.assertEqual(len(generator.calls), 1)

    def test_decision_only_timeout_does_not_generate_hash_tasks(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body), 0.7))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("decision", judge.base_url, generator.base_url, timeout_ms=250)
        result = self.request({"op": "generate", "instruction": "# convert dog.mp4 to dog.mov"})
        self.assertTrue(result["diagnostic"])
        self.assertEqual(generator.calls, [])

    def test_hybrid_privacy_denial_blocks_hash_generation(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body, share_remote=0.0), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        result = self.request({"op": "generate", "instruction": "# convert dog.mp4 to dog.mov"})
        self.assertIn("privacy", result["diagnostic"])
        self.assertEqual(generator.calls, [])

    def test_hybrid_sensitive_hash_task_never_reaches_models(self):
        judge = self.server("judge", lambda path, body: (200, b"not-json", 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        result = self.request({"op": "generate", "instruction": "# curl -H 'Authorization: Bearer SECRET_SENTINEL'"})
        self.assertTrue(result["diagnostic"])
        self.assertEqual(judge.calls, [])
        self.assertEqual(generator.calls, [])

    def test_hybrid_abstention_allows_generation_and_omits_old_context(self):
        judge = self.server(
            "judge",
            lambda path, body: (200, systemone_reply(
                body, selected_index=None, share_remote=0.99, omit_context=1.0,
            ), 0),
        )
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)
        self.seed_docker()
        self.record("printf UNIQUE_OLD_CONTEXT_MARKER")

        result = self.suggest("docker")

        self.assertEqual(result["source"], "llm")
        self.assertEqual(len(judge.calls), 1)
        self.assertEqual(len(generator.calls), 1)
        self.assertEqual(generator.calls[0]["path"], "/v1/chat/completions")
        serialized = json.dumps(generator.calls[0]["body"])
        self.assertNotIn("UNIQUE_OLD_CONTEXT_MARKER", serialized)
        self.assertEqual(self.events[-2:], ["judge", "generator"])

    def test_sensitive_typed_prefix_never_reaches_either_model(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body), 0))
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon("hybrid", judge.base_url, generator.base_url)

        result = self.suggest("curl -H 'Authorization: Bearer PRIVATE_TOKEN_SENTINEL'")

        self.assertTrue(result["diagnostic"])
        self.assertEqual(judge.calls, [])
        self.assertEqual(generator.calls, [])

    def test_configured_private_paths_and_env_context_never_reach_any_wire(self):
        private_path = "/work/restricted-client"
        judge = self.server(
            "judge",
            lambda path, body: (200, systemone_reply(
                body, selected_index=None, share_remote=0.99, omit_context=0.0,
            ), 0),
        )
        generator = self.server("generator", lambda path, body: (200, completion_reply(), 0))
        self.start_daemon(
            "hybrid", judge.base_url, generator.base_url,
            private_paths=[private_path],
        )
        self.seed_docker()
        self.record(
            "cat " + private_path + "/report.txt",
            output=private_path + "/report.txt PRIVATE_CONTEXT_SENTINEL\ncat .env HIDDEN_CONTEXT_SENTINEL",
        )

        self.suggest("docker")

        self.assertEqual(len(judge.calls), 1)
        self.assertEqual(len(generator.calls), 1)
        serialized = json.dumps([call["body"] for call in judge.calls + generator.calls])
        for private_value in (private_path, ".env", "PRIVATE_CONTEXT_SENTINEL", "HIDDEN_CONTEXT_SENTINEL"):
            self.assertNotIn(private_value, serialized)

    def test_remote_decision_endpoint_is_rejected_when_local_only_is_enabled(self):
        judge = self.server("judge", lambda path, body: (200, systemone_reply(body), 0))
        self.start_daemon(
            "decision", judge.base_url, local_only=True,
            decision_base_url="https://decision.example.invalid/v1",
        )
        self.seed_docker()

        result = self.suggest("docker")

        self.assertNotEqual(result["source"], "decision")
        self.assertTrue(result["diagnostic"])
        self.assertEqual(judge.calls, [], "local-only config must reject before network I/O")


if __name__ == "__main__":
    unittest.main(verbosity=2)
