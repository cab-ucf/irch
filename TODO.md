# TODO

See [PLAN.md](PLAN.md). irpush's own list: irpush's TODO.md.

## 1. Reachable from anywhere
- [x] Home on an open relay; token relays stay in the map, after it, so
      devices homed there by 0.1.7–0.1.9 are still reached
- [ ] After irpush's relay opens: home on it (long pings) everywhere
- [ ] `irch --doctor [user]`: each device's home relay, reachable or not,
      direct or relayed, push routes, ring time
- [ ] Measure the direct-path rate with and without QUIC address
      discovery (off since "Probe less"); keep it off only if it costs more
- [ ] Close idle QUIC connections after ~30 s; Network use warns when an
      idle device holds one

## 2. Rings under a second
- [ ] Test matrix: which of dial, push, gossip arrives first, and how fast
      (callee foreground, background, killed; desktop; mixed relays)
- [ ] Keep what wins, delete what never does
- [ ] The ring carries the caller's address; a woken phone dials back
- [ ] DM calls: no "lower id dials"; the callee dials the caller

## 3. One DM per person
- [ ] One row per person, histories merged
- [ ] `dm` and calls use the DM both are in; empty duplicates left quietly

## 4. Delivery by device list
- [ ] Measure gossip's chatter in an idle open app
- [ ] Direct fan-out to member devices for DMs and small spaces
- [ ] Message one device (fan-out to one)

## 5. Push servers that form themselves
- [ ] A user's storage nodes share one irpush mesh
- [ ] Phones pair once, register with every host

## 6. Short invite codes
- [ ] Security review: entropy, one guess, TTL, rendezvous, crate audit
- [ ] PAKE over iroh, pkarr rendezvous, 60 s, single use
- [ ] The invite handed over sealed to the joiner

## 7. UI
- [ ] Storage Nodes: switch toggles (not Rename); Remove, optionally shut
      down our own
- [ ] Settings icon: a cog
- [ ] Name the device at first start; storage nodes `hostname-storage`
