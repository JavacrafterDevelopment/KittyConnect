# KittyConnect

Portable peer-to-peer file sharing, port opening and voice/text rooms for Windows and macOS, built in Rust on
top of [TailCat](https://github.com/tailscale/tailcat), Tailscale's open-source
"netcat over WireGuard". It needs no account, no install, and no router setup.

## Use it

Copy the `KittyConnect` folder anywhere, such as a USB stick, another PC, or a zip
sent over Discord, and run `KittyConnect.exe` (or `Start KittyConnect.bat`,
which does the same; the Launcher offers both).

**On a Mac** (macOS 11 or newer, Apple Silicon or Intel): install it from the
Launcher, or open `KittyConnect-macos.dmg` and drag KittyConnect to
Applications (or unzip `KittyConnect-macos.zip` anywhere and open
`KittyConnect.app` or `Start KittyConnect.command`). The app isn't notarized by
Apple, so the first time macOS says it can't check it: right-click
KittyConnect.app and choose **Open** (on macOS 15, open it once, then press
**Open Anyway** in System Settings → Privacy & Security). The Launcher does
this for you. Everything works as on Windows, except:

- **Going live** (sharing your screen) isn't available yet: Apple's web view
  doesn't let apps capture the screen. You can still **watch** others
  (macOS 13.3 or newer).
- **Network permission** is a Windows Firewall fix and isn't shown. The Mac
  firewall is off unless you turned it on; if it is, macOS asks the first time
  KittyConnect connects. macOS 15 may also ask to let KittyConnect find
  devices on your local network: allow it, or direct connections between
  people on the same network fall back to Relay.
- macOS asks before KittyConnect uses the microphone (the first time you join
  voice) and before it saves into Downloads (the first download).
- Shortcuts use ⌘ instead of Ctrl.

Each time it opens, it asks for your name. The box shows a random suggestion in
grey: press **OK** to use it, or type your own. Your name is used everywhere
in the app (shown top right; click it to change it). It's what people see in
rooms, and the host of a port or file share you join sees it next to how
you're connected. It only travels inside the encrypted TailCat tunnel.

**File Share**
- *Send files*: drop files or folders in, press **Start sharing**, then **Copy for Discord**.
  Everyone who connects is listed by name, each with **Direct** or **Relay**.
- *Receive files*: paste the code, press **Connect**, then download what you want.
  Files go to your Downloads folder unless you pick another one (folders keep
  their layout inside it).

**Port Forward** opens on two choices:

*Game servers*: one click to host or join.
- *Host*: press **Host** next to the game. The port box shows the game's usual
  port in grey (25565 for Minecraft); leave it, or type another. Send your
  friends the code (`kitty:game.minecraft:25565:tc…`).
- *Join*: paste the code and press **Join**. The code says which game and port.
- **Minecraft: Java Edition** shows up like a LAN world. The host opens their
  world to LAN (Esc → Open to LAN, Port Number 25565, Start LAN World), and
  everyone who joined sees it under LAN in Multiplayer and joins from there.
  The host's card says what friends see, or what to fix (such as Minecraft
  having picked a different port). A dedicated server on that port shows up
  the same way, as "*name*'s Minecraft server".
- Terraria/tModLoader (7777), Starbound (21025) and Vintage Story (42420) open
  their port; friends join with the address KittyConnect shows them.

*Manual port forward*: any TCP port by number.
- *Open a port*: enter the port your server listens on (e.g. `8080`) and press
  **Open port**. Send your friend the TailCat code (`tc…`) and the port number.
  Everyone who joins is listed under that port by name, each with **Direct** or
  **Relay**.
- *Join a port*: paste the TailCat code, type the port number, and press **Connect**. Then connect your game or
  app to `127.0.0.1:<port>`. Turn on *Share with my network* to let a console
  or another device on your Wi-Fi use it too.

TailCat forwards **TCP** ports. Games that only use UDP won't work this way.

**VOIP**
- *Host a room* (you get a `kitty:voip:` code) or paste a code to *Join a room*.
  You show up under the name you picked when the app opened.
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
  **One slow viewer doesn't lower the picture for everyone.** Where the
  encoder can, frames come in three frame-rate layers ("layered" in your
  stats). A viewer whose connection can't keep up, such as one on a relay or
  a VPN, is sent fewer frames (15 instead of 30 a second, then 7.5, then key
  frames only) at the same sharpness, and goes back up a step after a few
  good seconds, while everyone else keeps the full stream. Only if even the
  viewer doing best falls behind (your own connection, or the host's, which
  everyone shares) is the bitrate lowered, down to a fifth of the chosen
  quality, until it can keep up; the stats say so while it's lowered.
- **Attach files** in chat with the paperclip, or drop them on the room (up to
  100 MB each). Images up to 8 MB show inline; anything can be downloaded to
  your Downloads folder. Attachments live only as long as the room.
- **Right-click someone** for their volume (0–200%) and **Mute for me**. These
  only change what you hear, and are remembered by name.
- The **host** can also **Mute for everyone** (their voice stops reaching anyone,
  they're told, and the chat says so for everyone) and **Disconnect from room**.

**Connection status.** Everything you're connected to shows **Direct** (peer to
peer) or **Relay** (through a TailCat relay, still end-to-end encrypted) with
the round-trip time, and the name of whoever you joined. When you're the one
sharing (a file share, a port or a room), everyone connected is listed with
their own.

**Stuck on Relay?**
- **Retry direct** shows next to anything on a relay, in File Share and Port
  Forward. On your own connection it reconnects your end of the tunnel, which
  tries for a direct path from scratch. Whatever goes through it drops for a
  moment, so it's hidden while you're downloading. Next to someone connected
  to you, it asks their KittyConnect to do the same at its next check (every
  10 seconds while on a relay).
- **Network permission** (bottom bar): if Windows' "allow on your network?"
  question for TailCat was cancelled or missed, Windows blocks direct
  connections and never asks again by itself. This makes Windows forget its
  answer for KittyConnect and its TailCat programs (after an administrator's
  OK), so it asks again when you reconnect. Press **Allow**. KittyConnect checks every 30 seconds through the tunnel,
and when the other side stops sharing, closes the port or ends the room, your
side disconnects by itself and says so. Downloads show their speed.

File codes look like `kitty:file:51234:tc…`, room codes `kitty:voip:50211:tc…`,
and port codes are a plain TailCat address (`tc…`). A code is a
secret: anyone who has it can connect while that share is on. Stopping a
share kills its code for good, and the next start gets a new one.

## Portable

- Two TailCat programs are embedded in the exe and unpacked to `data\bin\` on
  first use:
  - **TailCat 0.7.0** itself, for File Share and Port Forward, so those get
    TailCat's security updates by bumping one version. On Windows it's **the
    official build** (hash-pinned). TailCat publishes no macOS build, so on a
    Mac it's built from TailCat's source at the same version, with the official
    build tags and flags, the way Homebrew builds it
    (`tools/build-tailcat-macos.sh`).
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
  KittyConnect does, even if it crashes. macOS has no job objects, so a small
  watchdog (`sh`, in its own process group) waits for KittyConnect to exit,
  however it exits, and then ends the TailCat programs it started. Only
  programs in `data/bin/` are ever ended. Leftovers from a run whose watchdog
  was stopped too are ended at the next start.
- **On a Mac**, the data folder is `KittyConnectData` beside `KittyConnect.app`
  (writing inside the app would break its signature), or wherever the Launcher
  says. When there's nowhere writable beside it, such as when macOS runs an app
  opened straight from Downloads from a read-only copy, it's
  `~/Library/Application Support/KittyConnect`.

It needs the Microsoft Edge WebView2 runtime, which comes with Windows 11 and
most Windows 10 installs. For machines without it, drop a fixed-version
runtime into a `WebView2\` folder beside the exe.

## How it works

- **Port Forward**: the host runs `tailcat serve <ports>`, and the joiner runs
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
    once a second.
  - Frame-rate layers: the sharer's encoder makes L1T3 (on the graphics card
    where it can; in software up to 1080p30; otherwise none). Each frame says
    its layer and which frame it builds on, in spare bits of its flags byte,
    so older versions read it as before. For each watcher that understands
    layers, the room sends only the layers they keep up with: down a layer
    when their delay goes past 400 ms (or loss past 25%; scattered loss is
    made good by asking again), up one after four good seconds, and only at
    a frame where nothing sent afterwards builds on a frame they didn't get.
    A watcher shows each frame whose base it has; a lost top-layer frame
    costs only itself, a lost base-layer one is asked for again. The
    sharer's bitrate follows the watcher doing best.
- **Minecraft LAN worlds**: Minecraft announces a LAN world with a UDP message
  on port 4445 every 1.5 seconds (`[MOTD]name[/MOTD][AD]port[/AD]`). The host's
  KittyConnect listens on 4445 (sharing it with Minecraft) for messages from
  this PC's own addresses only. Joiners ask the host's beacon every 3 seconds,
  through the tunnel, which worlds are open on the port they joined, and send
  the same message to their own Minecraft on `127.0.0.1:4445`, pointing at the
  local end of the tunnel. Nothing about it leaves either PC except through
  the tunnel.
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
- **Who's connected**: only the joining side can tell whether its own tunnel
  went direct, so each joiner reports it, with its name, a random id for that
  connection and its last round-trip time, in that check
  (`{"op":"ping","peer":…,"name":…,"path":…}`), and again at once when its
  path or name changes. The host answers with its own name, and lists
  everyone it has heard from in the last 75 seconds; disconnecting says
  goodbye so you drop off at once. A port host's open ports share the one
  beacon, so the check also names the code it joined. Versions before 0.7
  send a bare ping: they still connect, but aren't listed.

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

**macOS**: `./build-macos.sh` (this Mac's chip) or `./build-macos.sh
universal` (Apple Silicon and Intel in one app). It needs Rust, Go and Xcode's
command line tools (`xcode-select --install`). It builds both TailCat programs
from source with `tools/build-tailcat-macos.sh`, the app, and
`dist/KittyConnect/` (KittyConnect.app, `Start KittyConnect.command`,
licenses) with `dist/KittyConnect-macos.zip` and a `.dmg`. The Mac icon is
`assets/KittyConnect.icns`, drawn by `tools/make-icns.py` (the same cat as
`src/icon.rs`).

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
both on GitHub, attaches `KittyConnect-windows-x64.zip` and
`KittyConnect-macos.zip` (each with a SHA-256 manifest of every file) and
`KittyConnect-macos.dmg` to the release, and updates `update.json` and
`update-macos.json` on the `updates` branch, which is where the Launcher looks
for new versions. [`check.yml`](.github/workflows/check.yml) builds and tests
both on every push to a work branch, and keeps the Mac app as a download.

TailCat is © Tailscale Inc & contributors, BSD 3-Clause (`TAILCAT-LICENSE`).
The licenses of everything compiled into `kc-voip` (TailCat, Tailscale,
WireGuard, gVisor, Go and the rest) ship in `licenses\kc-voip\`.
