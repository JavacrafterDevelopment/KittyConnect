# KittyConnect

Portable peer-to-peer file sharing and port opening for Windows, built in Rust on
top of [TailCat](https://github.com/tailscale/tailcat), Tailscale's open-source
"netcat over WireGuard". It needs no account, no install, and no router setup.

## Use it

Copy the `KittyConnect` folder anywhere, such as a USB stick, another PC, or a zip
sent over Discord, and run `KittyConnect.exe`.

**File Share**
- *Send files*: drop files or folders in, press **Start sharing**, then **Copy for Discord**.
- *Receive files*: paste the code, press **Connect**, then download what you want.
  Files go to `Received\` beside the exe unless you pick another folder.

**Ports**
- *Open a port*: enter the port your server listens on (e.g. `25565`) and press
  **Open ports**. Send your friend the TailCat code (`tc…`) and the port number.
- *Join a port*: paste the TailCat code, type the port number, and press **Connect**. Then connect your game or
  app to `127.0.0.1:<port>`. Turn on *Share with my network* to let a console
  or another device on your Wi-Fi use it too.

TailCat forwards **TCP** ports. Games that only use UDP won't work this way.

File codes look like `kitty:file:51234:tc…`, and port codes are a plain TailCat address (`tc…`). A code is a
secret: anyone who has it can connect while that share is on. Stopping a
share kills its code for good, and the next start gets a new one.

## Portable

- TailCat 0.7.0 (the official Windows build, hash-pinned) is embedded in the exe
  and unpacked to `data\bin\` on first use.
- TailCat's home and config are pointed at `data\tailcat-home\`, so no keys or
  settings go into your user profile.
- The WebView2 profile lives in `data\webview\`.
- Every TailCat process is tied to a Windows job object, so they all close when
  KittyConnect does, even if it crashes.

It needs the Microsoft Edge WebView2 runtime, which comes with Windows 11 and
most Windows 10 installs. For machines without it, drop a fixed-version
runtime into a `WebView2\` folder beside the exe.

## How it works

- **Ports**: the host runs `tailcat serve <ports>`, and the joiner runs
  `tailcat forward <addr> <local>:<remote>`.
- **Files**: the sharer runs a small file server on `127.0.0.1` and exposes
  only that port with `tailcat serve`. The receiver forwards a local port to it
  and requests files by id. Only files you added can be fetched. Received names
  are cleaned so they can't escape the save folder.

## Build

```
powershell -ExecutionPolicy Bypass -File build.ps1
```

This downloads and verifies TailCat into `vendor\` if it's missing, then builds
`dist\KittyConnect\` and `dist\KittyConnect-windows-x64.zip`. `cargo test` runs
the unit tests.

## Releases and the Launcher

KittyConnect is listed in the Javacrafter Launcher, which installs and updates
it. To publish a version:

1. Bump `version` in `Cargo.toml` (e.g. `0.1.1`) and commit.
2. Tag that commit `v0.1.1` as an annotated tag and push. GitHub Desktop pushes
   annotated tags along with the commits.

[`.github/workflows/release.yml`](.github/workflows/release.yml) then builds
on GitHub, attaches `KittyConnect-windows-x64.zip` (with a SHA-256 manifest of
every file) to the release, and updates `update.json` on the `updates` branch,
which is where the Launcher looks for new versions.

TailCat is © Tailscale Inc & contributors, BSD 3-Clause (`TAILCAT-LICENSE`).
