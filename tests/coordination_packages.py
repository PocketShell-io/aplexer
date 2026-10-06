"""Exercise generated hook commands and protect existing destination contents."""

import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
GENERATOR = ROOT / "scripts" / "package-coordination.py"


class CoordinationPackages(unittest.TestCase):
    def generate(self, dest, binary, *extra):
        return subprocess.run(
            [sys.executable, "-B", str(GENERATOR), "--engine", "all",
             "--dest", str(dest), "--aplexer-bin", str(binary), "--json", *extra],
            capture_output=True, text=True,
        )

    @unittest.skipIf(sys.platform == "win32", "POSIX shell quoting and #!/bin/sh stubs")
    def test_special_binary_paths_are_literal_in_shell_and_json(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / "agent bin ' \" $HOME `touch PWNED`"
            directory.mkdir()
            binary = directory / "a"
            binary.write_text("#!/bin/sh\nprintf '{}\\n'\n")
            binary.chmod(0o755)
            result = self.generate(root / "bundles", binary)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(json.loads(result.stdout)["bundles"]), 6)

            commands = []

            def collect(value):
                if isinstance(value, dict):
                    for key, item in value.items():
                        if key == "command" and isinstance(item, str):
                            commands.append(item)
                        else:
                            collect(item)
                elif isinstance(value, list):
                    for item in value:
                        collect(item)

            for path in (root / "bundles").rglob("*.json"):
                collect(json.loads(path.read_text()))
            self.assertGreaterEqual(len(commands), 5)
            for command in commands:
                self.assertEqual(shlex.split(command)[0], str(binary))
                executed = subprocess.run(
                    ["/bin/sh", "-c", command], input="{}", text=True,
                    capture_output=True, cwd=root, timeout=5,
                )
                self.assertEqual(executed.returncode, 0, executed.stderr)
                self.assertEqual(executed.stdout.strip(), "{}")
            self.assertFalse((root / "PWNED").exists())

    def test_force_preserves_an_unrelated_destination_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "a"
            binary.write_text("#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
            unrelated = root / "bundles" / "codex"
            unrelated.mkdir(parents=True)
            sentinel = unrelated / "user-data.txt"
            sentinel.write_text("Preserve this file.")
            result = self.generate(root / "bundles", binary, "--force")
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(sentinel.read_text(), "Preserve this file.")

    @unittest.skipIf(sys.platform == "win32", "POSIX shell stub binary")
    @unittest.skipUnless(shutil.which("node"), "Node is unavailable")
    def test_opencode_appends_notice_without_changing_tool_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "a"
            binary.write_text("#!/bin/sh\nprintf 'APLEXER_TEST_NOTICE\\n'\ncat\n")
            binary.chmod(0o755)
            generated = self.generate(root / "bundles", binary)
            self.assertEqual(generated.returncode, 0, generated.stderr)
            # Use an explicit ESM suffix for older Node releases used in CI.
            module = root / "plugin.mjs"
            module.write_text((root / "bundles" / "opencode" / "aplexer-awareness.js").read_text())
            script = """
import {pathToFileURL} from 'node:url';
const plugin = await import(pathToFileURL(process.argv[1]).href);
const hooks = await plugin.AplexerCoordination();
const original = 'actual tool result with trailing whitespace  \\n';
const output = {output: original, title: 'Original title', metadata: {kept: true}};
const input = {tool: 'edit', sessionID: 'native-session', callID: 'one'};
await hooks['tool.execute.before'](input, {args: {filePath: '/other/tree/src/new.rs', command: 'PRIVATE_COMMAND'}});
await hooks['tool.execute.after'](input, output);
if (!output.output.startsWith(original)) throw new Error('original tool result changed');
if (!output.output.includes('APLEXER_TEST_NOTICE')) throw new Error('notice never reached model output');
if (output.title !== 'Original title' || !output.metadata.kept) throw new Error('metadata changed');
if (!output.output.includes('/other/tree/src/new.rs')) throw new Error('destination path lost');
if (output.output.includes('PRIVATE_COMMAND')) throw new Error('shell command leaked');
const second = {output: 'second result'};
await hooks['tool.execute.after'](input, second);
if (second.output.includes('/other/tree/src/new.rs')) throw new Error('stale tool paths reused');
"""
            executed = subprocess.run(
                ["node", "--input-type=module", "-e", script, str(module)],
                capture_output=True, text=True, timeout=10,
            )
            self.assertEqual(executed.returncode, 0, executed.stderr)


if __name__ == "__main__":
    unittest.main()
