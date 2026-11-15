#!/bin/bash
# End-to-end encryption check, on Linux, as root.
#
# Two KittyConnects (src/e2e.rs) connect to each other through real tunnels,
# using the app's own code for the Hub, a VOIP room (chat, voice, screen
# video, an attachment), File Share, Port Forward (and a wrong port) and
# Mounts, while every packet on the machine is captured (capture.py). Then it
# checks that no marker the test sent, and no code, ever appeared in traffic
# between the TailCat programs or through the relay: only between
# KittyConnect and its own TailCat on this PC, which never leaves a real PC.
#
# It runs twice: once letting TailCat go direct (WireGuard over UDP), once
# forced through the relay (WireGuard inside the relay's frames).
#
# What's real: KittyConnect's code, the official TailCat (built from its
# source at the pinned version, as the Mac build does) and the VOIP helper's
# tunnel code (tunnel.go, unchanged). What isn't: the relay is a local one
# (tailscale's own DERP server code) speaking plain HTTP instead of TLS, so
# the capture sees exactly what a relay sees; and the VOIP helper is told to
# use it (testregion.go, the only change to it), since a test machine can't
# reach Tailscale's relays.
#
#   sudo tools/encryption-check/run.sh
#
# Needs Go, Rust, Xvfb (xvfb-run) and Python 3.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/tools/encryption-check"
[ "$(uname)" = Linux ] || { echo "Linux only." >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "Run as root (the packet capture needs it)." >&2; exit 1; }
for c in go cargo xvfb-run python3; do command -v $c >/dev/null || { echo "$c is needed." >&2; exit 1; }; done

W="$(mktemp -d)"
# KC_KEEP=1 keeps the captures and builds (in $W) for a closer look.
trap 'pkill -f "$W/bin/" 2>/dev/null || true; if [ -z "${KC_KEEP:-}" ]; then chmod -R u+w "$W" 2>/dev/null; rm -rf "$W"; else echo "Kept $W"; fi' EXIT
mkdir -p "$W/bin"
VERSION="$(sed -n 's/^pub const VERSION: &str = "\([^"]*\)";/\1/p' "$ROOT/src/tailcat.rs")"

echo "Building TailCat $VERSION from source, the VOIP helper and a local relay…"
src="$(cd "$ROOT/voip-helper" && go mod download -json "github.com/tailscale/tailcat@v$VERSION" | sed -n 's/^[[:space:]]*"Dir": "\(.*\)",$/\1/p')"
cp -R "$src" "$W/tailcat" && chmod -R u+w "$W/tailcat"
TAGS="$(tr -d '\n' < "$W/tailcat/build-tags.txt")"
(cd "$W/tailcat" && CGO_ENABLED=0 go build -trimpath -tags "$TAGS" -o "$W/bin/tailcat" ./cmd/tailcat)

cp -R "$ROOT/voip-helper" "$W/kcvoip"
cp "$HERE/testregion.go.txt" "$W/kcvoip/testregion.go"
sed -i 's/serve(ctx, logf, port, nil)/serve(ctx, logf, port, testRegion())/' "$W/kcvoip/main.go"
grep -q 'testRegion()' "$W/kcvoip/main.go" || { echo "Couldn't point the VOIP helper at the test relay." >&2; exit 1; }
cmp "$ROOT/voip-helper/tunnel.go" "$W/kcvoip/tunnel.go"
(cd "$W/kcvoip" && CGO_ENABLED=0 go build -trimpath -tags "$TAGS" -o "$W/bin/kc-voip" .)
mkdir -p "$W/kcvoip/cmd/localrelay" && cp "$HERE/localrelay/main.go" "$W/kcvoip/cmd/localrelay/"
(cd "$W/kcvoip" && go build -o "$W/bin/localrelay" ./cmd/localrelay)

echo "Building the test…"
(cd "$ROOT" && cargo test --locked --no-run 2>&1 | tail -n 2)

status=0
for mode in direct relay; do
  echo
  echo "---------------------------------------------------------------- $mode"
  "$W/bin/localrelay" > "$W/relay.out" 2>&1 &
  for _ in $(seq 50); do grep -q '^DERPPORT=' "$W/relay.out" && break; sleep 0.1; done
  rm -f "$W/stop"
  python3 -I "$HERE/capture.py" capture "$W/$mode.pickle" "$W/stop" &
  CAP=$!
  sleep 1
  extra=()
  [ "$mode" = relay ] && extra=(TS_DEBUG_ALWAYS_USE_DERP=1)
  if ! (cd "$ROOT" && env \
      TAILCAT_DERPMAP_URL="$(sed -n 's/^DERPMAP=//p' "$W/relay.out")" \
      KC_TEST_DERP_REGION="$(sed -n 's/^REGION=//p' "$W/relay.out")" \
      TS_DEBUG_USE_DERP_HTTP=1 \
      KC_TEST_TAILCAT="$W/bin/tailcat" KC_TEST_KC_VOIP="$W/bin/kc-voip" \
      KC_E2E_OUT="$W/$mode.json" "${extra[@]}" \
      xvfb-run -a cargo test --locked e2e:: -- --ignored --nocapture --test-threads=1); then
    echo "The test itself failed ($mode)." >&2
    status=1
  fi
  touch "$W/stop"
  wait "$CAP" || true
  pkill -f "$W/bin/localrelay" || true
  if [ -f "$W/$mode.json" ]; then
    python3 -I "$HERE/capture.py" check "$W/$mode.pickle" "$W/$mode.json" "$(sed -n 's/^DERPPORT=//p' "$W/relay.out")" "$mode" || status=1
  fi
done
echo
[ $status = 0 ] && echo "All checks passed." || echo "Something failed (above)."
exit $status
