"""Release automation checks with real temporary Git repositories and a fake GitHub CLI."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
GH_STUB = '''#!/usr/bin/env python3
import os
from pathlib import Path
import subprocess
import sys
import tempfile

args = sys.argv[1:]
def git(*args, cwd=None):
    return subprocess.check_output(["git", *args], cwd=cwd, text=True).strip()

if args[:2] == ["auth", "status"]:
    sys.exit(int(os.environ.get("RELEASE_AUTH_FAIL", "0")))
elif args[:2] == ["repo", "view"]:
    print("aft1n/termplete")
elif args[:2] == ["run", "list"]:
    print("123")
elif args[:2] == ["run", "watch"]:
    assert "--exit-status" in args
    if os.environ.get("RELEASE_WATCH_FAIL"):
        sys.exit(1)
    tag = git("describe", "--tags", "--abbrev=0")
    with tempfile.TemporaryDirectory(prefix="formula-bot-") as directory:
        git("clone", os.environ["RELEASE_REMOTE"], directory)
        git("config", "user.name", "Fixture Bot", cwd=directory)
        git("config", "user.email", "bot@example.invalid", cwd=directory)
        formula = Path(directory, "Formula/termplete.rb")
        formula.parent.mkdir(exist_ok=True)
        formula.write_text("# Fixture formula for " + tag + "\\n")
        git("add", "Formula/termplete.rb", cwd=directory)
        git("commit", "-m", "chore(homebrew): update " + tag, cwd=directory)
        git("push", "origin", "main", cwd=directory)
elif args[:2] == ["release", "view"]:
    print("https://github.com/aft1n/termplete/releases/tag/" + args[2])
else:
    raise SystemExit("Unexpected gh command: " + repr(args))
'''


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="termplete-release-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "checkout"
        self.repo.mkdir()
        self.remote = self.root / "remote.git"
        stub_bin = self.root / "bin"
        stub_bin.mkdir()
        (stub_bin / "gh").write_text(GH_STUB)
        (stub_bin / "gh").chmod(0o755)
        self.env = dict(os.environ, PATH=str(stub_bin) + os.pathsep + os.environ["PATH"],
                        RELEASE_REMOTE=str(self.remote), GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_CONFIG_SYSTEM=os.devnull)
        self.git("init", "--bare", "-b", "main", str(self.remote))
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Release Fixture")
        self.git("config", "user.email", "release@example.invalid")
        self.prepare_files()
        self.git("add", ".")
        self.git("commit", "-m", "feat: initial fixture")
        self.initial_commit = self.git("rev-parse", "HEAD")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "-u", "origin", "main")

    def prepare_files(self):
        scripts = self.repo / "scripts"
        scripts.mkdir()
        for name in ("publish.sh", "release.sh", "bump-version.sh"):
            shutil.copy2(ROOT / "scripts" / name, scripts / name)
        (self.repo / "Cargo.toml").write_text('[package]\nname = "termplete"\nversion = "0.1.4"\n')
        (self.repo / "Cargo.lock").write_text('[[package]]\nname = "termplete"\nversion = "0.1.4"\n')
        (self.repo / "npm").mkdir()
        (self.repo / "npm/package.json").write_text(json.dumps({"name": "termplete", "version": "0.1.4"}))

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, env=self.env,
                                       text=True, stderr=subprocess.PIPE).strip()

    def publish(self, *args, **overrides):
        return subprocess.run([str(self.repo / "scripts/publish.sh"), *args],
                              cwd=self.repo, env=dict(self.env, **overrides),
                              text=True, capture_output=True, timeout=20)

    def assert_version(self, version):
        for name in ("Cargo.toml", "Cargo.lock"):
            self.assertIn(f'version = "{version}"', (self.repo / name).read_text())
        self.assertEqual(json.loads((self.repo / "npm/package.json").read_text())["version"], version)

    def test_bump_version_requires_reset_for_downgrade(self):
        command = [str(self.repo / "scripts/bump-version.sh")]
        rejected = subprocess.run(command + ["0.1.0"], cwd=self.repo, env=self.env,
                                   text=True, capture_output=True, timeout=20)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("must be greater than current version", rejected.stderr)
        self.assert_version("0.1.4")

        reset = subprocess.run(command + ["0.1.0", "--reset"], cwd=self.repo, env=self.env,
                               text=True, capture_output=True, timeout=20)
        self.assertEqual(reset.returncode, 0, reset.stdout + reset.stderr)
        self.assert_version("0.1.0")

    def test_default_patch_publishes_and_pulls_formula(self):
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_version("0.1.5")
        self.assertIn("chore(release): prepare v0.1.5", self.git("log", "-2", "--format=%s"))
        self.assertIn("v0.1.5", (self.repo / "Formula/termplete.rb").read_text())
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertEqual(self.git("rev-parse", "HEAD"), self.git("rev-parse", "origin/main"))
        self.assertIn("refs/tags/v0.1.5", self.git("ls-remote", "--tags", "origin"))

    def test_minor_and_major_reset_lower_components(self):
        for request, expected in (("minor", "0.2.0"), ("major", "1.0.0")):
            with self.subTest(request=request):
                result = self.publish(request)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_version(expected)

    def test_explicit_current_version_publishes_without_bump_commit(self):
        result = self.publish("0.1.4")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_version("0.1.4")
        self.assertEqual(self.git("rev-parse", "v0.1.4^{}"), self.initial_commit)

    def test_existing_remote_tag_refuses_before_version_changes(self):
        self.git("tag", "v0.1.5")
        self.git("push", "origin", "v0.1.5")
        self.git("tag", "-d", "v0.1.5")
        result = self.publish("patch")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)
        self.assert_version("0.1.4")
        self.assertEqual(self.git("rev-parse", "HEAD"), self.initial_commit)

    def test_dirty_checkout_refuses_before_publishing(self):
        (self.repo / "uncommitted.txt").write_text("feature in progress")
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Commit your feature", result.stderr)
        self.assert_version("0.1.4")
        self.assertEqual(self.git("ls-remote", "--tags", "origin"), "")

    def test_failed_workflow_does_not_claim_publication_or_pull_formula(self):
        result = self.publish("patch", RELEASE_WATCH_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Release failed", result.stderr)
        self.assertNotIn("Published v", result.stdout)
        self.assertFalse((self.repo / "Formula/termplete.rb").exists())
        self.assertIn("refs/tags/v0.1.5", self.git("ls-remote", "--tags", "origin"))
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_invalid_or_older_version_cannot_publish(self):
        for request in ("0.1.3", "01.2.3", "0.1.5;touch injected"):
            with self.subTest(request=request):
                result = self.publish(request)
                self.assertNotEqual(result.returncode, 0)
                self.assert_version("0.1.4")
                self.assertEqual(self.git("ls-remote", "--tags", "origin"), "")

    def test_missing_auth_refuses_before_version_changes(self):
        result = self.publish(RELEASE_AUTH_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("gh auth login", result.stderr)
        self.assert_version("0.1.4")


if __name__ == "__main__":
    unittest.main(verbosity=2)
