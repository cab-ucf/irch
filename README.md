# irch

Private, peer-to-peer community chat: spaces with channels, threads,
roles, replies, reactions, mentions, direct messages and voice and video
calls, shaped like Discord, with no server. A small Rust core over
[iroh](https://iroh.computer), one terminal client, one app for the
desktop and Android.

## Install

Each of these keeps itself up to date.

```sh
# Windows (PowerShell)
irm https://github.com/cab-ucf/irch/releases/latest/download/install.ps1 | iex
# macOS, Linux
curl -fsSL https://github.com/cab-ucf/irch/releases/latest/download/install.sh | sh
```

- **Android**: open [irch.apk](https://github.com/cab-ucf/irch/releases/latest/download/irch.apk)
  on the phone and let the browser install it, once; or add this repository
  to [Obtainium](https://obtainium.imranr.dev).
- **iPhone**: not yet. Apple allows no install without its developer
  program and review.
- **An always-on storage node**: in the app, a space's Storage Nodes → *Run
  one on this computer*; or `… | sh -s -- --server`; or, on Windows,
  `& ([scriptblock]::Create((irm …/install.ps1))) -Server`.

Fetched by `curl` or PowerShell, nothing is marked as downloaded from the
internet, so neither Gatekeeper nor SmartScreen asks. The macOS app is
signed ad hoc; with an Apple Developer ID in the release's secrets it is
notarized too, and the `.dmg` opens from a browser download without a
question. Until then, a browser-downloaded `.dmg` needs one click in
System Settings → Privacy & Security, never `xattr`.

### NixOS

A flake (`/etc/nixos/flake.nix`):

```nix
inputs.irch.url = "github:cab-ucf/irch";
inputs.irch.inputs.nixpkgs.follows = "nixpkgs";
# outputs: in nixpkgs.lib.nixosSystem { modules = [ … ]; }
inputs.irch.nixosModules.default
```

(In a module's `imports` instead, `inputs` must come through
`specialArgs = { inherit inputs; };`, or Nix recurses forever.)

Without flakes: `imports = [ "${builtins.fetchTarball
"https://github.com/cab-ucf/irch/archive/main.tar.gz"}/nixos.nix" ];`.
Then:

```nix
programs.irch.enable = true;      # the app, and `irch` in a terminal
services.irch.enable = true;      # a storage node, always on
services.irch.users = [ "you" ];  # your irch pairs with it in one step
```

Hourly, a service fetches the newest release's source, checks it against
irch's signing key, and builds it with Nix as its own user; the app and
the node switch to that build (`packages.<system>.irch` is the flake's).

### From source

```sh
cargo run --release -p irch                 # terminal; /help lists commands
cargo run --release -p irch -- --serve      # a storage node: keeps ciphertext
cargo install tauri-cli --version 2.12.4 --locked
cd app && cargo tauri dev                   # desktop (Linux, macOS, Windows)
cd app && cargo tauri android build --apk   # Android (SDK and NDK installed)
```

Linux needs `libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev
libjavascriptcoregtk-4.1-dev` for the app, and
`gstreamer1.0-plugins-base` and `-good` for calls. CI builds an APK on
every pull request, runs it on an emulator, and keeps the terminal and
desktop binaries for Linux, macOS and Windows as artifacts.

## How it works

```text
op     = Profile | Rename | Channel | Role | Post | Edit | React | Rekey
       | Back | Host | Knock | Joined | Left | Storage
       | Typing | Here | Call | Gone              (live: gossiped, not kept)
env    = { epoch tag, time, XChaCha20-Poly1305(author ‖ device ‖ sig ‖ op) }
space  = a grow-only set of envs          state = a deterministic fold of it
live   : iroh-gossip, one topic per space
repair : set reconciliation on every new neighbor, one QUIC stream
calls  : QUIC between callers; audio as datagrams, video a stream per GOP
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
  or a VPS prints a code; give it to irch (`/store CODE`, or My Account)
  and it keeps your private space, so a new device can link while your
  others are off, and each space you choose (its menu's Storage Nodes,
  or `/store here`), so messages wait there while everyone else is
  offline. It cannot read any of it, so it takes
  envelopes only from your devices and the member devices they vouch for.
  An admin's storage node is listed in the space too, so members fetch
  from it and invites send newcomers there first.
- **Threads need no op of their own.** A message's thread is the channel
  named by the first 8 bytes of its id: replying in it is posting there,
  so two people who start the same thread at once start one, and unread
  counts, mentions, edits and sync come for free. A `Topic` names one
  (its starter's or an admin's, newest wins): a room, listed at once.
- **DMs** are two-person spaces, announced by a `Knock` that only the
  recipient's key opens.
- **Files** (up to 25 MiB) are sealed under a fresh key that travels only
  inside the post; their ciphertext is named by its BLAKE3 hash, fetched
  from whoever has it and kept by your storage nodes too.
- **Shared files** (any size: `/share PATH`, or "Share from this device")
  stay where they are and are not sealed: the post names their hash, and
  the sender's devices serve them, over QUIC, to members' devices only,
  while online. Asked for one that is offline, irch says to ask its
  sender to open irch. Big uploads go this way by themselves.
- **Calls are a mesh.** Everyone in a channel's call connects straight to
  everyone else in it: to devices a member's signed `Call` names, which
  must then sign as that member. The page captures and codes (WebCodecs:
  Opus, VP8 or H.264); the core only carries frames: audio as QUIC
  datagrams, never resent; video as one stream per group of pictures,
  each outranking the last, as in Media over QUIC, so a peer that falls
  behind skips to the newest keyframe instead of lagging. Nothing of a
  call is stored.

Sync sends per-day bucket fingerprints (BLAKE3 of the sorted ids, so
colliding sets cannot be crafted), then ids of the days that differ, then
only missing envelopes; an unchanged space costs one small round trip.
Nothing reaches disk before it is opened and checked; what no key opens
yet waits in memory, a bounded amount of it.

## Networks

| | terminal (app: env) | who else is involved |
|---|---|---|
| default | | relays `iroh.cxn.center`, n0's `use1-1`; n0 DNS; Mainline DHT |
| your relays | `--relay URL` (`IRCH_RELAY`) | your relays; the Mainline DHT |
| LAN only | `--lan` (`IRCH_LAN=1`) | nobody (mDNS) |

Run your own relay with `cargo install iroh-relay --features server`.
Invites carry your relay address, not your IP, whenever a relay is known.

## A server

An always-on member's desktop keeps the spaces it is in synced, but it can
read them and covers only those. A storage node (`irch --serve`) keeps
the spaces its users choose, ciphertext only. `irch --install` keeps one
running as you (systemd for your user, a LaunchAgent, a scheduled task),
at login, or at boot where Linux allows lingering; `--uninstall` stops
it. Its code waits in your data directory, so irch on the same computer
pairs with it in one click: a space's Storage Nodes, "this computer's"
(`/store local` in the terminal). On NixOS `services.irch.enable` runs
one as its own user, and its code waits in `/run/irch/code` for the
users listed in `services.irch.users`; `journalctl -u irch | grep -A1
'Storage code'` prints the code to hand out.

## Updates

The apps check for a newer release at launch (and in My Account) and
install it: desktops in place, the update's signature checked against
the key in `tauri.conf.json`; Android through its installer, which takes
only an APK signed with irch's own key. A storage node from a release
checks every six hours and replaces itself, under the same signature.
An irch the updater cannot replace (built with cargo, an unpacked
AppImage) opens the release page instead; Nix builds leave it to NixOS.
`release.yml` publishes a release for a tag, run by hand with that tag
(its secrets are named at its top), then installs it on Linux, macOS and
Windows and updates an older irch to it.

## Using it

Terminal: `/create NAME`, `/join CODE`, `/invite`, `/channel NAME`,
`/reply TEXT`, `/thread TEXT`, `/room NAME`, `/search WORDS`, `/react EMOJI`, `/edit TEXT`, `/delete`,
`/dm USER`, `/ban USER`, `/rekey`, `/file PATH`, `/share PATH`, `/save`, `/link`,
`/store CODE|local|here`, `/note [FILE]`, `/todo [FILE]`, `/invisible`,
`/help`.
Ctrl-N/P switches channel; PgUp/PgDn scrolls.

Messages are Markdown (markdown-it: headings, lists, tables, quotes,
code fenced with its language, highlighted by Prism), `||spoilers||`,
and math, `$...$` inline and `$$...$$` displayed: Typst's (`$sum_(k=1)^n
k = (n(n+1))/2$`, typeset by Typst itself, as MathML, in its own font),
or LaTeX (`$\frac{1}{2}$`, by Temml) when it has a `\` or a `{`. Math
can only be math: no `#` code, files or packages. Raw HTML is shown as
text, and links are copied, never opened.

Desktop and phone: a weechat-like tree of spaces and channels beside the
chat, IRC-dense rows (time, nick, text), members on request. `+` creates
or joins a space; the paperclip, a drop or a paste attaches files, and a
file's name previews it (images, audio, video, PDFs, highlighted text);
hover a message to react, reply, edit or delete; click a member to
message, call, ban or promote them; `@` in a message offers who is
here. The magnifier (Ctrl+F) searches this room or channel (with its
threads), this space, or everywhere: on this device, which holds the
history decrypted, so nobody else is asked. Ctrl+K jumps to a channel
or DM, and `@name` there to anyone to message. The arrow on a message replies in
its thread, listed under its channel; a channel's `+` opens a named room
(a thread before anyone replies), and its pencil renames it. The phone
in a channel's header joins its group call, the one on a DM rings that
person, and video keeps its shape (a phone held upright sends upright); mute, camera, screen and hang up sit under the tiles (the
terminal shows how many are in one: 🔊2). New messages notify like texts,
with a sound and (on phones) a buzz, except in the room on screen; the
space's menu picks all, @mentions, or none. A call in a DM rings like a
phone call: the ringtone until answered, declined or 45 s pass, full
screen on a locked phone, Answer and Decline in the notification; one in
a channel says it started.

The composer takes `/nick`, `/join`, `/dm`, `/me`, `/shrug`, `/thread`,
`/invite`, `/leave`, `/notes`, `/help`. Esc leaves it for normal mode:
`j`/`k` moves, `gg`/`G` jumps, `r` replies, `t` threads, `e` edits, `dd`
deletes, `y` copies, `+` reacts, `n` notes, `i` returns, `?` lists them.
Ctrl+K switches, Alt+↑/↓ moves between channels, ↑ edits your last
message. On phones, swipe right for channels and left for members; Back
closes the topmost thing (a thread returns to its channel), then sends
irch to the background.

Phones sleep: holding iroh connections open would wake the radio every
few seconds. Instead, whoever writes wakes the others. Best, with no
Google and nothing else to install: a phone paired with its owner's
storage node (`irch --serve`, on any always-on computer, as many as you
like) holds one TCP connection to one of them, which sends a byte every
4½ minutes (the phone's kernel answers it; irch sleeps) and "wake" the
moment anyone writes: the writer asks that phone's nodes over iroh until
one has it. A node offers this on TCP 7745 (`--wake-port`; NixOS opens
it): phones must reach it there, so behind a router forward that port
and say where (`--wake-addr host:7745`, `services.irch.wakeAddress`).
The node is checked by its key, the phone by its pairing. Else, a
UnifiedPush app ([ntfy](https://ntfy.sh): from F-Droid, or from Google
Play on ntfy.sh, which rides Google's own connection at no battery cost)
tells its spaces, sealed like any op, where to reach it; the writer's
device sends that address a ping with nothing in it (at most every 30
s), and the phone wakes for a few seconds, fetches from the writer over
iroh, and notifies. Push servers learn only that a ping went to an
address. Starting a call wakes the others the same way, and anyone in it
says so at once to a device that comes back. With no push app, irch
stays connected instead (a quiet "Connected for messages and calls"
notification, from boot on): every message and call arrives, at a cost
in battery that ntfy avoids. With one, out of sight for a minute (and in
no call), the node stops altogether; phones skip the Mainline DHT
(relays and n0's DNS find peers for them).

Notes: the bookmark on a message adds it (or its whole thread) to your
notes as a task, with an optional due date, or as a quote: plain Markdown
with an `irch://` link back, appended to a file in
[loroh](https://github.com/cab-ucf/siroh)'s vault (`LOROH_VAULT`, else
`~/notes`). A lab's ideas and projects live in a shared folder there
(`Lab/Ideas.md`, `Lab/Projects.md`), synced by siroh with everyone who
shares it, so loroh's task list tracks them beside your own. `irch://`
links open irch at that message. On Android, where loroh's vault is
private to it, the Markdown goes to the clipboard.

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
- Authors date their own ops, at most a day ahead. An op sealed under a
  replaced key counts only if dated before the replacement, or if its
  author was given the new key (a member who wrote offline): an old
  invite or a banned member under a new name gets nowhere. Inside its
  epoch, though, a key still writes: a demoted admin's backdated bans and
  renames stand.
- Calls: every caller sends to every other, so a handful of people, not
  a stage. QUIC encrypts them between the two devices (classical); the
  space's key does not. Invisible hides your name from presence, typing
  and the member list, not that your device is connected: members who
  watch the network can still tell, and a call you join shows you. A
  Mac needs Safari 26's WebKit (WebCodecs audio); Android cannot share
  its screen.
- A storage code is a password: whoever has it can make the node store
  anything. Members are trusted not to flood. Unaudited.

## Prior art

Quiet (Tor and OrbitDB), Keet (Hypercore, closed source), Briar, Cwtch,
SimpleX and Jami groups, Matrix's peer-to-peer experiments, and
[p2panda](https://p2panda.org), whose spaces and DCGKA group encryption
are the closest research. Calls take their shape from n0's
[iroh-live](https://github.com/n0-computer/iroh-live) (Media over QUIC
on iroh), without its native codec stack: the page has WebCodecs.
irch's niche: Discord's shape on a core small enough to read (`core/`,
about 2,500 lines), with keys that change on bans, storage that never
sees plaintext, calls with no server, and the same code in a terminal,
on desktops and on Android.

| | |
|---|---|
| `core/` | ops, crypto, fold, logs, sync, the node, the app's JSON API |
| `tui/` | `irch`: terminal client, `--serve`, `--headless` |
| `app/` | Tauri 2 shell; `ui/` is the page; `gen/android` the Android project |

GPL-3.0-or-later.
