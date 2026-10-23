# irch

Private, peer-to-peer community chat: spaces with channels, roles,
replies, reactions, mentions and direct messages, shaped like Discord,
with no server. A small Rust core over [iroh](https://iroh.computer),
one terminal client, one app for the desktop and Android.

```sh
cargo run --release -p irch                 # terminal; /help lists commands
cargo run --release -p irch -- --serve      # a storage node: keeps ciphertext
cargo install tauri-cli --version 2.12.4 --locked
cd app && cargo tauri dev                   # desktop (Linux, macOS, Windows)
cd app && cargo tauri android build --apk   # Android (SDK and NDK installed)
```

Linux needs `libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev
libjavascriptcoregtk-4.1-dev` for the app. CI builds an APK on every
pull request, runs it on an emulator, and keeps the terminal and desktop
binaries for Linux, macOS and Windows as artifacts.

## How it works

```text
op     = Profile | Rename | Channel | Role | Post | Edit | React | Rekey
       | Back | Host | Knock | Joined | Left | Storage    (Typing, Here: live)
env    = { epoch tag, time, XChaCha20-Poly1305(author ‖ device ‖ sig ‖ op) }
space  = a grow-only set of envs          state = a deterministic fold of it
live   : iroh-gossip, one topic per space
repair : set reconciliation on every new neighbor, one QUIC stream
disk   : one append-only log of envs per space; nothing else to migrate
```

- **A space is ciphertext.** Every op is signed by its author and sealed
  under the space's current epoch key. Relays, storage nodes and the
  network see an epoch tag, a time and ciphertext; who wrote what, and
  from which device, is inside.
- **Bans change the key.** Banning someone, or resetting invite links,
  makes a new epoch key and boxes it to each remaining member with
  X-Wing (ML-KEM-768 + X25519), so what is stored stays confidential
  against a future quantum computer. The banned keep what they had and
  read nothing after. A `Back` op under the new key carries the old one:
  history stays readable for members, including the ones who join later.
- **Authority is a fold.** Control ops are re-folded in `(time, id)`
  order, so "promote, then ban" resolves the same on every replica in any
  delivery order. Authors pick their own times, so the key that counts is
  the newest set by someone who is an admin now: a demoted admin's rekeys
  stop counting however they were dated, and bans and rekeys date after
  whatever they supersede. If concurrent rekeys leave the key with someone
  banned, any admin online rekeys again; keys a race cut off are handed
  on under the winner.
- **One seed is you, on every device.** Your signing and KEM keys derive
  from a 32-byte seed; every device keeps its own network key. A private
  space only you can read records what you joined and left, so linked
  devices follow each other. `/link` (My Account › Link a device) shows
  the code.
- **Storage nodes keep ciphertext, never keys.** `irch --serve` on a NAS
  or a VPS prints a code; give it to irch (`/store`, or My Account) and it
  keeps every space you are in, your private one included, so messages
  wait there while everyone else is offline, and a new device can link
  while your others are off. It cannot read any of it, so it takes
  envelopes only from your devices and the member devices they vouch for.
- **DMs** are two-person spaces, announced by a `Knock` that only the
  recipient's key opens.
- **Files** (up to 25 MiB) are sealed under a fresh key that travels only
  inside the post; their ciphertext is named by its BLAKE3 hash, fetched
  from whoever has it and kept by your storage nodes too.

Sync sends per-day bucket fingerprints (BLAKE3 of the sorted ids, so
colliding sets cannot be crafted), then ids of the days that differ, then
only missing envelopes; an unchanged space costs one small round trip.
Nothing reaches disk before it is opened and checked; what no key opens
yet waits in memory, a bounded amount of it.

## Networks

| | terminal (app: env) | who else is involved |
|---|---|---|
| default | | n0's relays (ciphertext), n0 DNS, the Mainline DHT |
| your relays | `--relay URL` (`IRCH_RELAY`) | your relays; the Mainline DHT |
| LAN only | `--lan` (`IRCH_LAN=1`) | nobody (mDNS) |

Run your own relay with `cargo install iroh-relay --features server`.
Invites carry your relay address, not your IP, whenever a relay is known.

## Using it

Terminal: `/create NAME`, `/join CODE`, `/invite`, `/channel NAME`,
`/reply TEXT`, `/react EMOJI`, `/edit TEXT`, `/delete`, `/dm USER`,
`/ban USER`, `/rekey`, `/file PATH`, `/save`, `/link`, `/store CODE`,
`/invisible`, `/help`.
Ctrl-N/P switches channel; PgUp/PgDn scrolls.

Desktop and phone: `+` creates or joins a space; the paperclip, a drop or
a paste attaches files; hover a message to react, reply, edit or delete;
click a member to message, ban or promote them. Mentions and direct
messages notify while irch is out of sight.
Ctrl+K switches, Alt+↑/↓ moves between channels, ↑ edits your last
message. On Android, Back closes the topmost thing, then sends irch to the
background; My Account › Stay connected keeps it receiving (there is no
push server to wake a peer-to-peer app).

## Limits

- No forward secrecy inside an epoch: a stolen, unlocked device reads
  what it stores, and an epoch key reads its epoch. Keys rotate on bans
  and on request, not per message.
- Authentication is Ed25519 (classical); confidentiality of what is
  stored is post-quantum, the QUIC transport is not.
- Metadata: peers learn each other's addresses when connecting directly;
  relays see endpoint ids and traffic; storage nodes see sizes, times and
  members' devices. Anyone who knows a space's id (every invite has it)
  can fetch its ciphertext from members.
- Authors date their own ops, at most a day ahead. Whoever ever held an
  epoch's key can still add ops dated inside it: a banned member's stay
  hidden, but a demoted admin's backdated bans and renames stand.
- A storage code is a password: whoever has it can make the node store
  anything. Members are trusted not to flood. Unaudited.

## Prior art

Quiet (Tor and OrbitDB), Keet (Hypercore, closed source), Briar, Cwtch,
SimpleX and Jami groups, Matrix's peer-to-peer experiments, and
[p2panda](https://p2panda.org), whose spaces and DCGKA group encryption
are the closest research. irch's niche: Discord's shape on a core small
enough to read (`core/`, about 2,200 lines), with keys that change on
bans, storage that never sees plaintext, and the same code in a
terminal, on desktops and on Android.

| | |
|---|---|
| `core/` | ops, crypto, fold, logs, sync, the node, the app's JSON API |
| `tui/` | `irch`: terminal client, `--serve`, `--headless` |
| `app/` | Tauri 2 shell; `ui/` is the page; `gen/android` the Android project |

GPL-3.0-or-later.
