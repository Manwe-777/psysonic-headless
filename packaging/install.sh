#!/bin/sh
# psysonic-headless installer for Linux. Downloads the binary for this machine, checks it against
# SHA256SUMS, makes sure the ALSA library is there, then runs `sudo psysonic-headless install`
# with the same arguments:
#
#   curl -fsSL https://github.com/Manwe-777/psysonic-headless/releases/latest/download/install.sh | sh
#
# It asks for what it needs (server, user, password, hub, hub token); or pass the settings, e.g.
#   ... | sh -s -- --server-url https://music.example.com --server-user me --hub-url https://hub.example.com
# Run it again to update (the settings are kept). See `psysonic-headless install --help`.
#
# PSYSONIC_DOWNLOAD overrides where the files come from (a release URL or a local directory).
set -eu

BASE=${PSYSONIC_DOWNLOAD:-https://github.com/Manwe-777/psysonic-headless/releases/latest/download}

fail() { echo "psysonic-headless install: $*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || fail "only Linux"
case $(uname -m) in
  x86_64 | amd64) ARCH=x86_64 ;;
  aarch64 | arm64) ARCH=aarch64 ;;
  *) fail "no build for $(uname -m) (x86_64 and aarch64 only; a Raspberry Pi needs a 64-bit OS)" ;;
esac
[ -e /lib/ld-musl-* ] 2>/dev/null && fail "musl systems (Alpine) aren't supported: the build needs glibc"

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  fail "needs curl or wget"
fi
case $BASE in
  /*) fetch() { cp "$1" "$2"; } ;;
esac
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
  fail "needs sha256sum"
fi
if [ "$(id -u)" = 0 ]; then
  as_root() { "$@"; }
elif command -v sudo >/dev/null 2>&1; then
  as_root() { sudo "$@"; }
else
  fail "run as root, or install sudo"
fi

# The ALSA library (libasound.so.2), which the player plays through.
has_alsa() { ldconfig -p 2>/dev/null | grep -q 'libasound\.so\.2 ' || [ -e /usr/lib/libasound.so.2 ]; }
if ! has_alsa; then
  echo "installing the ALSA library"
  if command -v apt-get >/dev/null 2>&1; then
    as_root apt-get update -q >/dev/null
    as_root apt-get install -y -q libasound2t64 >/dev/null 2>&1 || as_root apt-get install -y -q libasound2 >/dev/null
  elif command -v dnf >/dev/null 2>&1; then as_root dnf install -y -q alsa-lib
  elif command -v pacman >/dev/null 2>&1; then as_root pacman -S --noconfirm --needed alsa-lib
  elif command -v zypper >/dev/null 2>&1; then as_root zypper --non-interactive install libasound2
  else fail "install the ALSA library (libasound2 / alsa-lib), then run this again"
  fi
  has_alsa || fail "the ALSA library is still missing"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM
NAME=psysonic-headless-linux-$ARCH
fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || fail "can't download $BASE/SHA256SUMS"
echo "downloading $NAME"
fetch "$BASE/$NAME" "$TMP/psysonic-headless" || fail "can't download $BASE/$NAME"
want=$(awk -v f="$NAME" '$2 == f || $2 == "*" f { print $1 }' "$TMP/SHA256SUMS")
[ -n "$want" ] || fail "$NAME isn't listed in SHA256SUMS"
[ "$(sha256 "$TMP/psysonic-headless")" = "$want" ] || fail "$NAME doesn't match its checksum: not installing"
chmod 755 "$TMP/psysonic-headless"

as_root "$TMP/psysonic-headless" install "$@"
