# Portside game API

Games (and any other program) on the same PC as Portside can use it: see the
hub you're in and who's in it, your voice channel, show what you're playing
next to your name, send small messages to the same game on friends' PCs, and
host or join a multiplayer session with friends in your hub, peer to peer,
over **UDP or TCP**.

It's on by default (each game still has to be allowed by you the first time). Turn it off in **Settings › Game API** if you don't want it.

## How it stays private

- Portside listens on **127.0.0.1** only, never on your network: nothing on
  another PC can reach it.
- The first time a program connects, Portside asks you: **Allow** or
  **Don't allow**, showing the program's file. Once allowed, the program gets
  a key. Portside keeps only a hash of it, and the key only works from the
  program file it was given to. Remove a program in Settings any time.
- A web page can't use it: browsers can't open a plain connection like this,
  and anything that isn't the API's JSON is closed at once.
- Game traffic between friends goes through a tunnel of its own: Portside's
  VOIP helper (TailCat with UDP), WireGuard-encrypted end to end, with a new
  key for every session, the same as voice. Its address goes only to the
  people in your hub, through the hub's own encrypted tunnel. Your game only
  ever sends to and listens on 127.0.0.1.

## Connecting

Connect with TCP to `127.0.0.1`, trying ports **47800 to 47809** in order;
Portside is the one that greets you with:

```json
{"op":"portside","v":1,"version":"0.16.0"}
```

Everything is one JSON object per line (UTF-8, ending in `\n`), each way, at
most 64 KB a line. Send a hello first, within 15 seconds:

```json
{"op":"hello","v":1,"app":{"id":"com.you.space-game","name":"Space Game"},"token":"<your key, if you have one>"}
```

- `app.id`: 1 to 64 lowercase letters, digits, `.`, `-` or `_`. The same game
  must use the same id on every PC: messages and sessions only reach the same
  id.
- `app.name`: what Portside shows ("Space Game is asking…", "Playing Space Game").
- `token`: leave it out the first time.

What comes back:

| Reply | Meaning |
|---|---|
| `{"op":"asking"}` | Portside is asking the user. Wait (up to 2 minutes). |
| `{"op":"authorized","token":"…"}` | Allowed. **Save the token** and send it next time. |
| `{"op":"ready","v":1,"app":"…"}` | You're in: send requests. |
| `{"op":"denied","error":"…"}` | Not allowed (then the connection closes). After "Don't allow", the same id can't ask again for a minute. |

## Requests

```json
{"id":1,"cmd":"getHub"}
```

Each reply has your `id`:

```json
{"id":1,"ok":true,"data":{…}}
{"id":1,"ok":false,"error":"Join or start a hub in Portside first…"}
```

`id` can be any JSON value. Replies can arrive out of order (`hostSession`
and `joinSession` wait for a tunnel), and events can come between them.

| cmd | Arguments | data |
|---|---|---|
| `getMe` | | `{"name":"Kit","inHub":true,"inVoice":false}` |
| `getHub` | | the hub you're in (below), or `null` |
| `getSavedHubs` | | `[{"id","name","expiresIn":secs,"open":bool}]` |
| `getVoice` | | your voice room (below), or `null` |
| `getSessions` | | sessions in the hub for **your** game id (below) |
| `setActivity` | `text` (up to 60 characters) | `{}`: shows "Playing *name* · *text*" next to you in the hub |
| `clearActivity` | | `{}` |
| `sendMessage` | `data` (any JSON, up to 16 KB), `to` (member ids; leave out for everyone) | `{}` |
| `hostSession` | `port`, `title`, `info` (any JSON, up to 2 KB) | `{"sid":"1a2b3c4d","port":7777}` once the tunnel is up |
| `updateSession` | `sid`, and `title` and/or `info` | `{}` |
| `stopSession` | `sid` | `{}` |
| `joinSession` | `session` (a session's `id`) | `{"id":"1:1a2b3c4d","port":51234}` once it's listening |
| `leaveSession` | `session` | `{}` |

The hub:

```json
{"id":"…","name":"Kit's hub","saved":false,"host":true,"me":1,"games":true,
 "members":[{"id":1,"name":"Kit","host":true,"me":true,"path":"host","rtt":null,"activity":null},
            {"id":2,"name":"Jo","host":false,"me":false,"path":"direct","rtt":21,
             "activity":{"app":"Space Game","text":"In a lobby"}}]}
```

`games` is false when the hub's host runs a Portside older than 0.16.0: then
sessions and messages don't work in that hub.

Voice:

```json
{"room":"Hangout","me":3,"host":false,"inVoice":true,"muted":false,"deaf":false,"channel":0,
 "channels":[{"id":0,"name":"General"}],
 "members":[{"id":3,"name":"Kit","host":false,"voice":true,"muted":false,"deaf":false,"channel":0,"sharing":false,"me":true}]}
```

(Voice member ids are the room's, not the hub's.)

A session:

```json
{"id":"2:1a2b3c4d","sid":"1a2b3c4d","title":"Jo's lobby","info":{"map":"moon","players":1},
 "host":{"id":2,"name":"Jo"},"mine":false}
```

## Events

Sent whenever something changes (and once after `ready`):

| evt | data |
|---|---|
| `hub` | the hub, as `getHub` (`null` when you leave it) |
| `voice` | voice, as `getVoice` |
| `speaking` | `{"ids":[3,5]}`: who's talking, by voice member id |
| `sessions` | your game's sessions, as `getSessions` |
| `message` | `{"from":{"id":2,"name":"Jo"},"data":…}` from the same game on someone else's PC |
| `sessionEnded` | `{"sid":…}` or `{"id":…}`, and `error`: a session's tunnel stopped |

```json
{"evt":"message","data":{"from":{"id":2,"name":"Jo"},"data":{"chat":"gg"}}}
```

## Multiplayer

**Hosting.** Start your game's server listening on `127.0.0.1` (IPv4; or on
all addresses) on some port, UDP, TCP or both. Then:

```json
{"id":5,"cmd":"hostSession","port":7777,"title":"Kit's lobby","info":{"map":"moon","players":1}}
```

When the reply comes, everyone in your hub sees the session (their games get
a `sessions` event). Each friend who joins reaches your server from
`127.0.0.1`, with their own source port, so your server tells them apart the
usual way: by the address packets come from (UDP) or by connection (TCP).
`updateSession` changes its `title` or `info` (the player count, say), and
everyone sees the change. `stopSession` ends it; so does disconnecting from
the API (or Portside closing).

**Joining.**

```json
{"id":6,"cmd":"joinSession","session":"1:1a2b3c4d"}
```

The reply has a `port` on this PC. Send UDP to `127.0.0.1:<port>`, or
connect TCP to it, exactly as if the host's server were on this PC.

Things to know:

- The tunnel comes up when you first send, so the **first few UDP packets
  can be lost** (up to a few seconds while it reaches the host). Send a hello
  and resend it until the host answers; most game netcode does this anyway.
- A UDP flow with nothing sent either way for 3 minutes ends; the next
  packet starts it again. Send a keepalive every few seconds.
- It's direct peer to peer when the networks allow (hole punching), else it
  goes through a TailCat relay, still encrypted end to end. Settings shows
  which.
- Hosting is a star: friends connect to the host. For everyone-to-everyone,
  each player can host a session and the others join it.
- Small things (lobby chat, "ready", picking a map) can go as `sendMessage`
  instead; those travel through the hub's host, up to 60 a second each.

## Example

`examples/game-api/portside_client.py` is a small client with no libraries
beyond Python's own, plus a UDP echo demo:

```
python portside_client.py info              # shows your hub and voice
python portside_client.py host 7777         # hosts a UDP echo server as a session
python portside_client.py join              # (on a friend's PC) joins it and pings through it
```

The protocol's own reference is the comment at the top of `src/api/mod.rs`.
