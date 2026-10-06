"""Tests for the aplexer Python wrapper entry points."""

import os
import subprocess
import sys
import tempfile
import unittest
from io import StringIO
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import aplexer_cli
from aplexer_cli import _main


class TestVersion(unittest.TestCase):
    def test_version_exists(self):
        self.assertTrue(hasattr(aplexer_cli, "__version__"))

    def test_version_matches_pyproject(self):
        pyproject_path = os.path.join(
            os.path.dirname(os.path.dirname(__file__)), "pyproject.toml"
        )
        version = None
        with open(pyproject_path, "r") as f:
            for line in f:
                if line.startswith("version"):
                    version = line.split('"')[1]
                    break
        self.assertIsNotNone(version, "Could not find version in pyproject.toml")
        self.assertEqual(aplexer_cli.__version__, version)


class TestEntryPointsExist(unittest.TestCase):
    def test_main_a_is_callable(self):
        self.assertTrue(callable(_main.main_a))

    def test_main_aplexer_is_callable(self):
        self.assertTrue(callable(_main.main_aplexer))


class TestPlatformDetection(unittest.TestCase):
    """Platform detection resolves only architectures in the wheel matrix."""

    def _assert_resolves(self, plat, machine, suffix):
        with mock.patch("sys.platform", plat), mock.patch(
            "platform.machine", return_value=machine
        ):
            path = _main._get_binary_path()
            self.assertIsNotNone(path)
            self.assertTrue(path.endswith(os.path.join("bin", "aplexer" + suffix)))

    def test_linux_x86_64(self):
        self._assert_resolves("linux", "x86_64", "")

    def test_linux_aarch64(self):
        self._assert_resolves("linux", "aarch64", "")

    def test_linux_arm64_alias(self):
        self._assert_resolves("linux", "arm64", "")

    def test_windows_amd64_resolves_exe_when_released(self):
        if ("win32", "AMD64") not in _main._PLATFORM_MAP:
            self.skipTest("Windows wheel not part of this release matrix")
        self._assert_resolves("win32", "AMD64", ".exe")

    def test_unreleased_platforms_are_unsupported(self):
        for plat, machine in (
            ("darwin", "x86_64"),
            ("darwin", "arm64"),
            ("win32", "AMD64"),
            ("win32", "ARM64"),
            ("freebsd", "armv7l"),
        ):
            if (plat, machine) in _main._PLATFORM_MAP:
                continue  # released since: covered by its own resolve test
            with self.subTest(platform=plat, machine=machine), mock.patch(
                "sys.platform", plat
            ), mock.patch("platform.machine", return_value=machine):
                self.assertIsNone(_main._get_binary_path())


class TestMissingOrUnsupportedExitsWithError(unittest.TestCase):
    def test_unsupported_platform_exits_with_error(self):
        stderr = StringIO()
        with mock.patch("sys.platform", "freebsd"), mock.patch(
            "platform.machine", return_value="armv7l"
        ), mock.patch("sys.stderr", stderr), self.assertRaises(SystemExit) as cm:
            _main.main_a()
        self.assertEqual(cm.exception.code, 1)
        self.assertIn("Linux x86_64", stderr.getvalue())
        self.assertIn("Linux aarch64 (arm64)", stderr.getvalue())
        self.assertNotIn("macOS", stderr.getvalue())
        if ("win32", "AMD64") not in _main._PLATFORM_MAP:
            self.assertNotIn("Windows", stderr.getvalue())

    def test_missing_binary_exits_with_error(self):
        """Supported platform, but no binary bundled in the source tree."""
        with mock.patch("sys.platform", "linux"), mock.patch(
            "platform.machine", return_value="x86_64"
        ), mock.patch("sys.stderr"), self.assertRaises(SystemExit) as cm:
            _main.main_aplexer()
        self.assertEqual(cm.exception.code, 1)


class TestPythonModuleInvocation(unittest.TestCase):
    def test_python_m_aplexer_cli_invokes_entry_point(self):
        """``python -m aplexer_cli`` should invoke the aplexer entry point.

        No binary is bundled in the source tree, so it should exit 1 with
        an error message naming the missing binary.
        """
        result = subprocess.run(
            [sys.executable, "-m", "aplexer_cli"],
            capture_output=True,
            text=True,
            cwd=os.path.join(os.path.dirname(__file__), ".."),
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("aplexer", result.stderr.lower())


class TestArgumentForwarding(unittest.TestCase):
    @mock.patch("os.execvp")
    def test_unix_args_forwarded(self, mock_execvp):
        with tempfile.NamedTemporaryFile(suffix="aplexer", delete=False) as f:
            fake_binary = f.name
        try:
            with mock.patch.object(
                _main, "_get_binary_path", return_value=fake_binary
            ), mock.patch("sys.platform", "linux"), mock.patch(
                "sys.argv", ["aplexer", "run", "--"]
            ):
                _main.main_aplexer()
                mock_execvp.assert_called_once_with(
                    fake_binary,
                    [fake_binary, "run", "--"],
                )
        finally:
            os.unlink(fake_binary)

class TestWindowsLaunchCopy(unittest.TestCase):
    """The Windows launcher runs a private copy so the wheel's exe stays unlocked."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.local = os.path.join(self.tmp.name, "local")
        os.makedirs(self.local)
        self.binary = os.path.join(self.tmp.name, "wheel", "aplexer.exe")
        os.makedirs(os.path.dirname(self.binary))
        with open(self.binary, "wb") as f:
            f.write(b"v1" * 100)
        patcher = mock.patch.dict(os.environ, {"LOCALAPPDATA": self.local})
        patcher.start()
        self.addCleanup(patcher.stop)
        os.environ.pop("APLEXER_RUN_IN_PLACE", None)

    def test_copy_is_created_once_and_reused(self):
        first = _main._launch_copy(self.binary)
        self.assertNotEqual(os.path.abspath(first), os.path.abspath(self.binary))
        self.assertTrue(first.startswith(os.path.join(self.local, "aplexer", "bin")))
        with open(first, "rb") as f:
            self.assertEqual(f.read(), b"v1" * 100)
        self.assertEqual(_main._launch_copy(self.binary), first)

    def test_new_build_gets_its_own_copy_and_run_in_place_opts_out(self):
        first = _main._launch_copy(self.binary)
        with open(self.binary, "wb") as f:
            f.write(b"v2" * 50)
        second = _main._launch_copy(self.binary)
        self.assertNotEqual(first, second)
        with mock.patch.dict(os.environ, {"APLEXER_RUN_IN_PLACE": "1"}):
            self.assertEqual(_main._launch_copy(self.binary), self.binary)

    def test_stale_unused_copies_are_swept_but_fresh_ones_stay(self):
        stale = _main._launch_copy(self.binary)
        stale_dir = os.path.dirname(stale)
        old = os.path.getmtime(stale_dir) - 7200
        os.utime(stale_dir, (old, old))
        with open(self.binary, "wb") as f:
            f.write(b"v2" * 50)
        fresh = _main._launch_copy(self.binary)
        self.assertFalse(os.path.exists(stale_dir))
        self.assertTrue(os.path.isfile(fresh))

    def test_missing_localappdata_falls_back_to_bundled_binary(self):
        with mock.patch.dict(os.environ):
            os.environ.pop("LOCALAPPDATA")
            self.assertEqual(_main._launch_copy(self.binary), self.binary)

    @unittest.skipUnless(sys.platform == "win32", "needs Windows file locking")
    def test_running_copy_does_not_lock_the_wheel_binary(self):
        system_exe = os.path.join(os.environ["SystemRoot"], "System32", "ping.exe")
        shutil_copy = __import__("shutil").copyfile
        shutil_copy(system_exe, self.binary)
        target = _main._launch_copy(self.binary)
        proc = subprocess.Popen(
            [target, "-n", "30", "127.0.0.1"],
            stdout=subprocess.DEVNULL,
        )
        try:
            # The "upgrade": overwrite and delete the wheel's exe while a
            # session still runs from the copy.
            with open(self.binary, "wb") as f:
                f.write(b"upgraded")
            os.unlink(self.binary)
            self.assertIsNone(proc.poll())
        finally:
            proc.kill()
            proc.wait()

if __name__ == "__main__":
    unittest.main()
