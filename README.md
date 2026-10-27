# <img src="app/ui/logo.svg" width="40" align="top"> irch

Private, peer-to-peer community chat shaped like Discord, with no server:
spaces, channels, threads, DMs, voice and video calls. A small Rust core on
[iroh](https://iroh.computer), a terminal client, and one app for desktops
and Android.

## Install

Each keeps itself up to date.

```sh
irm https://github.com/cab-ucf/irch/releases/latest/download/install.ps1 | iex  # Windows
curl -fsSL https://github.com/cab-ucf/irch/releases/latest/download/install.sh | sh  # macOS, Linux
```

- **Android**: open [irch.apk](https://github.com/cab-ucf/irch/releases/latest/download/irch.apk)
  on the phone, or track this repo in [Obtainium](https://obtainium.imranr.dev).
- **Storage node** (always on): a space's *Storage Nodes → Run one on this
  computer*, or `install.sh … | sh -s -- --server`.
- **NixOS**: `inputs.irch.url = "github:cab-ucf/irch";`, import
  `inputs.irch.nixosModules.default`, then `programs.irch.enable = true;`
  (the app) and `services.irch = { enable = true; users = [ "you" ]; };`
  (a storage node). Updates are checked against irch's key, built by Nix.
- **From source**: `cargo run --release -p irch` (terminal, `--serve` for a
  storage node); `cd app && cargo tauri dev` or
  `cargo tauri android build --apk`. Linux needs webkit2gtk-4.1, gtk3,
  libsoup3 and, for calls, gstreamer plugins base and good.

## Features

- Spaces with channels (admins drag them into order), threads and named
  rooms, replies, reactions, edits, pins, @mentions, Markdown with code
  highlighting and Typst or LaTeX math, `||spoilers||`.
- DMs, voice messages, files sealed in chunks and kept by storage nodes
  (to 5 GB; more with the node's `--big-password`), or shared from your
  device at any size; previews of images, audio, video, PDFs.
- Photos, voice and short videos (to 25 MB) wait on the space's storage
  nodes whoever sent them, so everyone sees them inline, sender away.
- Voice and video calls with screen sharing; a DM call rings like a phone.
- Notifications on desktops and phones, per space: all, @mentions or none.
- Search on this device, Ctrl+K switcher, drafts per channel, vim keys
  (Esc, then `j`/`k`, `r`, `e`, `dd`, `?`), swipe gestures on phones.
- The terminal client is modal like vim: counts, `gg`/`G`, `/` and `n`,
  `:` commands, `l`/`h` into and out of threads, `J`/`K` channels, `?`.
- A DM closes (✕) and comes back with its next message; deleting it is
  separate (its menu, or `:leave`).
- Several profiles per device, linked devices, invisible mode, themes
  (Neon, Midnight, Nord, UCF, Light).
- *Add to notes*: a message or thread lands in a
  [loroh](https://github.com/cab-ucf/siroh) note as a task or quote, with
  an `irch://` link back.
- `irch --api`: JSON calls on stdin, answers and events on stdout, for bots
  and bridges; a [tydig](https://github.com/cab-ucf/tydig) project's
  Discussion is a channel this way.

## How it works

```text
op    = Profile | Rename | Channel | Order | Role | Post | Edit | React | Pin
      | Topic | Rekey | Back | Host | Hold | Knock | Joined | Left | Storage
      | Store | Push | Typing | Here | Call | Gone (the last four: live)
env   = { epoch tag, time, XChaCha20-Poly1305(author ‖ device ‖ sig ‖ op) }
space = a grow-only set of envs;   state = a deterministic fold of it
```

- **Everything is ciphertext.** Relays, storage nodes and the network see
  an epoch tag, a time and bytes.
- **Bans change the key**, boxed to each remaining member with X-Wing
  (ML-KEM-768 + X25519); a `Back` op keeps history readable for members.
- **Authority is a fold** in `(time, id)` order, so every replica agrees in
  any delivery order; only current admins' keys count.
- **One seed is you** on every device; a private space records what you
  joined, so linked devices follow each other.
- **Storage nodes keep ciphertext, never keys**, so messages wait while
  everyone is offline.
- **Threads are channels** named by a message's id: no extra protocol.
- **Calls are a mesh** over QUIC: Opus datagrams, VP8/H.264 one stream per
  group of pictures, so a slow peer skips to the newest keyframe.
- **Sync** compares per-day fingerprints, then sends only missing envs.
- **Phones sleep**: out of sight for a minute, the node stops and a
  doorbell (an iroh endpoint with no connections) waits. Whoever writes
  rings it, handing over the sealed message, so the phone notifies at once.
  Android must let irch run unrestricted; irch says so until it does. With
  a UnifiedPush app such as ntfy, that app wakes irch instead.

| network | terminal (app env) | who else is involved |
|---|---|---|
| default | | relays iroh.cxn.center and n0; n0 DNS; Mainline DHT |
| own relays | `--relay URL` (`IRCH_RELAY`) | your relays; Mainline DHT |
| LAN only | `--lan` (`IRCH_LAN=1`) | nobody (mDNS) |

## Limits

- No forward secrecy inside an epoch; keys rotate on bans and on request.
- Signatures are Ed25519; stored data is post-quantum, transport is not.
- Peers see each other's addresses, relays see endpoint ids and traffic,
  storage nodes see sizes, times and devices.
- Calls are for a handful of people; macOS needs Safari 26's WebKit.
- Any member, banned ones included, can have the space's storage nodes
  keep sealed files up to 25 MB each; its owner stops by unhosting it.
- A storage code is a password. Unaudited.

## Layout

| | |
|---|---|
| `core/` | ops, crypto, fold, logs, sync, node, the app's JSON API |
| `tui/` | `irch`: terminal client, `--serve`, `--install` |
| `app/` | Tauri 2 shell; `ui/` the page; `gen/android` the Android project |

Prior art: Quiet, Keet, Briar, Cwtch, SimpleX, Jami, p2panda;
calls follow n0's iroh-live. GPL-3.0-or-later.
