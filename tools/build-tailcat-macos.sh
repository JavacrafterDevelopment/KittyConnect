#!/usr/bin/env bash
# Builds both TailCat programs for macOS into vendor/macos/, where build.rs
# embeds them into the KittyConnect binary. Used by build-macos.sh and the
# release workflow. Needs Go (https://go.dev/dl/).
#
#   tools/build-tailcat-macos.sh [universal|arm64|amd64]     (default: universal)
#
# - tailcat: TailCat itself. TailCat publishes no macOS download (Homebrew
#   builds it from source), so it's built the same way: from TailCat's source
#   at the pinned version (checked against Go's checksum database), with the
#   official build tags and flags from its .goreleaser.yaml and build-tags.txt.
# - kc-voip: the VOIP rooms' helper, from voip-helper/, on the same TailCat.
#
# The TailCat versions in tools/fetch-tailcat.ps1, src/tailcat.rs and
# voip-helper/go.mod must match, or nothing is built. The licenses of
# everything compiled into each program go to vendor/macos/licenses/.
# "universal" needs lipo (part of Xcode's command line tools).

set -euo pipefail

ARCH="${1:-universal}"
case "$ARCH" in universal|arm64|amd64) ;; *) echo "Unknown architecture: $ARCH" >&2; exit 1 ;; esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/vendor/macos"
LIC="$OUT/licenses"

command -v go >/dev/null || { echo "Go is not installed. Get it from https://go.dev/dl/" >&2; exit 1; }
if [ "$ARCH" = universal ] && ! command -v lipo >/dev/null; then
  echo "lipo not found (install Xcode's command line tools), or pass arm64 or amd64." >&2
  exit 1
fi

# One TailCat version everywhere.
official="$(sed -n "s/^\$version *= *['\"]\([^'\"]*\)['\"].*/\1/p" "$ROOT/tools/fetch-tailcat.ps1" | head -n1)"
app="$(sed -n 's/^pub const VERSION: &str = "\([^"]*\)";/\1/p' "$ROOT/src/tailcat.rs" | head -n1)"
library="$(sed -n 's/^[[:space:]]*\(require[[:space:]]\{1,\}\)\{0,1\}github\.com\/tailscale\/tailcat[[:space:]]\{1,\}v\([^[:space:]]*\).*/\2/p' "$ROOT/voip-helper/go.mod" | head -n1)"
if [ -z "$library" ] || [ "$official" != "$library" ] || [ "$app" != "$library" ]; then
  echo "TailCat versions differ: fetch-tailcat.ps1 has '$official', src/tailcat.rs has '$app', voip-helper/go.mod has '$library'. Update them together." >&2
  exit 1
fi
VERSION="$library"

# TailCat's source at that version. The module cache is read-only, so it's
# built from a copy.
src="$(cd "$ROOT/voip-helper" && go mod download -json "github.com/tailscale/tailcat@v$VERSION" | sed -n 's/^[[:space:]]*"Dir": "\(.*\)",$/\1/p')"
[ -d "$src" ] || { echo "Couldn't get TailCat $VERSION's source." >&2; exit 1; }
work="$(mktemp -d)"
trap 'chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"' EXIT
cp -R "$src" "$work/tailcat"
chmod -R u+w "$work/tailcat"
TAGS="$(tr -d '\n' < "$work/tailcat/build-tags.txt")"

rm -rf "$OUT"
mkdir -p "$OUT" "$LIC"

# build <dir> <package> <output> <ldflags> <goarch>
build() {
  (cd "$1" && CGO_ENABLED=0 GOOS=darwin GOARCH="$5" go build -trimpath -tags "$TAGS" -ldflags "$4" -o "$3" "$2")
}

both() { # <dir> <package> <name> <ldflags>
  if [ "$ARCH" = universal ]; then
    build "$1" "$2" "$work/$3-arm64" "$4" arm64
    build "$1" "$2" "$work/$3-amd64" "$4" amd64
    lipo -create -output "$OUT/$3" "$work/$3-arm64" "$work/$3-amd64"
  else
    build "$1" "$2" "$OUT/$3" "$4" "$ARCH"
  fi
  chmod 755 "$OUT/$3"
}

both "$work/tailcat" ./cmd/tailcat tailcat "-s -w -X main.version=v$VERSION"
both "$ROOT/voip-helper" . kc-voip "-s -w"

# licenses <dir> <package> <name> <include main module>
licenses() {
  local to="$LIC/$3" mods
  mkdir -p "$to"
  if [ "$4" = yes ]; then
    mods="$(cd "$1" && GOOS=darwin go list -tags "$TAGS" -deps -f '{{with .Module}}{{.Path}}|{{.Version}}|{{.Dir}}{{end}}' "$2")"
  else
    mods="$(cd "$1" && GOOS=darwin go list -tags "$TAGS" -deps -f '{{with .Module}}{{if not .Main}}{{.Path}}|{{.Version}}|{{.Dir}}{{end}}{{end}}' "$2")"
  fi
  mods="$(printf '%s\ngo|%s|%s\n' "$mods" "$(go env GOVERSION)" "$(go env GOROOT)" | sed '/^$/d' | sort -u)"
  while IFS='|' read -r path ver dir; do
    local dest="$to/$(printf '%s' "$path" | tr '/\\:' '___')" found=0
    mkdir -p "$dest"
    for f in "$dir"/*; do
      case "$(basename "$f")" in
        LICENSE|LICENSE.txt|LICENSE.md|LICENCE|LICENCE.txt|LICENCE.md|COPYING|COPYING.txt|COPYING.md|NOTICE|NOTICE.txt|NOTICE.md|PATENTS|PATENTS.txt|PATENTS.md)
          cp "$f" "$dest/"; found=1 ;;
      esac
    done
    [ "$found" = 1 ] || { echo "No license file found for $path $ver in $dir" >&2; exit 1; }
    printf '%s %s\n' "$path" "${ver:-v$VERSION}" > "$dest/VERSION.txt"
  done <<< "$mods"
}

licenses "$work/tailcat" ./cmd/tailcat tailcat yes
licenses "$ROOT/voip-helper" . kc-voip no

printf '%s\n' "$VERSION" > "$OUT/tailcat.version"
echo "Built TailCat $VERSION for macOS ($ARCH) -> vendor/macos/tailcat and vendor/macos/kc-voip"
