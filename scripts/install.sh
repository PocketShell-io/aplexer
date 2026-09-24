#!/usr/bin/env bash
# Install the aplexer CLI: one real binary, with `a` as a symlink alias.
#
#   scripts/install.sh                     # build --release, install to ~/.local/bin
#   scripts/install.sh ~/.cargo/bin        # install into another bin dir
#   scripts/install.sh --bin PATH [DIR]    # install an already-built binary
#
# The alias is created as a symlink, never a copy, so upgrading is always a
# single-file operation and the two names cannot drift apart. A stale
# regular-file `a` left by an older copy-based install is replaced.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)

prebuilt=""
if [ "${1:-}" = "--bin" ]; then
  [ $# -ge 2 ] || { echo "usage: $0 --bin PATH [BIN_DIR]" >&2; exit 2; }
  prebuilt=$2
  shift 2
fi
bin_dir=${1:-"$HOME/.local/bin"}
mkdir -p "$bin_dir"

if [ -n "$prebuilt" ]; then
  src=$(cd "$(dirname "$prebuilt")" && pwd)/$(basename "$prebuilt")
else
  (cd "$ROOT" && cargo build --release --bin aplexer)
  src="$ROOT/target/release/aplexer"
fi

# Atomic replace: a concurrently running client or worker never executes a
# half-written file.
tmp=$(mktemp "$bin_dir/.aplexer.XXXXXX")
cp "$src" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$bin_dir/aplexer"

# `-sfn`: -f also replaces a stale regular file or an old symlink; -n keeps
# the replacement from following the old link into a directory.
ln -sfn aplexer "$bin_dir/a"

echo "installed $src"
echo "  as $bin_dir/aplexer"
echo "  alias $bin_dir/a -> aplexer"
