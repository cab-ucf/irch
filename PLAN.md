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

A device holds one relay at a time, best first: its storage node's irpush
relay (pings hours apart), then iroh.cxn.center, then n0's. A minute
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

Two ways, by whether the callee's process runs:
1. **Dial** its device by endpoint id: the relay carries the first packets
   at once, then QUIC moves to a direct path.
2. **Push** through its irpush host when it sleeps (phone in background,
   iPhone): the ring carries the sealed call op and the caller's address;
   the phone shows the call from its phonebook and dials straight back.

Which one is needed is measured, not guessed: a test matrix (callee
foreground, background, killed; desktop; relays mixed) records which path
arrives first and how fast. Expectation: dial wins whenever the process
runs, push is needed only when it does not, and gossip is never needed for
a ring. Whatever never wins is deleted.

## Delivery: users and their devices

Today each space is an iroh-gossip mesh (HyParView, Plumtree) plus range
sync; users' devices are known from signed ops. Gossip holds about five
neighbor connections open, each pinged every 5 s: a main source of chatter.

A space is a list of users, each with a list of devices, so for DMs and
small groups the simple thing is direct fan-out: dial each member device
(sleeping ones: push), send, close when idle; storage nodes are devices
too and catch everyone up. Gossip stays only if measurement shows big
spaces need it. A message to one device is fan-out to a list of one.

## Push servers that form themselves

Every storage node is an irpush host. A user's storage nodes join one
irpush mesh as the owner adds them; a phone pairs once and registers with
all of them, so rings get through while any one is up.

## Short invite and link codes, and everything ephemeral

An invite today is a bearer code holding the space key, good until the
next rekey; a link code holds the seed itself. Instead, both become:
- `nameplate-word-word-word`, as sendmew: the nameplate finds the inviter
  (a short-lived pkarr record, as irch's addresses already are; no DHT),
  the full code is the password of a PAKE (SPAKE2, RFC 9382, or CPace).
- One online guess per code, then it dies; expires after 60 s (said on
  screen). The rendezvous learns a nameplate and an endpoint id for a
  minute, and cannot MITM the PAKE.
- Through the PAKE channel the inviter hands the invite sealed to the
  joiner's own X-Wing key (ML-KEM-768 + X25519).
- Optionally a password both sides type: its Argon2id hash keys a second,
  symmetric layer around that. A recorded PAKE is open to a future quantum
  computer (SPAKE2 is elliptic-curve); X-Wing covers that; the password
  covers X-Wing failing too.
- Linking a device is the same: a new device makes its own device key and
  a one-off X-Wing key at first start and shows a code; the old device
  hands over the seed (who you are, the same on every device) sealed to
  it. Each device keeps its own device key (its endpoint id). Per-device
  identity keys, so one lost phone can be cut off alone, stay in README's
  "Later".
- To check before shipping: code entropy against one online guess,
  denial by burning codes, what the rendezvous sees, the crate's audit
  status.

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
