# KittyConnect

Portable peer-to-peer file sharing, port opening and voice/text rooms for Windows, built in Rust on
top of [TailCat](https://github.com/tailscale/tailcat), Tailscale's open-source
"netcat over WireGuard". It needs no account, no install, and no router setup.

## Use it

Copy the `KittyConnect` folder anywhere, such as a USB stick, another PC, or a zip
sent over Discord, and run `KittyConnect.exe`.

**File Share**
- *Send files*: drop files or folders in, press **Start sharing**, then **Copy for Discord**.
- *Receive files*: paste the code, press **Connect**, then download what you want.
  Files go to your Downloads folder unless you pick another one (folders keep
  their layout inside it).

**Ports**
- *Open a port*: enter the port your server listens on (e.g. `25565`) and press
  **Open ports**. Send your friend the TailCat code (`tc…`) and the port number.
- *Join a port*: paste the TailCat code, type the port number, and press **Connect**. Then connect your game or
  app to `127.0.0.1:<port>`. Turn on *Share with my network* to let a console
  or another device on your Wi-Fi use it too.

TailCat forwards **TCP** ports. Games that only use UDP won't work this way.

**VOIP**
- Type your name, then *Host a room* (you get a `kitty:voip:` code) or paste a
  code to *Join a room*.
- Each room has a text channel and a voice channel. **Join voice**, then
  **Mute** or **Deafen** as needed. Everyone in the room is listed with a
  speaking light and how they're connected. Use headphones: there's no echo
  cancellation.
- When the host ends the room, everyone sees it closed.

**Connection status.** Everything you're connected to shows **Direct** (peer to
peer) or **Relay** (through a TailCat relay, still end-to-end encrypted) with
the round-trip time. KittyConnect checks every 30 seconds through the tunnel,
and when the other side stops sharing, closes the port or ends the room, your
side disconnects by itself and says so. Downloads show their speed.

File codes look like `kitty:file:51234:tc…`, room codes `kitty:voip:50211:tc…`,
and port codes are a plain TailCat address (`tc…`). A code is a
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
- **VOIP**: the host runs a room server on `127.0.0.1` exposed with
  `tailcat serve`; members connect through `tailcat forward`, and the host over
  loopback. Names, chat and voice (16 kHz IMA ADPCM, about 64 kbit/s per
  speaker, sent only while you talk) are all frames on that one connection.
  Voice is relayed by the room, not mixed; each member mixes what they hear.
- **Every byte between peers goes through the WireGuard-encrypted TailCat
  tunnel.** KittyConnect's own servers and clients only ever use `127.0.0.1`.
  The one exception is *Share with my network* on a joined port, which lets
  devices on your own LAN reach that port on your PC.
- **Direct or relay**: TailCat runs with `--verbose`, and its
  `magicsock … now using <ip:port>` line marks the switch to a direct path. The
  30-second check is a ping through the tunnel itself: to the file server, to
  the room, or for ports to a small beacon (`127.0.0.1:47474`) the host serves
  in the same tunnel. KittyConnect doesn't use `tailcat ping`, because each
  run adds a new client to the host that's never removed.

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
