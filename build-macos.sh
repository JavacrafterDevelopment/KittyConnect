#!/usr/bin/env bash
# Builds KittyConnect for macOS: dist/KittyConnect/ (KittyConnect.app and its
# folder) and dist/KittyConnect-macos.zip.
#
#   ./build-macos.sh              this Mac's chip only (quicker)
#   ./build-macos.sh universal    Apple Silicon and Intel in one app (needs lipo)
#
# Needs Rust (https://rustup.rs), Go (https://go.dev/dl/) and Xcode's command
# line tools (xcode-select --install).

set -euo pipefail
cd "$(dirname "$0")"

if [ "${1:-}" = universal ]; then
  ./tools/build-tailcat-macos.sh universal
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  mkdir -p target/release-universal
  lipo -create -output target/release-universal/KittyConnect \
    target/aarch64-apple-darwin/release/KittyConnect \
    target/x86_64-apple-darwin/release/KittyConnect
  bin=target/release-universal/KittyConnect
else
  case "$(uname -m)" in arm64) arch=arm64 ;; *) arch=amd64 ;; esac
  ./tools/build-tailcat-macos.sh "$arch"
  cargo build --release
  bin=target/release/KittyConnect
fi

./tools/package-macos.sh "$bin" dist
echo "Built dist/KittyConnect/KittyConnect.app"
