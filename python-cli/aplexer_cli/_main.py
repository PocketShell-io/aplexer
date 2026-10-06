"""Entry points that locate and execute the bundled aplexer binary.

Same pattern as the ``a``/``aplexer`` lookup in ``aplexer-client``'s
``resolve_cli()``: the binary this wheel ships lives next to this file,
under ``bin/``, not on ``$PATH``. One binary serves both names -- ``a``
is just an alias that executes the same file as ``aplexer``.
"""

import os
import platform
import sys


# Release wheels exist for Linux x86_64, Linux aarch64 and Windows x86_64.
# Linux normally
# reports the latter as "aarch64", but accept "arm64" as an equivalent machine
# spelling when locating the same bundled binary.
_PLATFORM_MAP = {
    ("linux", "x86_64"): "",
    ("linux", "aarch64"): "",
    ("linux", "arm64"): "",
    ("win32", "AMD64"): ".exe",
}

_SUPPORTED_PLATFORMS = [
    "Linux x86_64",
    "Linux aarch64 (arm64)",
    "Windows x86_64",
]

# The one binary this wheel bundles; both console scripts execute it.
BINARY_NAME = "aplexer"


def _binary_suffix():
    """Return the bundled-binary filename suffix for this platform, or None."""
    return _PLATFORM_MAP.get((sys.platform, platform.machine()))


def _get_binary_path():
    """Return the path to the bundled binary for the current platform."""
    suffix = _binary_suffix()
    if suffix is None:
        return None
    package_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.join(package_dir, "bin", BINARY_NAME + suffix)

def _launch_copy(binary_path):
    """Windows: return a per-build copy of the bundled binary to execute.

    A running session's worker keeps its executable open, and Windows refuses
    to overwrite or delete an open .exe -- so ``pip install -U`` over a wheel
    whose binary is in use fails with "Access denied". Executing a private
    copy under ``%LOCALAPPDATA%\\aplexer\\bin\\<build-hash>\\`` leaves the
    wheel's own file unlocked. Copies of other builds are removed once nothing
    runs them (a still-running one cannot be deleted and simply stays).
    Any failure falls back to the bundled path, and
    ``APLEXER_RUN_IN_PLACE=1`` opts out.
    """
    if os.environ.get("APLEXER_RUN_IN_PLACE"):
        return binary_path
    try:
        import hashlib
        import shutil
        import time

        base = os.environ.get("LOCALAPPDATA")
        if not base:
            return binary_path
        st = os.stat(binary_path)
        ident = "{}|{}|{}".format(os.path.abspath(binary_path), st.st_size, st.st_mtime_ns)
        key = hashlib.sha256(ident.encode("utf-8")).hexdigest()[:16]
        root = os.path.join(base, "aplexer", "bin")
        target_dir = os.path.join(root, key)
        target = os.path.join(target_dir, os.path.basename(binary_path))
        if not (os.path.isfile(target) and os.path.getsize(target) == st.st_size):
            os.makedirs(target_dir, exist_ok=True)
            tmp = "{}.{}.tmp".format(target, os.getpid())
            shutil.copyfile(binary_path, tmp)
            try:
                os.replace(tmp, target)
            except OSError:
                # A concurrent launcher won the race and is already running it.
                if os.path.exists(tmp):
                    os.unlink(tmp)
                if not os.path.isfile(target):
                    return binary_path
        os.utime(target_dir)
        # Sweep other builds' copies that nothing runs and nobody touched lately.
        cutoff = time.time() - 3600
        for name in os.listdir(root):
            old = os.path.join(root, name)
            if name == key or not os.path.isdir(old):
                continue
            try:
                if os.path.getmtime(old) < cutoff:
                    shutil.rmtree(old)
            except OSError:
                pass  # in use (or racing): leave it
        return target
    except Exception:
        return binary_path


def _run():
    """Locate the bundled binary and execute it, forwarding all arguments."""
    binary_path = _get_binary_path()

    if binary_path is None or not os.path.isfile(binary_path):
        plat = sys.platform
        machine = platform.machine()
        print(
            "Error: no bundled '{name}' binary for this platform "
            "({platform} {machine}).\n"
            "\n"
            "Supported platforms:\n"
            "{platforms}\n"
            "\n"
            "You can build from source with: cargo install --path .".format(
                name=BINARY_NAME,
                platform=plat,
                machine=machine,
                platforms="\n".join("  - " + p for p in _SUPPORTED_PLATFORMS),
            ),
            file=sys.stderr,
        )
        sys.exit(1)

    if sys.platform == "win32":
        binary_path = _launch_copy(binary_path)
    args = [binary_path] + sys.argv[1:]

    if sys.platform == "win32":
        # os.exec* on Windows spawns a new process and returns immediately, so
        # wait for the child and forward its exit status instead.
        import subprocess

        sys.exit(subprocess.call(args))
    os.execvp(binary_path, args)


def main_a():
    """Console-script entry point for ``a`` -- an alias for ``aplexer``."""
    _run()


def main_aplexer():
    """Console-script entry point for ``aplexer`` -- the aplexer binary."""
    _run()
