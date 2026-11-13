#!/bin/sh
# Starts KittyConnect from this folder, the same as double-clicking
# KittyConnect.app. Its data stays beside it, in KittyConnectData.
cd "$(dirname "$0")" || exit 1
if [ ! -d "KittyConnect.app" ]; then
  echo "KittyConnect.app is not in this folder: $(pwd)"
  exit 1
fi
open "./KittyConnect.app" --args "$@"
