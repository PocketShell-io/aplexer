#!/usr/bin/env python3
"""Generate native aplexer coordination packages for one or more agent engines.

Assembles plugins/coordination/engines/<engine>/ adapter files plus the shared
protocol skill (.agents/skills/a2a-communication/SKILL.md) into per-engine
bundles under a destination you choose. The absolute aplexer binary is baked
into hook commands via --aplexer-bin (default: resolved from PATH).

Substitution is structurally safe: JSON files are parsed, every string that
carries the placeholder gets shlex.quote(binary) spliced in, and the document
is re-serialized — the binary path can never break out of a JSON string or a
shell word. The OpenCode plugin gets the binary as a JSON-serialized JS
string literal. Markdown gets the raw path for prose display.

By design this script NEVER installs anything and NEVER edits user config:
it only writes inside --dest. Each bundle is built in a staging directory and
renamed into place only after validation, and carries a
.aplexer-generated-bundle.json marker. --force replaces only directories that
carry that marker, never arbitrary pre-existing content.

Usage:
    scripts/package-coordination.py --engine all --dest /tmp/bundles
    scripts/package-coordination.py --engine claude --dest /tmp/bundles \
        --aplexer-bin /home/me/.local/bin/a
    scripts/package-coordination.py --engine all --dest /tmp/bundles --json
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import sys
from pathlib import Path

PLACEHOLDER = "__APLEXER_BIN__"
QUOTED_PLACEHOLDER = f'"{PLACEHOLDER}"'  # the JS literal form
MARKER = ".aplexer-generated-bundle.json"
ENGINES = ("codex", "claude", "grok", "antigravity", "gemini", "opencode")

# source path (relative to plugins/coordination/engines/<engine>) ->
# destination path (relative to the bundle root). The shared skill and
# INSTALL.md are added to every bundle separately.
ENGINE_FILES: dict[str, dict[str, str]] = {
    "codex": {
        "plugin.json": "plugin.json",
        "codex-plugin.json": ".codex-plugin/plugin.json",
        "hooks/hooks.json": "hooks/hooks.json",
    },
    "claude": {
        "plugin.json": ".claude-plugin/plugin.json",
        "hooks/hooks.json": "hooks/hooks.json",
    },
    "grok": {
        "plugin.json": ".claude-plugin/plugin.json",
        "hooks/hooks.json": "hooks/hooks.json",
    },
    "antigravity": {
        "plugin.json": "plugin.json",
        "hooks.json": "hooks.json",
    },
    "gemini": {
        "gemini-extension.json": "gemini-extension.json",
        "hooks/hooks.json": "hooks/hooks.json",
        "GEMINI.md": "GEMINI.md",
    },
    "opencode": {
        "aplexer-awareness.js": "aplexer-awareness.js",
    },
}

# Required top-level keys per emitted JSON manifest, checked after generation.
MANIFEST_KEYS: dict[str, tuple[str, ...]] = {
    "codex/plugin.json": ("name", "version", "extensions"),
    "codex/.codex-plugin/plugin.json": ("name",),
    "claude/.claude-plugin/plugin.json": ("name",),
    "grok/.claude-plugin/plugin.json": ("name",),
    "antigravity/plugin.json": ("name",),
    "gemini/gemini-extension.json": ("name", "version"),
}


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def resolve_aplexer_bin(explicit: str | None) -> Path:
    if explicit:
        candidate = Path(explicit).expanduser()
        if not candidate.is_absolute():
            candidate = Path.cwd() / candidate
        candidate = candidate.resolve()
        if not candidate.is_file():
            raise SystemExit(f"error: --aplexer-bin is not a file: {candidate}")
        return candidate
    found = shutil.which("a") or shutil.which("aplexer")
    if not found:
        raise SystemExit(
            "error: no aplexer binary on PATH; pass --aplexer-bin /absolute/path/to/a"
        )
    return Path(found).resolve()


def validate_skill(path: Path) -> None:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as e:
        raise SystemExit(f"error: cannot read shared skill {path}: {e}")
    if not text.startswith("---\n"):
        raise SystemExit(f"error: {path} has no YAML frontmatter")
    frontmatter = text.split("---\n", 2)[1]
    for key in ("name:", "description:"):
        if key not in frontmatter:
            raise SystemExit(f"error: {path} frontmatter lacks {key}")
    for required in ("a whoami", "a message send", "a work join", "a context"):
        if required not in text:
            raise SystemExit(f"error: {path} is missing required content: {required!r}")


def substitute_json(text: str, a_bin: str, label: str) -> str:
    """Parse JSON, splice shlex.quote(binary) into placeholder-carrying
    strings, re-serialize. Shell metacharacters in the binary path can never
    escape the JSON string or form extra shell words."""

    def walk(value):
        if isinstance(value, str):
            if PLACEHOLDER in value:
                substituted = value.replace(PLACEHOLDER, shlex.quote(a_bin))
                words = shlex.split(substituted)
                if not words or words[0] != a_bin:
                    raise SystemExit(
                        f"error: {label}: substituted command does not parse "
                        f"with the binary as its first word: {substituted!r}"
                    )
                return substituted
            return value
        if isinstance(value, list):
            return [walk(v) for v in value]
        if isinstance(value, dict):
            return {k: walk(v) for k, v in value.items()}
        return value

    try:
        data = json.loads(text)
    except json.JSONDecodeError as e:
        raise SystemExit(f"error: {label} is not valid JSON: {e}")
    return json.dumps(walk(data), indent=2) + "\n"


def substitute_js(text: str, a_bin: str, label: str) -> str:
    """Replace the quoted JS literal placeholder with a JSON-serialized
    string, so quotes/$/backticks in the path stay inside the literal."""
    substituted = text.replace(QUOTED_PLACEHOLDER, json.dumps(a_bin))
    if PLACEHOLDER in substituted:
        raise SystemExit(f"error: {label} has an un-substitutable placeholder")
    return substituted


def substitute_text(text: str, a_bin: str) -> str:
    """Markdown: the raw path is for prose display, never executed."""
    return text.replace(PLACEHOLDER, a_bin)


def render_file(src: Path, dest_rel: str, a_bin: str) -> str:
    text = src.read_text(encoding="utf-8")
    if dest_rel.endswith(".json"):
        return substitute_json(text, a_bin, str(src))
    if dest_rel.endswith(".js"):
        return substitute_js(text, a_bin, str(src))
    return substitute_text(text, a_bin)


def build_engine(engine: str, dest_root: Path, sources: Path, skill_src: Path,
                 a_bin: str, force: bool) -> dict:
    bundle = dest_root / engine
    if bundle.exists():
        if not force:
            raise SystemExit(
                f"error: {bundle} already exists (pass --force to replace it)"
            )
        if not (bundle / MARKER).is_file():
            raise SystemExit(
                f"error: refusing to replace {bundle}: it carries no {MARKER} "
                "marker, so it was not generated by this script"
            )
        shutil.rmtree(bundle)

    staged = dest_root / f".staging-{engine}-{os.getpid()}"
    if staged.exists():
        shutil.rmtree(staged)
    written: list[str] = []
    try:
        for src_rel, dest_rel in ENGINE_FILES[engine].items():
            rendered = render_file(sources / engine / src_rel, dest_rel, a_bin)
            target = staged / dest_rel
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(rendered, encoding="utf-8")
            written.append(dest_rel)

        skill_dir = staged / "skills" / skill_src.parent.name
        skill_dir.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(skill_src, skill_dir / skill_src.name)
        written.append(str(skill_dir.relative_to(staged)) + "/" + skill_src.name)

        install_src = sources / engine / "INSTALL.md"
        if install_src.exists():
            (staged / "INSTALL.md").write_text(
                substitute_text(install_src.read_text(encoding="utf-8"), a_bin),
                encoding="utf-8",
            )
            written.append("INSTALL.md")

        (staged / MARKER).write_text(
            json.dumps({
                "generator": "scripts/package-coordination.py",
                "engine": engine,
                "aplexer_bin": a_bin,
            }, indent=2) + "\n",
            encoding="utf-8",
        )

        # validate: every JSON file in the bundle parses and manifests keep keys
        for rel in written:
            if not rel.endswith(".json"):
                continue
            data = json.loads((staged / rel).read_text(encoding="utf-8"))
            required = MANIFEST_KEYS.get(f"{engine}/{rel}")
            if required:
                missing = [k for k in required if k not in data]
                if missing:
                    raise SystemExit(f"error: {engine} bundle {rel} lacks keys {missing}")

        leftover = [
            str(p.relative_to(staged)) for p in staged.rglob("*")
            if p.is_file() and PLACEHOLDER.encode() in p.read_bytes()
        ]
        if leftover:
            raise SystemExit(f"error: placeholder left in: {leftover}")

        os.rename(staged, bundle)  # atomic: validated bundle appears whole
    except BaseException:
        if staged.exists():
            shutil.rmtree(staged, ignore_errors=True)
        raise

    return {"engine": engine, "dest": str(bundle), "files": sorted(written)}


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate native aplexer coordination packages (no installation)."
    )
    parser.add_argument(
        "--engine", action="append", required=True,
        choices=[*ENGINES, "all"],
        help="engine to generate a bundle for; repeat or use 'all'",
    )
    parser.add_argument("--dest", required=True, type=Path,
                        help="destination directory for the bundles (created if missing)")
    parser.add_argument("--aplexer-bin", default=None,
                        help="absolute aplexer binary to bake into hook commands "
                             "(default: resolved from PATH)")
    parser.add_argument("--force", action="store_true",
                        help="replace existing bundles generated by this script "
                             "(marked with " + MARKER + ")")
    parser.add_argument("--json", action="store_true", help="machine-readable summary")
    args = parser.parse_args()

    engines = list(ENGINES) if "all" in args.engine else sorted(set(args.engine))
    root = repo_root()
    sources = root / "plugins" / "coordination" / "engines"
    skill_src = root / ".agents" / "skills" / "a2a-communication" / "SKILL.md"

    validate_skill(skill_src)
    for engine in engines:
        missing = [s for s in ENGINE_FILES[engine] if not (sources / engine / s).is_file()]
        if missing:
            raise SystemExit(f"error: {engine} adapter is missing sources: {missing}")
        if not (sources / engine / "INSTALL.md").is_file():
            raise SystemExit(f"error: {engine} adapter is missing INSTALL.md")

    a_bin = str(resolve_aplexer_bin(args.aplexer_bin))
    args.dest = args.dest.expanduser()
    if not args.dest.is_absolute():
        args.dest = Path.cwd() / args.dest
    args.dest.mkdir(parents=True, exist_ok=True)

    results = [build_engine(e, args.dest, sources, skill_src, a_bin, args.force)
               for e in engines]

    summary = {"aplexer_bin": a_bin, "skill": str(skill_src), "bundles": results}
    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        for r in results:
            print(f"{r['engine']}: {r['dest']} ({len(r['files'])} files)")
        print(f"aplexer binary: {a_bin}")
        print("Nothing was installed and no user config was touched.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
