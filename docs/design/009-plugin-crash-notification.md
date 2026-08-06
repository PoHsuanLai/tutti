# 009 — Telling a host its plugin died

Status: **proposed, not built.** Scope and open questions for replacing the
polled crash bool with a notification plus an honest status enum.

## The problem

A plugin crash is an **edge**: it happens once, on the bridge thread, between two
host calls. The only way a host learns about it today is
`PluginHandle::is_crashed() -> bool`, a **level** it must sample. So every
consumer runs a per-frame poll to discover something that already happened, and
learns about it up to a frame late.

The bool also cannot say *why*. At the detection site the real `BridgeError` is in
hand; by the time anyone polls, it is gone. `bevy-tutti` says so at the point
where it would use it:

> The engine discards the `BridgeError` that carried the real cause, so this is as
> specific as the host can be. Surfacing the true reason needs an engine change;
> until then, do not invent one.

```rust
let cause = "bridge reported the plugin as crashed".to_string();
```

That string is the whole diagnosis a user gets for a dead plugin.

## What is already there

The notification mechanism exists and is wired. `BridgeEvent` already carries
`LatencyChanged`, `TailChanged`, `ParameterChanged` and `Resync`; the bridge
thread fires them through a `ListenerSlot`, and `PluginHandle::on_invalidate`
delivers the structural half to the host as `PluginInvalidation::{Latency, Tail,
Io, Reloaded}`.

**Crash is simply not one of the variants.** Adding it is a new arm on an existing
channel, not a new mechanism.

## Verified before proposing

Three facts were checked at source, because each could have sunk the design.

### 1. All three crash sites are genuine crashes, not shutdowns

`mark_crashed` is called at exactly three places, all in
`ipc_client/audio/thread.rs`:

| site | condition |
|---|---|
| `ipc::connect` fails | never connected |
| handshake missing or version mismatch | never usable |
| `result.is_err()` in `pump` | stream died mid-session |

Ordinary teardown exits through `while lifecycle.is_running()` and calls
`request_shutdown()`, which stores to a **different atomic** and never touches
`crashed`. Crash and shutdown are already separate, so firing a notification at
these sites cannot produce a false positive on a normal close.

### 2. The listener is in scope at all three

`pump` already receives `&ListenerSlot` and calls `drain_unsolicited(channels,
listener)` on the line above the third site. No plumbing is needed to reach it.

### 3. `Failing` is **not** an artifact of polling, and must survive

This one refuted an earlier assumption in this design, so it is recorded rather
than quietly dropped.

`PluginBridge::is_crashed()` delegates to `self.audio.is_crashed()` — the *audio*
bridge's atomic. The control path (`parameter_value`, `save_state`, …) does not
set it. So a peer that answers a `GetParameter` with a well-formed reply of the
**wrong kind** leaves the flag clear while the call returns `None`, exactly as
`bevy-tutti` documents:

> `!is_crashed()` does not mean healthy. A peer that answers a `GetParameter` with
> a well-formed reply of the *wrong kind* leaves the flag clear while the call
> returns `None`.

That is a second failure mode, with no detection site to notify from. **The
callback removes the publish race; it does not remove `Failing`.** An earlier
draft of this proposal claimed the debounce existed *only* because of polling.
Half true, and the wrong half to build on.

## Design

### Two mechanisms, because there are two failure modes

| state | source | detection | debounce |
|---|---|---|---|
| `Dead` | new `BridgeEvent::Crashed` | edge, at the detection site | none — the event *is* the fact |
| `Failing` | consecutive control-call failures | polled | yes, and it stays |
| `Healthy` | neither | — | — |

This is wgpu's device-loss shape — callback *and* status query, not one or the
other — with the difference that the debounce for the second mode lives in the
library instead of being rebuilt per consumer.

Keeping the query matters: a pure callback loses the answer for anything that
starts mid-session, and for code that only needs to skip work rather than react.

### The pieces

1. **`BridgeEvent::Crashed { cause: BridgeError }`** — fired at each of the three
   sites, carrying the error already in hand.
2. **`PluginInvalidation::Crashed { cause }`** — the public arm, delivered through
   the existing `on_invalidate`. It belongs on `PluginInvalidation` rather than
   `PluginRefresh` by that enum's own rule: structural, re-plan the graph.
3. **`PluginStatus { Healthy, Failing { consecutive }, Dead { cause } }`** in
   `tutti-plugin` — moved from `bevy-tutti`, where it is host-agnostic and in the
   wrong crate.
4. **`PluginHandle::status() -> PluginStatus`**, with `is_crashed()` kept as
   `matches!(status(), Dead { .. })` so no caller breaks in the same change.

### The startup-ordering problem, and why the query is not optional

`PluginBridge::new` spawns the bridge thread; `set_listener` is called afterwards,
from `PluginClient::new`. **Two of the three crash sites — connect failure and
handshake failure — can therefore fire before any listener exists.** A pure
callback drops them silently, which is worse than the poll it replaced.

Two candidate fixes, and this is the main open decision:

- **(a) Latch the cause.** Store the `BridgeError` beside the `crashed` atomic;
  `status()` reports `Dead { cause }` whether or not anyone was listening. The
  callback becomes an optimisation for liveness, and the query stays
  authoritative. Simple, no ordering rule for callers to obey.
- **(b) Replay on install.** `set_listener` fires immediately if the bridge is
  already crashed. Callback-complete, but it means a listener can fire during
  installation, which is a surprising reentrancy for a caller to reason about.

**(a) is the recommendation.** It keeps one source of truth, and the startup
failures are exactly the ones a host most needs to see reliably — a plugin that
never connected is the common case for a bad install, not an exotic one.

### Explicitly not proposed

- **No typestate.** A crash is externally triggered with no call site to attach a
  transition to — the same reason wgpu makes device loss a callback while using
  types for its acquisition chain. See `008-plugin-lifecycle-strategies.md`.
- **No relaunch.** `PluginStatus::Dead`'s existing doc is right: *"the plugin is
  not coming back without a fresh load, because the engine offers no relaunch."*
  Recovery stays a replacement entity carrying `PluginHealth::snapshot()`.
- **No change to the audio path.** A crashed bridge already returns zeros. This is
  about telling the *control* side, not about what the audio thread does.

## Work

| step | crate | test |
|---|---|---|
| 1. `BridgeEvent::Crashed` + fire at the three sites + latch the cause | `tutti-plugin` | a hostile peer that drops the socket delivers exactly one event carrying the real error |
| 2. `PluginInvalidation::Crashed`, routed through `on_invalidate` | `tutti-plugin` | a listener installed before the crash receives it |
| 3. `PluginStatus` + `PluginHandle::status()`, `is_crashed()` kept | `tutti-plugin` | `status()` reports `Dead` for a crash that preceded listener install (the ordering case) |
| 4. `bevy-tutti` consumes the event; keep its `Failing` debounce for control-call failures | `bevy-tutti` | unwiring still happens; a wrong-kind reply still reaches `Dead` via the poll |

Steps 1–3 are additive — nothing is removed, so no consumer breaks mid-stack.
Step 4 is where `health.rs` shrinks.

`hostile_peer_tests.rs` already builds misbehaving peers and already polls for
`mark_crashed` (*"Polling rather than a single read because `mark_crashed` happens
on the bridge thread"*), so it is the natural home for steps 1–3 and its poll
becomes an assertion on the event instead.

## Mutation targets

Each new test must be shown to fail against a specific break:

- delete the `fire` at the `pump` site → step 1's test fails
- fire `Crashed` on the shutdown path too → a clean-close test must fail (this is
  the false-positive guard, and the reason fact 1 above was checked)
- drop the latch, keep only the callback → step 3's ordering test fails
- remove `Failing`, report `Dead` on the first control-call failure → the
  wrong-kind-reply case must fail, since that is the mode with no event

## Open questions

1. **Is `BridgeError` cloneable and `Send`** into the event? Not checked. If not,
   the event carries a string and the latch carries the error.
2. **One event or many?** A stream death also pushes `AudioResponse::Error` and
   drains with errors; whether the crash event should fire once at
   `mark_crashed` or per failed in-flight request needs a look at
   `drain_with_errors`. Once, at the flag, is the intent.
3. **Does anything outside this tree call `is_crashed()`?** It stays either way,
   but it changes whether `status()` can eventually replace it.
