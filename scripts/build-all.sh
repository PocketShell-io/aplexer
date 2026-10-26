#!/usr/bin/env bash
# Cross-compile aplexer release artifacts for every supported target on a
# Linux x86_64 host (Hetzner), without GitHub Actions.
#
# Supported targets (6):
#   linux-amd64    x86_64-unknown-linux-gnu      (glibc capped at 2.28 via zig)
#   linux-arm64    aarch64-unknown-linux-gnu     (glibc capped at 2.28 via zig)
#   windows-amd64  x86_64-pc-windows-msvc        (MSVC SDK via cargo-xwin)
#   windows-arm64  aarch64-pc-windows-msvc       (MSVC SDK via cargo-xwin)
#   macos-amd64    x86_64-apple-darwin           (zig cc wrapper; needs the
#   macos-arm64    aarch64-apple-darwin           macOS port in this tree)
#
# macOS cross-linking uses zig's bundled macOS SDK through two small
# wrapper scripts (~/.local/bin/zig-cc-{x86_64,aarch64}-macos, created by
# install_toolchain). The wrappers drop rustc's `-exported_symbols_list`
# (unsupported by zig's Mach-O driver; default visibility links fine) and
# `-liconv` (the libc crate links it unconditionally, zig ships no stub,
# and nothing in this workspace references iconv_* symbols).
#
# Usage:
#   scripts/build-all.sh            # full build + verify + smoke into dist/
#   scripts/build-all.sh --skip-smoke
#   scripts/build-all.sh --skip-toolchain   # reuse already-installed tools
#
# Toolchains are installed under $HOME (no sudo):
#   ~/.rustup toolchains, ~/.local/zig, ~/.local/bin, ~/.cache/cargo-xwin,
#   ~/.local/aplexer-build-venv (maturin + auditwheel).
#
# Output: dist/ with final release asset names:
#   a-<platform>, aplexer-<platform> (... .exe suffix on Windows)
#   aplexer-<ver>-py3-none-{manylinux_2_28_x86_64,manylinux_2_28_aarch64,
#     win_amd64,win_arm64,macosx_11_0_x86_64,macosx_11_0_arm64}.whl
#   aplexer_client-<ver>-cp311-abi3-{...same tags...}.whl
#   SHA256SUMS
#
# Verification is enforced, not advisory: any missing artifact, wrong
# executable header (PE 0x8664/0xAA64, ELF x86-64/AArch64, Mach-O
# x86_64/arm64), or wrong wheel platform tag fails the script.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/dist"
SKIP_SMOKE=0
SKIP_TOOLCHAIN=0
for arg in "$@"; do
    case "$arg" in
        --skip-smoke) SKIP_SMOKE=1 ;;
        --skip-toolchain) SKIP_TOOLCHAIN=1 ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

# ---- pinned toolchain -------------------------------------------------------
RUST_VERSION="1.85.0"       # matches .github/workflows/release.yml
ZIG_VERSION="0.15.2"
CARGO_ZIGBUILD_VERSION="0.20.1"
CARGO_XWIN_VERSION="0.19.2"
MATURIN_VERSION="1.14.1"    # matches .github/workflows/release.yml
AUDITWHEEL_VERSION="6.8.1"  # matches .github/workflows/release.yml
PATCHELF_VERSION="0.18.0"   # static patchelf for auditwheel repair
QEMU_VERSION="v7.2.0-1"     # multiarch/qemu-user-static tag (aarch64 smoke)
MACOSX_DEPLOYMENT_TARGET="11.0"

export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export PATH="$HOME/.local/aplexer-build-venv/bin:$PATH"  # maturin/auditwheel/patchelf
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export RUSTUP_TOOLCHAIN="$RUST_VERSION"

log() { printf 'build-all: %s\n' "$*"; }
fail() { printf 'build-all: ERROR: %s\n' "$*" >&2; exit 1; }

install_toolchain() {
    [ "$SKIP_TOOLCHAIN" = 1 ] && { log "skipping toolchain install"; return; }
    command -v rustup >/dev/null || fail "rustup not found; install rustup first"
    log "installing Rust $RUST_VERSION + targets"
    rustup toolchain install "$RUST_VERSION" --profile minimal >/dev/null
    rustup target add --toolchain "$RUST_VERSION" \
        x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
        x86_64-pc-windows-msvc aarch64-pc-windows-msvc \
        x86_64-apple-darwin aarch64-apple-darwin >/dev/null

    if ! [ -x "$HOME/.local/zig/zig" ] || \
       [ "$("$HOME/.local/zig/zig" version 2>/dev/null)" != "$ZIG_VERSION" ]; then
        log "installing zig $ZIG_VERSION to ~/.local/zig"
        tmp="$(mktemp -d)"
        trap 'rm -rf "$tmp"' RETURN
        curl -fsSL -o "$tmp/zig.tar.xz" \
            "https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz"
        rm -rf "$HOME/.local/zig"
        mkdir -p "$HOME/.local/zig"
        tar -xf "$tmp/zig.tar.xz" -C "$HOME/.local/zig" --strip-components=1
        rm -rf "$tmp"
        trap - RETURN
    fi
    mkdir -p "$HOME/.local/bin"
    ln -sf "$HOME/.local/zig/zig" "$HOME/.local/bin/zig"
    # llvm-dlltool shim for pyo3 generate-import-lib (Windows cross wheels).
    printf '#!/bin/bash\nexec "%s" dlltool "$@"\n' \
        "$HOME/.local/zig/zig" > "$HOME/.local/bin/llvm-dlltool"
    chmod +x "$HOME/.local/bin/llvm-dlltool"

    # macOS cross-link wrappers (see the header comment for why they filter).
    cat > "$HOME/.local/bin/zig-cc-wrap.sh" <<'WRAP'
#!/bin/bash
target="$1"; shift
args=()
skip_next=0
for arg in "$@"; do
    if [ "$skip_next" = 1 ]; then skip_next=0; continue; fi
    base="$arg"
    case "$base" in
        -Wl,*) base="${base#-Wl,}" ;;
    esac
    case "$base" in
        -exported_symbols_list|-exported_symbol) skip_next=1 ;;
        -liconv) ;;
        *) args+=("$arg") ;;
    esac
done
exec zig cc -target "$target" "${args[@]}"
WRAP
    chmod +x "$HOME/.local/bin/zig-cc-wrap.sh"
    printf '#!/bin/bash\nexec "$HOME/.local/bin/zig-cc-wrap.sh" x86_64-macos "$@"\n' \
        > "$HOME/.local/bin/zig-cc-x86_64-macos"
    printf '#!/bin/bash\nexec "$HOME/.local/bin/zig-cc-wrap.sh" aarch64-macos "$@"\n' \
        > "$HOME/.local/bin/zig-cc-aarch64-macos"
    chmod +x "$HOME/.local/bin/zig-cc-x86_64-macos" "$HOME/.local/bin/zig-cc-aarch64-macos"

    log "installing cargo-zigbuild $CARGO_ZIGBUILD_VERSION + cargo-xwin $CARGO_XWIN_VERSION"
    cargo +"$RUST_VERSION" install --locked --root "$HOME/.local" \
        cargo-zigbuild@"$CARGO_ZIGBUILD_VERSION" cargo-xwin@"$CARGO_XWIN_VERSION" \
        -j "$CARGO_BUILD_JOBS" -q

    log "installing maturin $MATURIN_VERSION + auditwheel $AUDITWHEEL_VERSION (venv)"
    if ! [ -x "$HOME/.local/aplexer-build-venv/bin/python" ]; then
        if command -v uv >/dev/null 2>&1; then
            uv venv "$HOME/.local/aplexer-build-venv"
        else
            # Debian/Ubuntu: needs the python3-venv package (apt install python3-venv)
            python3 -m venv "$HOME/.local/aplexer-build-venv"
        fi
    fi
    if [ -x "$HOME/.local/aplexer-build-venv/bin/pip" ]; then
        "$HOME/.local/aplexer-build-venv/bin/pip" install -q \
            "maturin==$MATURIN_VERSION" "auditwheel==$AUDITWHEEL_VERSION" \
            "patchelf==$PATCHELF_VERSION"
    elif command -v uv >/dev/null 2>&1; then
        uv pip install -q --python "$HOME/.local/aplexer-build-venv/bin/python" \
            "maturin==$MATURIN_VERSION" "auditwheel==$AUDITWHEEL_VERSION" \
            "patchelf==$PATCHELF_VERSION"
    else
        fail "no pip in build venv and no uv; install python3-venv or uv"
    fi
}

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/python-cli/pyproject.toml" | head -1)"
[ -n "$VERSION" ] || fail "could not read version from python-cli/pyproject.toml"

# platform -> rust target / exe suffix / expected header machine / wheel tag
PLATFORMS="linux-amd64 linux-arm64 windows-amd64 windows-arm64 macos-amd64 macos-arm64"
rust_target() { case "$1" in
    linux-amd64) echo x86_64-unknown-linux-gnu ;;
    linux-arm64) echo aarch64-unknown-linux-gnu ;;
    windows-amd64) echo x86_64-pc-windows-msvc ;;
    windows-arm64) echo aarch64-pc-windows-msvc ;;
    macos-amd64) echo x86_64-apple-darwin ;;
    macos-arm64) echo aarch64-apple-darwin ;; esac; }
exe_suffix() { case "$1" in windows-*) echo ".exe" ;; *) echo "" ;; esac; }
expect_machine() { case "$1" in
    linux-amd64) echo "elf-x86-64" ;;
    linux-arm64) echo "elf-aarch64" ;;
    windows-amd64) echo "pe-8664" ;;
    windows-arm64) echo "pe-aa64" ;;
    macos-amd64) echo "macho-x86-64" ;;
    macos-arm64) echo "macho-arm64" ;; esac; }
wheel_tag() { case "$1" in
    linux-amd64) echo "manylinux_2_28_x86_64" ;;
    linux-arm64) echo "manylinux_2_28_aarch64" ;;
    windows-amd64) echo "win_amd64" ;;
    windows-arm64) echo "win_arm64" ;;
    macos-amd64) echo "macosx_11_0_x86_64" ;;
    macos-arm64) echo "macosx_11_0_arm64" ;; esac; }

# ---- build ------------------------------------------------------------------
install_toolchain
MATURIN="$HOME/.local/aplexer-build-venv/bin/maturin"
[ -x "$MATURIN" ] || fail "maturin missing at $MATURIN"

rm -rf "$DIST"
INTER="$ROOT/.allarch-tmp"
rm -rf "$INTER"
mkdir -p "$DIST" "$INTER/stage" "$INTER/wheels-cli-native" "$INTER/wheels-cli" "$INTER/wheels-client"

for plat in $PLATFORMS; do
    target="$(rust_target "$plat")"
    log "building binaries: $plat ($target)"
    case "$plat" in
        linux-*)
            cargo zigbuild --locked -j "$CARGO_BUILD_JOBS" --release \
                --bins --target "${target}.2.28" ;;
        windows-*)
            cargo xwin build --locked -j "$CARGO_BUILD_JOBS" --release \
                --bins --target "$target" ;;
        macos-*)
            CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$HOME/.local/bin/zig-cc-x86_64-macos" \
            CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$HOME/.local/bin/zig-cc-aarch64-macos" \
            MACOSX_DEPLOYMENT_TARGET="$MACOSX_DEPLOYMENT_TARGET" \
            cargo build --locked -j "$CARGO_BUILD_JOBS" --release \
                --bins --target "$target" ;;
    esac
    src="$ROOT/target/$target/release/aplexer$(exe_suffix "$plat")"
    [ -f "$src" ] || fail "binary missing after build: $src"
    mkdir -p "$INTER/stage/aplexer-bins-$plat"
    cp "$src" "$INTER/stage/aplexer-bins-$plat/aplexer$(exe_suffix "$plat")"
done

for plat in $PLATFORMS; do
    log "building aplexer CLI wheel: $plat"
    python3 "$ROOT/scripts/build-wheels.py" --platform "$plat" \
        --binaries-dir "$INTER/stage" --output-dir "$INTER/wheels-cli-native"
done
for plat in $PLATFORMS; do
    tag="$(wheel_tag "$plat")"
    case "$plat" in
        linux-*)
            # pick the one wheel matching this platform's arch
            src=""
            for w in "$INTER"/wheels-cli-native/*.whl; do
                case "$plat,$w" in
                    linux-amd64,*linux_x86_64*) src="$w" ;;
                    linux-arm64,*linux_aarch64*) src="$w" ;;
                esac
            done
            [ -n "$src" ] || fail "native CLI wheel missing for $plat"
            if [ "$plat" = "linux-amd64" ]; then
                # Native arch: auditwheel validates and repairs the tag.
                "$HOME/.local/aplexer-build-venv/bin/python" -m auditwheel repair \
                    --plat "$tag" --only-plat --wheel-dir "$INTER/wheels-cli" "$src" >/dev/null
            else
                # Cross arch: auditwheel repair only runs natively, but
                # `auditwheel show` analyses any ELF. Verify conformance, then
                # apply the exact relabel repair would have made (the wheel
                # vendors no external libs; the binary links system libc only).
                "$HOME/.local/aplexer-build-venv/bin/python" -m auditwheel show "$src" \
                    | grep "platform tag: \"$tag\"" >/dev/null \
                    || fail "auditwheel: $src is not $tag conformant"
                python3 - "$src" "$tag" "$INTER/wheels-cli" <<'PY'
import base64, csv, hashlib, io, sys, zipfile
from pathlib import Path
src, new_tag, outdir = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
parts = src.name.split("-")
assert parts[-1].startswith("linux_"), src.name
parts[-1] = new_tag + ".whl"
dst = outdir / "-".join(parts)
def digest(b):
    return "sha256=" + base64.urlsafe_b64encode(hashlib.sha256(b).digest()).rstrip(b"=").decode()
with zipfile.ZipFile(src) as zin:
    names = zin.namelist()
    dist_info = next(n.split("/")[0] for n in names if n.endswith(".dist-info/WHEEL"))
    wheel_name = f"{dist_info}/WHEEL"
    blob = zin.read(wheel_name).decode()
    new_blob = "\n".join(
        f"Tag: py3-none-{new_tag}" if line.startswith("Tag: ") else line
        for line in blob.splitlines()
    ) + "\n"
    assert new_blob != blob, "WHEEL tag unchanged"
    with zipfile.ZipFile(dst, "w", zipfile.ZIP_DEFLATED) as zout:
        records = []
        for n in names:
            if n.endswith("/RECORD"):
                continue
            data = new_blob.encode() if n == wheel_name else zin.read(n)
            zout.writestr(n, data)
            records.append((n, digest(data), str(len(data))))
        records.append((f"{dist_info}/RECORD", "", ""))
        buf = io.StringIO()
        csv.writer(buf, lineterminator="\n").writerows(records)
        zout.writestr(f"{dist_info}/RECORD", buf.getvalue().encode())
print(f"relabeled {dst.name}")
PY
            fi
            ;;
        windows-*|macos-*)
            # Tags are final (no auditwheel for PE/Mach-O on this host);
            # build-wheels.py already emitted the release tag.
            cp "$INTER"/wheels-cli-native/aplexer-"$VERSION"-py3-none-"$tag".whl \
                "$INTER/wheels-cli/" || fail "CLI wheel missing for $plat"
            ;;
    esac
done

for plat in $PLATFORMS; do
    target="$(rust_target "$plat")"
    tag="$(wheel_tag "$plat")"
    log "building aplexer-client wheel: $plat"
    case "$plat" in
        linux-*)
            ( cd "$ROOT/python" && "$MATURIN" build --release --locked \
                --target "$target" --zig --compatibility manylinux_2_28 \
                --out "$INTER/wheels-client" ) ;;
        windows-*)
            # generate-import-lib: cross-link without a Windows Python install.
            # The `python` feature must be repeated: maturin CLI --features
            # replaces (not extends) the features from python/pyproject.toml.
            # (maturin drives cargo-xwin itself; no xwin env eval in this shell --
            # stacking two evals in one shell mixes x64/arm64 lib paths.)
            ( cd "$ROOT/python" && "$MATURIN" build --release --locked \
                --features python,pyo3/generate-import-lib \
                --target "$target" --out "$INTER/wheels-client" ) ;;
        macos-*)
            # Wrapper linkers (see header comment) + deployment target floor.
            # The `python` feature must be repeated (see windows note above).
            ( cd "$ROOT/python" && \
                CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$HOME/.local/bin/zig-cc-x86_64-macos" \
                CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$HOME/.local/bin/zig-cc-aarch64-macos" \
                MACOSX_DEPLOYMENT_TARGET="$MACOSX_DEPLOYMENT_TARGET" \
                "$MATURIN" build --release --locked \
                --features python \
                --target "$target" --out "$INTER/wheels-client" ) ;;
    esac
done

# ---- collect release assets -------------------------------------------------
for plat in $PLATFORMS; do
    suf="$(exe_suffix "$plat")"
    src="$INTER/stage/aplexer-bins-$plat/aplexer$suf"
    cp "$src" "$DIST/a-$plat$suf"
    cp "$src" "$DIST/aplexer-$plat$suf"
done
cp "$INTER"/wheels-cli/*.whl "$DIST/"
cp "$INTER"/wheels-client/*.whl "$DIST/"

# ---- verify -----------------------------------------------------------------
log "verifying headers, tags, and completeness"
python3 - "$DIST" "$VERSION" <<'PY'
import struct, sys, zipfile
from pathlib import Path

dist, version = Path(sys.argv[1]), sys.argv[2]
plats = ["linux-amd64", "linux-arm64", "windows-amd64", "windows-arm64",
         "macos-amd64", "macos-arm64"]
expect = {
    "linux-amd64": ("elf", 62, "manylinux_2_28_x86_64"),
    "linux-arm64": ("elf", 183, "manylinux_2_28_aarch64"),
    "windows-amd64": ("pe", 0x8664, "win_amd64"),
    "windows-arm64": ("pe", 0xAA64, "win_arm64"),
    "macos-amd64": ("macho", 0x01000007, "macosx_11_0_x86_64"),
    "macos-arm64": ("macho", 0x0100000C, "macosx_11_0_arm64"),
}
errors = []
wanted = set()
for plat in plats:
    kind, machine, tag = expect[plat]
    suf = ".exe" if plat.startswith("windows-") else ""
    wanted.add(f"a-{plat}{suf}")
    wanted.add(f"aplexer-{plat}{suf}")
    wanted.add(f"aplexer-{version}-py3-none-{tag}.whl")
    wanted.add(f"aplexer_client-{version}-cp311-abi3-{tag}.whl")
    for name in (f"a-{plat}{suf}", f"aplexer-{plat}{suf}"):
        p = dist / name
        if not p.is_file():
            errors.append(f"missing binary {name}")
            continue
        data = p.read_bytes()
        if kind == "elf":
            if data[:4] != b"\x7fELF":
                errors.append(f"{name}: not ELF")
            elif struct.unpack("<H", data[18:20])[0] != machine:
                errors.append(f"{name}: wrong ELF machine")
        elif kind == "pe":
            if data[:2] != b"MZ":
                errors.append(f"{name}: not PE")
            else:
                e = struct.unpack("<I", data[0x3C:0x40])[0]
                m = struct.unpack("<H", data[e + 4:e + 6])[0]
                if m != machine:
                    errors.append(f"{name}: PE machine {m:#x} != {machine:#x}")
        else:  # macho: FAT or MH_MAGIC_64, little-endian
            if data[:4] == b"\xca\xfe\xba\xbe":
                errors.append(f"{name}: fat binary, expected thin {tag}")
            elif data[:4] != b"\xcf\xfa\xed\xfe":
                errors.append(f"{name}: not Mach-O 64-bit")
            else:
                _, cputype, _, filetype = struct.unpack("<IIII", data[:16])
                if cputype != machine:
                    errors.append(f"{name}: Mach-O cputype {cputype:#x} != {machine:#x}")
                if filetype != 0x2:  # MH_EXECUTE
                    errors.append(f"{name}: Mach-O filetype {filetype:#x} != MH_EXECUTE")
    for prefix, wtag in (("aplexer-", tag), ("aplexer_client-", tag)):
        matches = sorted(dist.glob(f"{prefix}{version}-*{wtag}.whl"))
        if len(matches) != 1:
            errors.append(f"expected exactly one {prefix}*{wtag}.whl, found {len(matches)}")

have = {p.name for p in dist.iterdir() if p.is_file()} - {"SHA256SUMS"}
if have != wanted:
    errors.append(f"asset set mismatch: missing {sorted(wanted - have)}, extra {sorted(have - wanted)}")
# RECORD integrity for every wheel (catches a broken relabel/repair).
import base64, csv, hashlib
for w in sorted(dist.glob("*.whl")):
    with zipfile.ZipFile(w) as z:
        try:
            rec = z.read([n for n in z.namelist() if n.endswith("/RECORD")][0]).decode()
        except IndexError:
            errors.append(f"{w.name}: RECORD missing"); continue
        for row in csv.reader(rec.splitlines()):
            if len(row) != 3:
                errors.append(f"{w.name}: bad RECORD row {row}"); continue
            path, want, size = row
            if not want:
                continue
            try:
                data = z.read(path)
            except KeyError:
                errors.append(f"{w.name}: RECORD lists missing {path}"); continue
            got = "sha256=" + base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
            if got != want or str(len(data)) != size:
                errors.append(f"{w.name}: RECORD mismatch for {path}")
if errors:
    print("\n".join(f"VERIFY FAIL: {e}" for e in errors))
    sys.exit(1)
print(f"verify OK: {len(wanted)} assets, headers and wheel tags match")
PY

( cd "$DIST" && sha256sum -- a-* aplexer-* *.whl > SHA256SUMS )
log "wrote $DIST/SHA256SUMS"

# ---- smoke ------------------------------------------------------------------
if [ "$SKIP_SMOKE" = 1 ]; then
    log "smoke skipped"
    exit 0
fi

log "smoke: linux-amd64 native"
# Product constraint: control socket paths must fit in SUN_LEN (108).
# The agent TMPDIR is deep inside the worktree, so smoke state goes to /tmp.
# The ambient APLEXER_WORKSPACE/APLEXER_SESSION_ID (when this script itself
# runs inside an aplexer session) must not leak into the smoke: bare-tag
# resolution prefers them over the cwd, which would point the smoke kill
# at the wrong workspace.
export TMPDIR=/tmp
unset APLEXER_SESSION_ID
export APLEXER_WORKSPACE="$ROOT"
"$DIST/aplexer-linux-amd64" --version
APLEXER_STATE_DIR="$(mktemp -d)" APLEXER_RUNTIME_DIR="$(mktemp -d)"
export APLEXER_STATE_DIR APLEXER_RUNTIME_DIR
"$DIST/aplexer-linux-amd64" start --workspace "$ROOT" --tag smoke-native -- sleep 60
"$DIST/aplexer-linux-amd64" list | grep smoke-native >/dev/null
"$DIST/aplexer-linux-amd64" kill smoke-native
"$DIST/aplexer-linux-amd64" prune || true
rm -rf "$APLEXER_STATE_DIR" "$APLEXER_RUNTIME_DIR"

log "smoke: linux-amd64 wheel (venv install + import + CLI)"
smokevenv="$(mktemp -d -p /tmp)"
if command -v uv >/dev/null 2>&1; then
    uv venv "$smokevenv" >/dev/null
    uv pip install --offline --python "$smokevenv/bin/python" \
        --no-index --find-links "$INTER/wheels-client" \
        "$DIST"/aplexer-"$VERSION"-py3-none-manylinux_2_28_x86_64.whl
else
    # Debian/Ubuntu: needs the python3-venv package (apt install python3-venv)
    python3 -m venv "$smokevenv"
    "$smokevenv/bin/python" -m pip install -q --no-index \
        --find-links "$INTER/wheels-client" \
        "$DIST"/aplexer-"$VERSION"-py3-none-manylinux_2_28_x86_64.whl
fi
"$smokevenv/bin/python" - <<'PY'
from importlib.metadata import version
import aplexer, aplexer_cli
print("aplexer", version("aplexer"), "client", version("aplexer-client"))
PY
"$smokevenv/bin/a" --version
"$smokevenv/bin/aplexer" --version
rm -rf "$smokevenv"

log "smoke: linux-arm64 via qemu-user-static"
# QEMU_LD_PREFIX must NOT be set: it splits the guest filesystem view
# (dirfd walks from "/" land in the sysroot while absolute AT_FDCWD
# lookups do not), which breaks path-heavy binaries. Instead, patchelf a
# COPY of the artifact to use an arm64 sysroot loader+rpath and run with
# a plain host filesystem view. The shipped dist/ binary is untouched.
if ! [ -x "$HOME/.local/bin/qemu-aarch64-static" ]; then
    curl -fsSL -o "$HOME/.local/bin/qemu-aarch64-static" \
        "https://github.com/multiarch/qemu-user-static/releases/download/${QEMU_VERSION}/qemu-aarch64-static"
    chmod +x "$HOME/.local/bin/qemu-aarch64-static"
fi
if ! [ -f "$HOME/.local/arm64-sysroot/sysroot/usr/lib/aarch64-linux-gnu/libc.so.6" ]; then
    # Ubuntu 24.04 arm64 libc (loader + libs). No sudo: direct .deb fetch.
    log "fetching arm64 sysroot (Ubuntu ports libc6)"
    mkdir -p "$HOME/.local/arm64-sysroot"
    curl -fsSL -o "$HOME/.local/arm64-sysroot/libc6-arm64.deb" \
        "http://ports.ubuntu.com/ubuntu-ports/pool/main/g/glibc/libc6_2.39-0ubuntu8.9_arm64.deb"
    ( cd "$HOME/.local/arm64-sysroot" && dpkg -x libc6-arm64.deb sysroot )
fi
SYSROOT="$HOME/.local/arm64-sysroot/sysroot"
cp "$DIST/aplexer-linux-arm64" "$TMPDIR/aplexer-arm64-smoke"
"$HOME/.local/aplexer-build-venv/bin/patchelf" \
    --set-interpreter "$SYSROOT/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1" \
    --set-rpath "$SYSROOT/usr/lib/aarch64-linux-gnu" \
    "$TMPDIR/aplexer-arm64-smoke"
"$HOME/.local/bin/qemu-aarch64-static" "$TMPDIR/aplexer-arm64-smoke" --version
S4="$HOME/.local/tmp/qemu-state" R4="$HOME/.local/tmp/qemu-runtime"
rm -rf "$S4" "$R4"; mkdir -p "$S4" "$R4"
APLEXER_STATE_DIR="$S4" APLEXER_RUNTIME_DIR="$R4" \
    "$HOME/.local/bin/qemu-aarch64-static" "$TMPDIR/aplexer-arm64-smoke" \
    start --workspace /tmp --tag smoke-qemu -- sleep 60
APLEXER_STATE_DIR="$S4" APLEXER_RUNTIME_DIR="$R4" \
    "$HOME/.local/bin/qemu-aarch64-static" "$TMPDIR/aplexer-arm64-smoke" \
    list | grep smoke-qemu >/dev/null
APLEXER_STATE_DIR="$S4" APLEXER_RUNTIME_DIR="$R4" \
    "$HOME/.local/bin/qemu-aarch64-static" "$TMPDIR/aplexer-arm64-smoke" \
    kill /tmp:smoke-qemu
rm -rf "$S4" "$R4" "$TMPDIR/aplexer-arm64-smoke"

log "smoke: windows binaries are header-verified only (no wine on host);"
log "smoke: windows-amd64 + windows-arm64 runtime deferred to real machines."
log "smoke: macOS binaries are header-verified only (no Mac on host);"
log "smoke: macos-amd64 + macos-arm64 runtime runs in the mirror CI"
log "smoke: (GitHub-hosted macos-14 + macos-15, cargo test + start/list/stop)."
log "ALL DONE: dist/ has $(find "$DIST" -maxdepth 1 -type f | wc -l) files"
