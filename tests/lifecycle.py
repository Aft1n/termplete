"""CLI lifecycle checks; run after `cargo build --release` with python3 tests/lifecycle.py."""

import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("TERMPLETE_TEST_BIN", ROOT / "target/release/tp")).resolve()


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        if not BIN.is_file():
            self.fail(f"missing {BIN}; build the release binary first")
        self.temp = tempfile.TemporaryDirectory(prefix="tp-lifecycle-")
        self.home = Path(self.temp.name)
        self.config_dir = self.home / "termplete"
        self.config_dir.mkdir()
        self.config_path = self.config_dir / "config.toml"
        self.socket_path = self.home / "daemon.sock"
        self.env = dict(os.environ)
        self.env.update({
            "HOME": str(self.home),
            "TERMPLETE_HOME": str(self.home),
            "TERMPLETE_SOCKET": str(self.socket_path),
            "TMPDIR": str(self.home),
        })
        for key in list(self.env):
            upper = key.upper()
            if any(secret in upper for secret in ("API_KEY", "API_TOKEN", "ACCESS_KEY", "SECRET")):
                self.env.pop(key, None)
        self.write_config(enabled=True)
        self.addCleanup(self.cleanup)

    def write_config(self, enabled):
        self.config_path.write_text(
            f"enabled = {str(enabled).lower()}\n"
            "[context]\nimport_shell_history = false\n"
            "[daemon]\nautostart = true\nidle_shutdown_secs = 600\n"
        )

    def run_tp(self, *args):
        return subprocess.run(
            [str(BIN), *args], env=self.env, capture_output=True, text=True, timeout=15
        )

    def daemon_is_live(self):
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                connection.settimeout(0.1)
                connection.connect(str(self.socket_path))
            return True
        except OSError:
            return False

    def wait_for_daemon(self, live, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.daemon_is_live() == live:
                return True
            time.sleep(0.025)
        return self.daemon_is_live() == live

    def cleanup(self):
        try:
            if self.daemon_is_live():
                self.run_tp("service", "stop")
                self.wait_for_daemon(False, timeout=2)
        finally:
            self.temp.cleanup()

    def test_stop_persists_pause_and_prevents_suggestion_autostart(self):
        stopped = self.run_tp("stop")
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertIn("TermPlete paused", stopped.stdout)
        self.assertIn("older wrappers", stopped.stdout.lower())
        self.assertIn("enabled = false", self.config_path.read_text())
        self.assertFalse(self.daemon_is_live())

        suggestion = self.run_tp("suggest", "git ", "--shell-reply")
        self.assertEqual(suggestion.returncode, 0, suggestion.stderr)
        self.assertFalse(self.daemon_is_live(), "a paused suggestion must not autostart the daemon")

        repeated = self.run_tp("stop")
        self.assertEqual(repeated.returncode, 0, repeated.stderr)
        self.assertIn("daemon   was not running", repeated.stdout)
        self.assertIn("enabled = false", self.config_path.read_text())

    def test_stop_shuts_down_a_live_daemon(self):
        started = self.run_tp("service", "start")
        self.assertEqual(started.returncode, 0, started.stderr)
        self.assertTrue(self.wait_for_daemon(True), started.stdout + started.stderr)

        stopped = self.run_tp("stop")
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertIn("daemon   stopped", stopped.stdout)
        self.assertIn("enabled = false", self.config_path.read_text())
        self.assertTrue(self.wait_for_daemon(False), "stop must wait for the daemon to exit")

    def test_start_restores_enabled_and_starts_the_daemon(self):
        self.write_config(enabled=False)
        started = self.run_tp("start")
        self.assertEqual(started.returncode, 0, started.stderr)
        self.assertIn("TermPlete resumed", started.stdout)
        self.assertIn("daemon   started", started.stdout)
        self.assertIn("enabled = true", self.config_path.read_text())
        self.assertTrue(self.wait_for_daemon(True), started.stdout + started.stderr)

    def test_service_start_refuses_when_globally_paused(self):
        self.write_config(enabled=False)

        started = self.run_tp("service", "start")

        self.assertNotEqual(started.returncode, 0, started.stdout + started.stderr)
        self.assertIn("TermPlete is paused", started.stderr)
        self.assertFalse(self.daemon_is_live(), "a paused service start must not leave a socket")

    def test_configuration_pause_stops_instead_of_restarting_the_daemon(self):
        started = self.run_tp("start")
        self.assertEqual(started.returncode, 0, started.stderr)
        stopped = self.run_tp("config", "set", "--key", "enabled", "--value", "false")
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertFalse(self.daemon_is_live(), "pausing must not restart the daemon")

    def test_foreground_daemon_exits_without_listening_when_globally_paused(self):
        self.write_config(enabled=False)

        daemon = self.run_tp("daemon", "--foreground")

        self.assertEqual(daemon.returncode, 0, daemon.stderr)
        self.assertFalse(self.daemon_is_live(), "a paused daemon must not leave a socket behind")

    def test_start_reuses_a_running_daemon(self):
        started = self.run_tp("start")
        self.assertEqual(started.returncode, 0, started.stderr)
        self.assertTrue(self.wait_for_daemon(True), started.stdout + started.stderr)

        self.write_config(enabled=False)
        resumed = self.run_tp("start")
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertIn("daemon   already running", resumed.stdout)
        self.assertIn("enabled = true", self.config_path.read_text())
        self.assertTrue(self.daemon_is_live())


if __name__ == "__main__":
    unittest.main()
