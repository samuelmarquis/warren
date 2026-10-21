#!/bin/sh
# Build warren (single Rust binary) and install it into $PREFIX/bin.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PREFIX=${PREFIX:-$HOME/.local}
BIN=$PREFIX/bin

echo "==> building warren (release)"
cargo build --release --manifest-path "$HERE/Cargo.toml"

echo "==> installing into $BIN"
mkdir -p "$BIN"
install -m 0755 "$HERE/target/release/warren" "$BIN/warren"

# Next to bin/, where man(1) looks for a PATH entry's pages.
MAN=$PREFIX/share/man/man1
echo "==> installing the manual into $MAN"
mkdir -p "$MAN"
install -m 0644 "$HERE/man/warren.1" "$MAN/warren.1"

echo
echo "Done. Make sure $BIN is on your PATH, then run:  warren"
case ":$PATH:" in
	*":$BIN:"*) ;;
	*) echo "  (note: $BIN is not currently on your PATH)";;
esac
