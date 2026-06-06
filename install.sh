#!/bin/sh
# Build the patched dvtm + abduco and install warren into $PREFIX/bin.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# abduco and dvtm live inside this repo (their own git repos; see README)
DVTM_SRC=${DVTM_SRC:-$HERE/dvtm}
ABDUCO_SRC=${ABDUCO_SRC:-$HERE/abduco}
PREFIX=${PREFIX:-$HOME/.local}
BIN=$PREFIX/bin

echo "==> building abduco ($ABDUCO_SRC)"
make -C "$ABDUCO_SRC" >/dev/null

echo "==> building dvtm ($DVTM_SRC)"
make -C "$DVTM_SRC" dvtm >/dev/null

echo "==> installing into $BIN"
mkdir -p "$BIN"
install -m 0755 "$ABDUCO_SRC/abduco"        "$BIN/warren-abduco"
install -m 0755 "$DVTM_SRC/dvtm"            "$BIN/warren-dvtm"
install -m 0755 "$HERE/bin/warren"          "$BIN/warren"
install -m 0755 "$HERE/bin/warren-sessions" "$BIN/warren-sessions"
install -m 0755 "$HERE/bin/warren-hook"     "$BIN/warren-hook"

echo "==> installing dvtm terminfo (best effort)"
tic -s "$DVTM_SRC/dvtm.info" 2>/dev/null && echo "    installed dvtm terminfo" \
	|| echo "    skipped (will fall back to screen-256color)"

echo
echo "Done. Make sure $BIN is on your PATH, then run:  warren"
case ":$PATH:" in
	*":$BIN:"*) ;;
	*) echo "  (note: $BIN is not currently on your PATH)";;
esac
