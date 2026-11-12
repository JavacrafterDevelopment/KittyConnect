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
  **Mute** or **Deafen** as needed. Voice is 48 kHz Opus at 64 kbit/s (what
  Discord uses), with noise suppression (RNNoise), an 80 Hz high-pass, and
  concealment of lost packets.
- The **gear** in the voice panel: input volume (up to 400%), automatic input
  sensitivity or a manual level with a live meter, noise suppression, and
  output volume. Yours only, remembered on this PC. If someone keeps cutting
  out, have them raise input volume or turn off automatic sensitivity and
  drag the level left. Everyone in the room is listed with a
  speaking light and how they're connected. Use headphones: there's no echo
  cancellation.
- When the host ends the room, everyone sees it closed.
- Everyone in a room needs KittyConnect 0.6 or newer: voice and screen share
  moved to UDP in 0.6, so older versions are told to update.
- **Share screen**: pick a resolution (it starts at your screen's own, e.g.
  2560×1440, with 1440p/1080p/720p/480p below it) and a frame rate (15, 30 or
  60; 30 by default) and a quality: **Text and detail** (about three times
  the bitrate, tuned for sharp edges, for code and documents), **Balanced**,
  **Smooth motion** (games, video) or **Data saver**. Then **Go live**. Others press **Watch** on your name, and **Full screen** (or double-click
  the picture; Esc to leave).
  Like Discord, it's encoded as H.264 on your GPU where available (VP8
  otherwise), at roughly 2.5 Mbit/s for 720p30 and 4.5 Mbit/s for 1080p30, with
  a key frame every 2 seconds, so new viewers and dropped frames recover quickly.
  If a viewer's connection can't keep up, the bitrate is lowered (down to a
  fifth of the chosen quality) until it can, then raised again; the stats say
  so while it's lowered.
- **Attach files** in chat with the paperclip, or drop them on the room (up to
  100 MB each). Images up to 8 MB show inline; anything can be downloaded to
  your Downloads folder. Attachments live only as long as the room.
- **Right-click someone** for their volume (0–200%) and **Mute for me**. These
  only change what you hear, and are remembered by name.
- The **host** can also **Mute for everyone** (their voice stops reaching anyone,
  they're told, and the chat says so for everyone) and **Disconnect from room**.

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

- Two TailCat programs are embedded in the exe and unpacked to `data\bin\` on
  first use:
  - **TailCat 0.7.0, the official Windows build** (hash-pinned), for File Share
    and Ports. Keeping the official build means those get TailCat's security
    updates by bumping one version.
  - **`kc-voip`**, for VOIP rooms only. It's built from `voip-helper\` on the
    TailCat *library*, the same version 0.7.0, and takes the same `serve` and
    `forward` commands. The difference is that it carries UDP as well as TCP
    on the room's port, which voice and screen share need; the official
    command line only forwards TCP. It's built with the same Go build tags as
    the official binaries, listens only on `127.0.0.1`, always uses a new key
    and the WireGuard pre-shared key, and has no other options. When the
    official tailcat can serve UDP itself, rooms can go back to it and
    `voip-helper\` can be deleted.
- Each has its own home and config folder, `data\tailcat-home\` and
  `data\voip-home\`, so no keys or settings go into your user profile, and the
  two never share any.
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
- **VOIP**: the host runs a room server on `127.0.0.1`, TCP and UDP on the
  same port, exposed with `kc-voip serve`; members connect through
  `kc-voip forward`, and the host over loopback.
  - **TCP** carries names, chat, the member list, host controls and the
    30-second check on one connection, and attachments on a second one, so a
    download never queues in front of a chat message.
  - **UDP** carries voice and screen video, inside the same WireGuard tunnel.
    TCP delivers in order, so one lost packet holds up everything behind it
    until it's resent; over UDP a lost packet costs only itself. There is no
    TCP fallback for voice or video: the UDP goes through the same tunnel as
    the TCP, so when the room's chat works, so does this. If it doesn't
    connect, the voice panel says so, and chat keeps working.
  - Each member registers their UDP socket with the room using a key from
    their welcome message, and keeps it registered every 5 seconds (which also
    keeps the tunnel's UDP flow open). The room ignores UDP from anywhere else.
  - Voice (48 kHz Opus, 64 kbit/s per speaker, sent only while you talk) is
    relayed by the room, not mixed; each member mixes what they hear. A short
    jitter buffer puts packets back in order, and a lost one is rebuilt from
    the next packet's FEC copy or concealed.
  - Screen video (WebCodecs, in the page) is cut into pieces that fit
    TailCat's 1232-byte UDP limit and paced out at about 2.5 times the
    bitrate, so a big key frame never goes in one burst, and voice is always
    sent ahead of video. It goes only to members watching that share. The room
    keeps the last second of each share; a watcher asks again for missing
    pieces, and if a frame still isn't whole in time (about a third of a
    second), skips to the next key frame and asks for one, so the picture
    stays live instead of freezing. Each watcher reports its loss and delay
    once a second, and the sharer's bitrate follows the worst.
- The UI is served from `kc://localhost` (WebView2 loads it as
  `https://kc.localhost`, a secure origin, which screen capture and WebCodecs
  need). Nothing is fetched from the network; the page and image previews come
  from inside the app.
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

It needs [Rust](https://rustup.rs) and [Go](https://go.dev/dl/). It downloads
and verifies the official TailCat into `vendor\` if it's missing, builds the
VOIP helper (`kc-voip.exe`, with the licenses of everything in it) from
`voip-helper\` with `tools\build-voip-helper.ps1`, then builds
`dist\KittyConnect\` and `dist\KittyConnect-windows-x64.zip`. `cargo test` runs
the unit tests; `go test ./...` in `voip-helper\` runs the helper's, including
a real tunnel through a local relay.

To move to a newer TailCat, change the version and hash in
`tools\fetch-tailcat.ps1`, `VERSION` in `src\tailcat.rs`, and run
`go get github.com/tailscale/tailcat@v<version>` then `go mod tidy` in
`voip-helper\`. The build refuses to run if the three disagree.

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
The licenses of everything compiled into `kc-voip` (TailCat, Tailscale,
WireGuard, gVisor, Go and the rest) ship in `licenses\kc-voip\`.
