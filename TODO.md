# TODO

See [PLAN.md](PLAN.md). irpush's own list: irpush's TODO.md.

## 1. Reachable from anywhere
- [x] Time relays again, so a device has a home relay at all
- [x] No relay tokens; one relay at a time, best first (storage node's
      irpush, iroh.cxn.center, n0), the next after a minute without it
- [x] Relay keys (`url#key`, from whoever runs the relay): no speed limit
      through it; a big download not direct says so
- [ ] Probe only on a network change, not every 20–26 s
- [x] Doctor (`irch --doctor [who]`, Network use, a call not getting
      through): our relay, each device and push server timed, direct or
      relayed; nothing rung
- [ ] Measure the direct-path rate (address discovery is back on)
- [ ] Close idle QUIC connections after ~30 s; Network use warns when an
      idle device holds one

## 2. Rings under a second
- [x] Measured (`ring_paths`): awake, the call said in the space wins
      (~12 ms vs push ~50 ms); asleep, push alone (~32 ms). No dial path
- [x] Ring before opening the sound devices
- [ ] On real networks: the doctor's numbers from real devices
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

## 6. Short invite and link codes
- [ ] Security review: entropy, one guess, TTL, rendezvous, crate audit
- [ ] PAKE over iroh, pkarr rendezvous, 60 s, single use
- [ ] The invite (or seed, to link) sealed to the joiner's X-Wing key
- [ ] Optional password: an Argon2id-keyed layer around that

## 8. Relays without the cloud
- [ ] A storage node with a public address serves a relay
- [ ] One button: a VPS provider's API token in, a relay out

## 7. UI
- [x] Storage Nodes: the switch toggles (not Rename)
- [x] Storage Nodes: Remove, optionally shut down our own
- [x] Settings icon: a cog
- [x] Name the device at first start; storage nodes `hostname-storage`
