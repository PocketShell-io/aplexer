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
