"""Offline checks for the release installer and Homebrew formula generator."""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from io import BytesIO
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts" / "install.sh"
FORMULA_GENERATOR = ROOT / "packaging" / "homebrew" / "generate_formula.py"
VERSION = re.search(
    r'^version = "([^"]+)"$', (ROOT / "Cargo.toml").read_text(), re.MULTILINE
).group(1)
TARGET = "x86_64-unknown-linux-gnu"
ARCHIVE = f"termplete-{VERSION}-{TARGET}.tar.gz"
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)


def make_archive(path: Path, extra_path: str | None = None) -> bytes:
    payloads = {
        "termplete": b"termplete test executable\n",
        "tp": b"tp test executable\n",
    }
    with tarfile.open(path, "w:gz") as archive:
        for name, payload in payloads.items():
            member = tarfile.TarInfo(name)
            member.mode = 0o755
            member.size = len(payload)
            archive.addfile(member, BytesIO(payload))
        if extra_path is not None:
            payload = b"outside\n"
            member = tarfile.TarInfo(extra_path)
            member.mode = 0o644
            member.size = len(payload)
            archive.addfile(member, BytesIO(payload))
    return path.read_bytes()


def checksum(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class DistributionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="termplete-distribution-")
        self.root = Path(self.temporary.name)
        self.fixture = self.root / "fixture"
        self.fixture.mkdir()
        self.install_dir = self.root / "bin"
        self.stub_bin = self.root / "stub-bin"
        self.stub_bin.mkdir()
        self.tarball = self.fixture / ARCHIVE
        self.archive_bytes = make_archive(self.tarball)
        (self.fixture / "SHA256SUMS").write_text(
            f"{checksum(self.archive_bytes)}  {ARCHIVE}\n", encoding="utf-8"
        )
        self.write_stub("curl", self.curl_stub())
        self.write_stub("uname", self.uname_stub())
        self.env = os.environ.copy()
        self.env.update(
            {
                "HOME": str(self.root / "home"),
                "PATH": f"{self.stub_bin}{os.pathsep}{os.environ['PATH']}",
                "TERMPLETE_INSTALL_DIR": str(self.install_dir),
                "TERMPLETE_TEST_FIXTURE": str(self.fixture),
                "TERMPLETE_TEST_LATEST_URL": f"https://github.com/aft1n/termplete/releases/tag/v{VERSION}",
                "TERMPLETE_TEST_OS": "Linux",
                "TERMPLETE_TEST_ARCH": "x86_64",
            }
        )
        self.env.pop("TERMPLETE_VERSION", None)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_stub(self, name: str, contents: str) -> Path:
        path = self.stub_bin / name
        path.write_text(contents, encoding="utf-8")
        path.chmod(0o755)
        return path

    @staticmethod
    def curl_stub() -> str:
        # Respect curl's end-of-options marker: ignoring it hid a broken live
        # download where -o followed -- and was treated as another URL.
        return """#!/bin/sh
set -eu
url=
output=
write_output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -w) write_output=$2; shift 2 ;;
        -o) output=$2; shift 2 ;;
        https://*) url=$1; shift ;;
        --)
            shift
            [ "$#" -eq 1 ] || exit 24
            url=$1
            shift
            ;;
        *) shift ;;
    esac
done
case "$url" in
    https://github.com/aft1n/termplete/releases/latest)
        [ "$write_output" = '%{url_effective}' ] || exit 21
        printf '%s' "$TERMPLETE_TEST_LATEST_URL"
        ;;
    https://github.com/aft1n/termplete/releases/download/*)
        [ -n "$output" ] || exit 22
        name=${url##*/}
        cp "$TERMPLETE_TEST_FIXTURE/$name" "$output"
        ;;
    *) exit 23 ;;
esac
"""

    @staticmethod
    def uname_stub() -> str:
        return """#!/bin/sh
case "$1" in
    -s) printf '%s\\n' "$TERMPLETE_TEST_OS" ;;
    -m) printf '%s\\n' "$TERMPLETE_TEST_ARCH" ;;
    *) exit 1 ;;
esac
"""

    def run_installer(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["sh", str(INSTALLER), *args],
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_shell_syntax_and_latest_release_install(self) -> None:
        syntax = subprocess.run(["sh", "-n", str(INSTALLER)], check=False)
        self.assertEqual(syntax.returncode, 0)

        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.install_dir / "termplete").read_bytes(), b"termplete test executable\n"
        )
        self.assertEqual((self.install_dir / "tp").read_bytes(), b"tp test executable\n")
        self.assertIn(f"Installed TermPlete {VERSION}", result.stdout)
        self.assertIn("Run tp setup", result.stdout)

    def test_each_supported_platform_installs_its_release_archive(self) -> None:
        platforms = (
            ("Darwin", "arm64", TARGETS[0]),
            ("Darwin", "x86_64", TARGETS[1]),
            ("Linux", "aarch64", TARGETS[2]),
            ("Linux", "x86_64", TARGETS[3]),
        )
        for system, architecture, target in platforms:
            with self.subTest(system=system, architecture=architecture):
                name = f"termplete-{VERSION}-{target}.tar.gz"
                data = make_archive(self.fixture / name)
                (self.fixture / "SHA256SUMS").write_text(f"{checksum(data)}  {name}\n")
                self.env.update(TERMPLETE_TEST_OS=system, TERMPLETE_TEST_ARCH=architecture)
                result = self.run_installer("--version", VERSION)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f"({target})", result.stdout)
                for binary in ("termplete", "tp"):
                    self.assertTrue(os.access(self.install_dir / binary, os.X_OK))

    def test_unsupported_platform_and_invalid_latest_redirect_fail_without_installing(self) -> None:
        self.env["TERMPLETE_TEST_OS"] = "Windows"
        result = self.run_installer("--version", VERSION)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported platform", result.stderr)
        self.assertFalse(self.install_dir.exists())

        self.env["TERMPLETE_TEST_OS"] = "Linux"
        for url in ("https://example.com/releases/tag/v0.1.0",
                    "https://github.com/aft1n/termplete/releases/tag/v../../bad"):
            with self.subTest(url=url):
                self.env["TERMPLETE_TEST_LATEST_URL"] = url
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.install_dir.exists())

    def test_existing_binary_symlink_is_preserved(self) -> None:
        self.install_dir.mkdir()
        original = self.root / "original"
        original.write_bytes(b"keep me\n")
        (self.install_dir / "termplete").symlink_to(original)
        result = self.run_installer("--version", VERSION)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to replace symlink", result.stderr)
        self.assertTrue((self.install_dir / "termplete").is_symlink())
        self.assertEqual(original.read_bytes(), b"keep me\n")
        self.assertFalse((self.install_dir / "tp").exists())

    def test_explicit_version_and_invalid_version(self) -> None:
        self.env["TERMPLETE_VERSION"] = f"v{VERSION}"
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)

        rejected = self.run_installer("--version", "../../latest")
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("invalid version", rejected.stderr)

    def test_bad_checksum_does_not_replace_existing_binaries(self) -> None:
        self.install_dir.mkdir()
        (self.install_dir / "termplete").write_bytes(b"old termplete\n")
        (self.install_dir / "tp").write_bytes(b"old tp\n")
        (self.fixture / "SHA256SUMS").write_text(
            f"{'0' * 64}  {ARCHIVE}\n", encoding="utf-8"
        )

        result = self.run_installer("--version", VERSION)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum verification failed", result.stderr)
        self.assertEqual((self.install_dir / "termplete").read_bytes(), b"old termplete\n")
        self.assertEqual((self.install_dir / "tp").read_bytes(), b"old tp\n")

    def test_archive_path_traversal_is_rejected(self) -> None:
        self.archive_bytes = make_archive(self.tarball, "../escaped")
        (self.fixture / "SHA256SUMS").write_text(
            f"{checksum(self.archive_bytes)}  {ARCHIVE}\n", encoding="utf-8"
        )
        self.install_dir.mkdir()
        (self.install_dir / "termplete").write_bytes(b"old termplete\n")
        (self.install_dir / "tp").write_bytes(b"old tp\n")

        result = self.run_installer("--version", VERSION)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("archive must contain exactly", result.stderr)
        self.assertFalse((self.root / "escaped").exists())
        self.assertEqual((self.install_dir / "termplete").read_bytes(), b"old termplete\n")
        self.assertEqual((self.install_dir / "tp").read_bytes(), b"old tp\n")

    def test_failed_second_move_restores_both_binaries(self) -> None:
        self.install_dir.mkdir()
        (self.install_dir / "termplete").write_bytes(b"old termplete\n")
        (self.install_dir / "tp").write_bytes(b"old tp\n")
        real_mv = shutil.which("mv")
        self.assertIsNotNone(real_mv)
        self.env["TERMPLETE_TEST_REAL_MV"] = str(real_mv)
        self.env["TERMPLETE_TEST_MV_COUNTER"] = str(self.root / "mv-count")
        self.write_stub(
            "mv",
            """#!/bin/sh
set -eu
count=0
if [ -f "$TERMPLETE_TEST_MV_COUNTER" ]; then read count < "$TERMPLETE_TEST_MV_COUNTER"; fi
count=$((count + 1))
printf '%s\\n' "$count" > "$TERMPLETE_TEST_MV_COUNTER"
if [ "$count" -eq 2 ]; then exit 1; fi
exec "$TERMPLETE_TEST_REAL_MV" "$@"
""",
        )

        result = self.run_installer("--version", VERSION)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.install_dir / "termplete").read_bytes(), b"old termplete\n")
        self.assertEqual((self.install_dir / "tp").read_bytes(), b"old tp\n")

    def test_existing_install_lock_is_preserved_and_no_binary_is_replaced(self) -> None:
        self.install_dir.mkdir()
        lock = self.install_dir / ".termplete-install.lock"
        lock.mkdir()
        (self.install_dir / "termplete").write_bytes(b"old termplete\n")
        result = self.run_installer("--version", VERSION)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("another installation or a leftover lock", result.stderr)
        self.assertTrue(lock.is_dir())
        self.assertEqual((self.install_dir / "termplete").read_bytes(), b"old termplete\n")
        self.assertFalse((self.install_dir / "tp").exists())

    def test_homebrew_formula_uses_each_release_checksum(self) -> None:
        checksums = self.root / "formula-SHA256SUMS"
        expected: dict[str, str] = {}
        lines = []
        for index, target in enumerate(TARGETS, 1):
            filename = f"termplete-{VERSION}-{target}.tar.gz"
            digest = hashlib.sha256(f"asset-{index}".encode()).hexdigest()
            expected[target] = digest
            lines.append(f"{digest}  {filename}")
        checksums.write_text("\n".join(lines) + "\n", encoding="utf-8")
        output = self.root / "termplete.rb"
        result = subprocess.run(
            [
                "python3",
                str(FORMULA_GENERATOR),
                "--version",
                VERSION,
                "--checksums",
                str(checksums),
                "--output",
                str(output),
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        formula = output.read_text(encoding="utf-8")
        ruby = shutil.which("ruby")
        if ruby is not None:
            syntax = subprocess.run([ruby, "-c", str(output)], capture_output=True, text=True)
            self.assertEqual(syntax.returncode, 0, syntax.stderr)
        self.assertNotIn("@@", formula)
        for digest in expected.values():
            self.assertIn(digest, formula)
        self.assertIn(f'version "{VERSION}"', formula)

        lines.pop()
        checksums.write_text("\n".join(lines) + "\n", encoding="utf-8")
        rejected = subprocess.run(
            [
                "python3",
                str(FORMULA_GENERATOR),
                "--version",
                VERSION,
                "--checksums",
                str(checksums),
                "--output",
                str(output),
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertNotEqual(rejected.returncode, 0)


if __name__ == "__main__":
    unittest.main()
