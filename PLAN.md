# Plan: calls that always ring, phones that sleep

Goal: any device rings any other in under a second, on any relay, while an
idle phone spends under 1 % of its battery a day. Simple, opinionated, and
fault tolerant: one way that always works beats three that sometimes do.
Steps and status: [TODO.md](TODO.md). irpush's side: irpush's TODO.md.

## What broke

Since "Probe less", irch's endpoint ran neither https latency probes nor
QUIC address discovery, and iroh then never picks a home relay: a device
was reachable on its LAN or a public address, never through a relay.
0.1.7–0.1.9 also homed devices on irpush's relay, which admitted only
token holders. Calls dial from the lower id, and duplicate DMs can ring a
DM the other never joined.

## Relays: open, one at a time, best first

The token only kept strangers off a relay, and cut everyone else off from
whoever homed there. It is gone: relays are open, abuse bounded by
per-client rates (relayed traffic only; direct paths have no limit),
per-IP connection caps and idle timeouts. Privacy never rested on it:
ops are sealed end to end, irpush frames are X-Wing per frame, the relay
hop is post-quantum TLS. An open relay can tell anyone whether an id is
connected there, nothing more.

Relayed traffic is capped per client so one stranger cannot fill the
operator's bandwidth; direct paths are never limited. Whoever runs a relay
hands out keys (`make key`) that lift the cap: a lab's machines send
terabytes through their own relay when a direct path cannot form. A key
goes only to its relay. A big download that is not direct says so.

A device holds one relay at a time, best first: its storage node's irpush
relay, then irpush.cxn.center (both pinged hours apart; the default for a
storage node's phones too), then iroh.cxn.center, then n0's. A minute
without it, the next; every half hour, the best again if it answers.
iroh reaches a peer on whichever relay its address names, so devices on
different relays reach each other.

While irch runs, iroh times its relay over https every 20–26 s (its
fixed rate) and asks it our public address. That is the price of being
found; a phone's irch does not run in the background, so it costs an idle
phone nothing. Later: probe only on a network change (an iroh patch or
upstream option).

## One quiet socket per device, UDP only in use

UDP is the noise: NATs forget idle UDP in ~30 s, so every open QUIC path is
pinged every 5 s. NATs keep idle TCP 2 h+ (RFC 5382), so one TLS/TCP socket
to a long-ping relay is nearly silent, and the relay makes a NAT'd device
reachable from anywhere by its endpoint id.

| state | holds | wakes by |
|---|---|---|
| phone, background | irpush's one TCP socket, shared by every iroh app on the phone | an irpush push, shown at once |
| any device, irch running, idle | iroh's relay TCP socket (long-ping relay), no QUIC connections | a dial through the relay |
| talking, syncing, calling | direct QUIC (UDP), closed ~30 s after use | |

The irpush relay is an iroh relay with long pings, so both irch's endpoint
and irpush's device can home on it: on a desktop that is one quiet socket
for all of irch (and, later, one shared endpoint for every iroh app, as
irpush already shares one socket per phone). A phone's irch node does not
run in the background: irpush's socket holds on for it and for every
other iroh app on the phone. A push carries the sealed message or call
itself, so it is shown at once, with no node started and no connection
opened; the node starts only to answer.

## Waking and ringing

Measured (`ring_paths` in core/tests/net.rs, loopback, so each path's own
cost; four runs):

| callee | first to arrive | the other |
|---|---|---|
| awake | the call said in the space (gossip, a link already open), ~12 ms | push, ~50 ms |
| asleep | push through its storage node, ~32 ms | none |

A new link costs ~7 ms to a device and ~16 ms to a push server, so a
third way, dialing the callee to ring it, never beats a link already
open, and fails just when that link could not form. Two ways stay, both
sent at once (the caller cannot know which the callee is in): the call
said in the space, and a push. A dial is not added.

On a real network each costs round trips: the open link ~half of one; a
push ~2 (a new link to the push server, then the push server's already
open socket to the phone): ~100–300 ms at common latencies. The doctor
(`irch --doctor WHO`, or in a call that is not getting through) times
these on the real network, device by device.

Found on the way: `call()` opened the sound devices before ringing; it
rings first now.

## The design (agreed)

- Asleep, a phone holds one TCP socket to a relay, a keepalive every ~30
  min or longer, nothing else: irpush's, shared by every iroh app on the
  phone. irch's node is stopped; irpush wakes it. On iOS, irpush the same
  way, through APNs where a socket cannot stay.
- Awake, a device dials each member device of a space directly and
  closes a link ~30 s after it goes quiet (an open one is pinged every
  5 s). No mesh held open.
- A device asleep is woken by an irpush push through its storage node's
  relay; storage nodes keep what it missed. Every device gets pushes.
- So the iroh client needs no change: its own endpoint runs only while
  irch is in use. The relay needs only to not ping (irpush's patch).

## Delivery: users and their devices

Today each space is an iroh-gossip mesh (HyParView, Plumtree) plus range
sync; users' devices are known from signed ops. Gossip holds about five
neighbor connections open, each pinged every 5 s: a main source of chatter.

A space is a list of users, each with a list of devices, so for DMs and
small groups the simple thing is direct fan-out: dial each member device
(sleeping ones: push), send, close when idle; storage nodes are devices
too and catch everyone up. Gossip stays only if measurement shows big
spaces need it. A message to one device is fan-out to a list of one.

Phase 1 (done): every op is also dialed straight to each device of a
space up to 64 (members' and storage nodes'), one stream each, over links
closed after ~30 s quiet; the receiver takes it from devices that space
knows, as if gossiped (duplicates drop). Phase 2 (done): spaces up to 64 devices hold no gossip at all. Ops go
to who wrote there, its storage nodes, devices met there and our own
devices; a sync (either way) is how a device is first met, and presence
and "peers" come from direct links. Finding a device by id is DNS's
(n0's) on the default network, mDNS's on a LAN; custom relays have none
yet. Tests use one address book per process, as DNS. Next: measure, and
remove gossip's code if no space needs it.

## Push servers that form themselves

Every storage node is an irpush host. A phone's one irpush device joins
the mesh of each of its owner's storage nodes (irpush: one device, several
meshes) and registers with all of them; members ring every one at once,
so a ring gets through while any one is up. Only the owner's: another
person's host could wake every irpush app on the phone. Next: the storage
nodes share one mesh, so a ring queued on one that is up reaches a phone
whose link to it is down.

## Short invite and link codes, and everything ephemeral

Long codes are bearer secrets: an invite holds the space key until the
next rekey, a link code the seed itself.

Built (`core/src/code.rs`), simpler than a PAKE and with nothing new to
trust but Argon2id:
- A code is 9 characters of Crockford base32 (`k7m-x9q-p3a`, ~45 bits;
  any case, dashes or spaces; i/l read as 1, o as 0).
- Stretched with Argon2id (RustCrypto, OWASP's defaults), it gives a
  throwaway iroh identity and a proof. The giver answers as that identity
  for 60 s, found like any device: by id through n0's DNS, or tried on the
  relays both use.
- The taker proves the code and sends a fresh X-Wing key (ML-KEM-768 +
  X25519); the giver hands the long code (invite or link) sealed to it.
  One try: a wrong proof gets nothing and spends the code. Tested: hand
  over, spent after use, burnt by one wrong proof.
- What sees what: n0's DNS (or a relay) sees the throwaway id for a
  minute. To learn the code from it, one must stretch up to 2^45 guesses
  in that minute; at ~50 ms each that is ~55 000 years of one core. To
  impersonate the giver, one needs the code. A taker who guesses gets one
  try at 2^-45. Denial (burning a code by guessing at its id) is possible
  for whoever sees the id: the giver then makes another.
- The transport is iroh's QUIC (TLS 1.3, X25519); the payload inside is
  also sealed to X-Wing, so a recording is safe from a future quantum
  computer.
- Linking works the same way: the old device's short code carries the
  link code (the seed, who you are, the same on every device), to the new
  device's fresh key. Each device keeps its own device key.
- An optional password both type Argon2id-keys a second layer around the
  payload (salted by the code's proof), so a recording stays shut even if
  X-Wing fell. The taker may give it after the one try: the sealed payload
  waits in memory, opened there. The long codes themselves stay bearer codes; the
  short code only stops them being pasted around.

Ephemeral elsewhere: contact codes rotate, scheduled rekeys (README's
"Later"), push queues stay in memory for a day, idle connections close.

## Relays without the cloud, or one button

irch runs on its users' hardware. A storage node with a public address
(IPv6, or a forwarded port) can serve the relay itself; a NAT'd home
needs one public relay somewhere. For that, one button in the app: a VPS
provider's API token in, a relay out (cloud-init, a signed static binary,
an IP certificate or the provider's DNS), its address handed to the
user's storage nodes.

## Small things

- Storage Nodes: the switch opens Rename (the button sits in the switch's
  label, before its checkbox); add Remove (does not stop a remote node;
  optionally shuts down our own).
- Settings icon: a real cog, not a sun.
- Name a device at first start (hostname by default); storage nodes
  default to `hostname-storage`, `--name` at install.
- One DM per person in the list; calls and messages use the one both
  are in; empty duplicates the other never joined are left quietly.

## From iroh's code and issues (Nov 2026, iroh 1.4.1 = main)

- Fixed timers: client relay ping 15 s, net_report every 20–26 s, open
  paths pinged every 5 s (path keepalive clamped at 5 s, idle at 15 s).
  Open issues to make them configurable or quiet: #4590, #4589, #4651,
  #4598; none merged. Either side's relay ping resets the other's timer,
  so irpush's server patch alone does not quiet an iroh client: phones
  idle on irpush's own client, which learns its keepalive.
- Nothing in iroh wakes or pushes: irpush fills a real gap.
- Taken now: direct links close 10 s after their last use (iroh-gossip
  0.102 does the same); segmentation offload off on Android (#4647);
  custom relays: a device is looked for on them (no DNS needed).
- Later: iroh_util ConnectionPool if it replaces our pool; stream
  affinity (#4654); iroh-blobs 0.104 toward 1.0; a self-hosted
  iroh-dns-server for custom-relay networks if dialing on our relays is
  not enough; iroh-live (calls) once past preview.
