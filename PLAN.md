# Plan: calls that always ring, phones that sleep

Goal: any device rings any other in under a second, on any relay, while an
idle phone spends under 1 % of its battery a day. Simple, opinionated, and
fault tolerant: one way that always works beats three that sometimes do.
Steps and status: [TODO.md](TODO.md). irpush's side: irpush's TODO.md.

## What broke

0.1.7–0.1.9 moved every device's iroh endpoint onto its storage node's
irpush relay. That relay admits only token holders, so a device without
the token cannot reach anyone homed there: a phone on iroh.cxn.center
cannot dial a desktop on irpush.cxn.center. The relay also caps each
client at 64 KB/s, sized for pushes, now carrying calls. Calls dial from
the lower id, so half the pairs never connect. Duplicate DMs make it worse:
the call button can ring a DM the other side never joined.

## The token

It was abuse control, not encryption: it kept strangers from using the
relay as a free relay (README: "not a secret"). Privacy comes from elsewhere:
ops are sealed end to end (XChaCha20-Poly1305, X-Wing rekeys), irpush frames
are X-Wing per frame, and the relay hop is post-quantum TLS
(`relay-pq.patch`). The only metadata a token hides is presence: an open
relay answers anyone `EndpointGone` for an absent id.

iroh-relay's access control is per connection and all or nothing, and
irpush's devices only ever talked to hosts that held the token, so nobody
needed to reach a token relay from outside, until irch homed there.

From now on the token is a relay option, off by default:
- **open** (default): anyone may connect. Long pings, rate limits, PQ TLS.
- **token**: as today, for an operator who wants strangers off entirely.
- later, if wanted, **home token**: anyone may send to devices homed there;
  only token holders may home (a patch to iroh-relay's send path).

Rule: **a device homes only on a relay that every sender can use.**

## One quiet socket per device, UDP only in use

UDP is the noise: NATs forget idle UDP in ~30 s, so every open QUIC path is
pinged every 5 s. NATs keep idle TCP 2 h+ (RFC 5382), so one TLS/TCP socket
to a long-ping relay is nearly silent, and the relay makes a NAT'd device
reachable from anywhere by its endpoint id.

| state | holds | wakes by |
|---|---|---|
| phone, background | irpush's one TCP socket | an irpush push from a host |
| any device, irch running, idle | iroh's relay TCP socket (long-ping relay), no QUIC connections | a dial through the relay |
| talking, syncing, calling | direct QUIC (UDP), closed ~30 s after use | |

The irpush relay is an iroh relay with long pings, so both irch's endpoint
and irpush's device can home on it: on a desktop that is one quiet socket
for all of irch (and, later, one shared endpoint for every iroh app, as
irpush already shares one socket per phone). A phone's irch process does
not live in the background, so there irpush's socket alone holds on.

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

## Short invite codes, and everything ephemeral

An invite today is a bearer code holding the space key, good until the
next rekey. Instead:
- `nameplate-word-word-word`, as sendmew: the nameplate finds the inviter
  (a short-lived pkarr record, as irch's addresses already are; no DHT),
  the full code is the password of a PAKE (SPAKE2, RFC 9382, or CPace).
- One online guess per code, then it dies; expires after 60 s (said on
  screen). The rendezvous learns a nameplate and an endpoint id for a
  minute, and cannot MITM the PAKE.
- Through the PAKE channel the inviter hands an invite sealed to the
  joiner's own key: nothing reusable is ever written down or typed.
- To check before shipping: code entropy against one online guess,
  denial by burning codes, what the rendezvous sees, the crate's audit
  status.

Ephemeral elsewhere: contact codes rotate, scheduled rekeys (README's
"Later"), push queues stay in memory for a day, idle connections close.

## Small things

- Storage Nodes: the switch opens Rename (the button sits in the switch's
  label, before its checkbox); add Remove (does not stop a remote node;
  optionally shuts down our own).
- Settings icon: a real cog, not a sun.
- Name a device at first start (hostname by default); storage nodes
  default to `hostname-storage`, `--name` at install.
- One DM per person in the list; calls and messages use the one both
  are in; empty duplicates the other never joined are left quietly.
