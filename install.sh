#!/bin/sh
# Build warren v1 (single Rust binary) and install it into $PREFIX/bin.
#
# The legacy v0 pieces (abduco/dvtm/bin) are no longer installed; if you still
# have running v0 agents, the previous launcher is preserved as `warren0`
# (reach a v0 agent directly with:
#   ABDUCO_SOCKET_DIR=~/.warren/agents warren-abduco -a <name>).
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PREFIX=${PREFIX:-$HOME/.local}
BIN=$PREFIX/bin

echo "==> building warren v1 (release)"
cargo build --release --manifest-path "$HERE/v1/Cargo.toml"

echo "==> installing into $BIN"
mkdir -p "$BIN"
# Keep the v0 launcher reachable during the transition.
if [ -f "$BIN/warren" ] && head -2 "$BIN/warren" | grep -q '^#!/bin/sh'; then
	cp "$BIN/warren" "$BIN/warren0"
	echo "    (v0 shell launcher preserved as warren0)"
fi
install -m 0755 "$HERE/v1/target/release/warren" "$BIN/warren"

echo
echo "Done. Make sure $BIN is on your PATH, then run:  warren"
case ":$PATH:" in
	*":$BIN:"*) ;;
	*) echo "  (note: $BIN is not currently on your PATH)";;
esac
