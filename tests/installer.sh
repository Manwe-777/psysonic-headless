#!/usr/bin/env bash
# The installer on clean distros, in Docker containers running systemd: install.sh from a local
# "release" folder (the ALSA library gets installed, the service starts and joins a hub on this
# machine), a re-run keeps the settings without asking, `update` checks versions and refuses a
# tampered download, uninstall keeps the settings and --purge deletes them.
#
#   tests/installer.sh [debian ubuntu]
#
# Builds the binary like the release (Ubuntu 22.04, so it runs on older glibc too) and runs a hub
# in its own container on a test network. Needs Docker and the hub checked out (with its
# node_modules) at ../psysonic-hub.
set -euo pipefail
cd "$(dirname "$0")/.."
DISTROS=${*:-debian ubuntu}
HUB_DIR=${HUB_DIR:-../psysonic-hub}
WORK=$(mktemp -d)
NET=psh-installer-test
cleanup() {
  docker rm -f $(docker ps -aq --filter label=psh-installer-test) >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== build (ubuntu 22.04)"
docker run --rm -v "$PWD":/src -w /src \
  -v psh-cargo:/root/.cargo -v psh-rustup:/root/.rustup -v psh-target:/target -e CARGO_TARGET_DIR=/target \
  -e PSYSONIC_HEADLESS_VERSION=0.1.0 -e DEBIAN_FRONTEND=noninteractive ubuntu:22.04 sh -c '
    set -e
    apt-get update -q >/dev/null && apt-get install -y -q curl build-essential cmake pkg-config libasound2-dev >/dev/null
    [ -x /root/.cargo/bin/cargo ] || curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null
    /root/.cargo/bin/rustup update -q stable >/dev/null 2>&1 || true
    /root/.cargo/bin/cargo build --locked --release -q'
DL=$WORK/dl
mkdir -p "$DL"
docker run --rm -v psh-target:/target -v "$DL":/out alpine sh -c \
  "cp /target/release/psysonic-headless /out/psysonic-headless-linux-x86_64 && chown $(id -u):$(id -g) /out/*"
cp packaging/install.sh tests/installer/fake_subsonic.py "$DL/"
(cd "$DL" && sha256sum psysonic-headless-linux-* > SHA256SUMS)
echo 0.1.0 > "$DL/VERSION"
# A newer release with the same binary, and a tampered one
DL2=$WORK/dl2; cp -r "$DL" "$DL2"; echo 99.0.0 > "$DL2/VERSION"
BAD=$WORK/bad; cp -r "$DL2" "$BAD"; echo tampered >> "$BAD/psysonic-headless-linux-x86_64"

echo "== hub"
TOKEN=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
docker network create "$NET" >/dev/null
docker run -d --rm --network "$NET" --name psh-hub --label psh-installer-test -v "$(realpath "$HUB_DIR")":/hub:ro \
  -w /hub -e PORT=5599 -e HUB_TOKEN="$TOKEN" node:24-alpine node server/index.ts >/dev/null
sleep 2

fail() { echo "FAIL [$distro]: $*"; docker exec "$c" journalctl -u psysonic-headless --no-pager -n 30 || true; exit 1; }
for distro in $DISTROS; do
  docker build -q -t "psh-$distro" -f "tests/installer/$distro.Dockerfile" tests/installer >/dev/null
  # Not --privileged: a privileged systemd takes over the host's VTs (see boot.sh)
  c=$(docker run -d --cgroupns=private --cap-add SYS_ADMIN --tmpfs /run --tmpfs /run/lock \
        --network "$NET" --label psh-installer-test \
        -v "$DL":/dl:ro -v "$DL2":/dl2:ro -v "$BAD":/bad:ro --hostname "psh-$distro" "psh-$distro")
  docker exec "$c" sh -c 'until systemctl is-system-running 2>/dev/null | grep -qE "running|degraded"; do sleep 1; done'
  docker exec -d "$c" python3 /dl/fake_subsonic.py
  docker exec "$c" sh -c "printf 'pass word ' > /root/pw && printf '%s\n' $TOKEN > /root/token"

  echo "== $distro: install"
  docker exec -e PSYSONIC_DOWNLOAD=/dl "$c" sh /dl/install.sh --server-url http://127.0.0.1:4533 \
    --server-user tester --server-password-file /root/pw --hub-url http://psh-hub:5599 \
    --hub-token-file /root/token --device-name "test $distro" > "$WORK/$distro.log" 2>&1 \
    || { cat "$WORK/$distro.log"; fail "install.sh"; }
  grep -q "signed in to http://127.0.0.1:4533" "$WORK/$distro.log" || fail "did not check the server"
  docker exec "$c" systemctl is-active -q psysonic-headless || fail "service not active"
  for f in server-password hub-token; do
    [ "$(docker exec "$c" stat -c '%a %U %G' /etc/psysonic-headless/$f)" = "640 root psysonic-headless" ] \
      || fail "$f is not root:psysonic-headless 0640"
  done
  [ "$(docker exec "$c" cat /etc/psysonic-headless/server-password)" = "pass word " ] || fail "password changed on the way"
  for i in $(seq 20); do
    docker exec "$c" journalctl -u psysonic-headless --no-pager | grep -q "joined the hub" && break
    sleep 1
  done
  docker exec "$c" journalctl -u psysonic-headless --no-pager | grep -q "joined the hub as \"test $distro\"" || fail "did not join the hub"
  docker exec "$c" journalctl -u psysonic-headless --no-pager | grep -q "track cache dir=/var/cache/psysonic-headless" || fail "cache not in systemd's cache directory"

  echo "== $distro: a re-run keeps the settings without asking"
  docker exec -e PSYSONIC_DOWNLOAD=/dl "$c" sh /dl/install.sh --max-bitrate 320 < /dev/null > "$WORK/$distro-2.log" 2>&1 \
    || { cat "$WORK/$distro-2.log"; fail "re-run"; }
  docker exec "$c" grep -q 'PSYSONIC_SERVER_USER="tester"' /etc/psysonic-headless/config.env || fail "lost a setting"
  docker exec "$c" grep -q 'PSYSONIC_MAX_BITRATE="320"' /etc/psysonic-headless/config.env || fail "new setting not saved"
  docker exec "$c" systemctl is-active -q psysonic-headless || fail "service not active after the re-run"

  echo "== $distro: update"
  docker exec -e PSYSONIC_DOWNLOAD=/dl "$c" psysonic-headless update --check | grep -q "0.1.0 is the latest" || fail "update --check"
  docker exec -e PSYSONIC_DOWNLOAD=/dl2 "$c" psysonic-headless update --check | grep -q "99.0.0 is available" || fail "missed 99.0.0"
  docker exec -e PSYSONIC_DOWNLOAD=/bad "$c" psysonic-headless update > "$WORK/$distro-3.log" 2>&1 && fail "installed a tampered binary"
  grep -q "doesn't match its checksum" "$WORK/$distro-3.log" || { cat "$WORK/$distro-3.log"; fail "tampering not reported"; }
  docker exec -e PSYSONIC_DOWNLOAD=/dl2 "$c" psysonic-headless update > "$WORK/$distro-4.log" 2>&1 || { cat "$WORK/$distro-4.log"; fail "update"; }
  grep -q "updated psysonic-headless 0.1.0 -> 99.0.0" "$WORK/$distro-4.log" || { cat "$WORK/$distro-4.log"; fail "update didn't say it updated"; }
  docker exec "$c" systemctl is-active -q psysonic-headless || fail "service not active after the update"

  echo "== $distro: uninstall"
  docker exec "$c" psysonic-headless uninstall >/dev/null || fail "uninstall"
  docker exec "$c" test -f /etc/psysonic-headless/config.env || fail "uninstall dropped the settings"
  docker exec "$c" sh -c '! command -v psysonic-headless' || fail "binary still there"
  docker exec -e PSYSONIC_DOWNLOAD=/dl "$c" sh /dl/install.sh < /dev/null >/dev/null 2>&1 || fail "reinstall from kept settings"
  docker exec "$c" psysonic-headless uninstall --purge >/dev/null || fail "uninstall --purge"
  docker exec "$c" test ! -e /etc/psysonic-headless || fail "--purge left the settings"
  docker exec "$c" sh -c '! id psysonic-headless' >/dev/null 2>&1 || fail "--purge left the user"
  echo "   ok"
  docker rm -f "$c" >/dev/null
done
echo "all passed: $DISTROS"
