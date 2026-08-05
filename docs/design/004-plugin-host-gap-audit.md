# Plugin host gap audit — CLAP, VST2, AU (+ one VST3 residue)

Tracking doc for the gap audit run on 2026-08-04, covering the three format
hosts that had never had one. VST3's equivalent is issue #54.

**A gap audit is not a conformance sweep.** CLAP and VST2 both had conformance
sweeps already (PR #69 and the `fix(clap-host)` run). Those asked "does what
this host does conform?". This asks "what does this host *not* do?" — and the
answers are different.

## What is deliberately NOT in scope

The capability-level gaps are already inventoried and must not be re-filed:

- `crates/tutti/crates/plugin/tutti-plugin/README.md` — the per-format
  capability table, which separates `○` (our loader doesn't implement it) from
  `✕` (the format can't express it).
- `tutti_plugin_types::features::probed` — the same claim as per-format masks.
- `tutti-plugin-server/src/loaders/au.rs:330-338` — the AU loader documents its
  own gaps inline.

So "AU has no transport", "VST2 has no note expression" and friends are known,
written down, and not findings. Everything below is either behaviour that is
wired but wrong, a doc comment the spec contradicts, or a capability the table
claims we have.

## Status legend

`TODO` unstarted · `WIP` in progress · `DONE` landed · `HELD` blocked/deferred

---

## A. Cross-format

### A-0 · Provenance sum types leak format disagreement to the consumer · DONE

The rule this crate should hold: **a format host may model its format's
disagreement; the plugin host must normalize it.** A caller asking "what is this
parameter's range" should not have to know that VST2 sometimes declines
`effGetParameterProperties`.

Three types currently spell provenance into the *shape* a consumer matches on:

| Type | Where | Variants |
|---|---|---|
| `ParamRange` | `tutti-plugin-types/src/parameters.rs:31` | `Normalized { default }` / `Plain { min, max, default }` |
| `ParamSteps` | `parameters.rs:165` | `Unknown` / `Continuous` / `Toggle` / `Enumerated(n)` |
| `EditorPresence` | `descriptor.rs:69` | `Unknown` / `Absent` / `Present` |

**The good news, and why this is small.** Every match site on `ParamRange` and
`ParamSteps` outside their own module is a *producer* — a format host or a
loader building the value (`clap-host/instance/params.rs:458`,
`vst2-host/parameters.rs:155-175`, `loaders/vst3.rs:476-496`). There are
currently **zero consumer-side matches**. The accessors that make the enum
unnecessary already exist: `ParamRange::{bounds, default_value, to_plain,
to_normalized}`, `ParamSteps::count`, `EditorPresence::{measured, is_present}`.

**Landed.** The work was smaller than a redesign, because the accessors already
existed and nothing consumed the variants:

- `ParameterInfo` gained `bounds()`, `default_value()`, `step_count()` beside
  the `to_plain`/`to_normalized`/`flag` it already forwarded, so the whole
  parameter can be read without touching `ParamRange` or `ParamSteps`.
- `PluginDescriptor::has_editor()` forwards `EditorPresence::is_present()`.
- `step_count()` collapses `Continuous` and `Unknown` to `None` — both mean "do
  not draw a stepped control", the only decision a consumer makes from it. The
  distinction stays on `steps` for a coverage report.
- The module doc now states the rule: the variants are for the host that must
  *produce* them, not the reading path.

The variants were deliberately **not** made `pub(crate)`. The format hosts are
separate crates and legitimately construct these, and `probed`-style coverage
reporting is a real second consumer. What changed is that reading through them
is no longer necessary — verified: the only remaining reference outside
`tutti-plugin-types`, the format hosts, and the loaders is
`EditorPresence::measured(..)` in `vst2_in_process/loader.rs:69`, which is a
constructor.

**Not in scope, and why.** These enums look similar but are *not* provenance —
each variant is a genuinely different thing, so normalizing would delete
meaning, not friction:

- `ParamAddress` (68 sites) — opaque id vs positional index. Two different
  addressing models; collapsing them is the bug `4831c6115` fixed.
- `PluginClass` / `AuComponentType` / `Vst2Category` / `Vst3PlugType` /
  `ClapFeature` — deliberately verbatim per format, with `PluginRole::role()`
  *already* the normalized view. This is the pattern A-0 wants, done right:
  raw kept, normalized derived. See [[plugin-role-normalization]].
- `PluginTail` (33 sites) — `Finite`/`Unbounded`/`Unknown` are different
  quantities, not provenance about one.
- `RenderMode` (added by A-1) — two modes, no provenance variant. The
  "did the plugin honour it" half is `Features::RENDER_MODE`, deliberately kept
  out of the enum.

### A-1 · `ProcessContext` cannot express realtime vs offline · DONE

`tutti-plugin-types/src/process.rs` — `ProcessContext` carries `midi_events`,
`param_changes`, `note_expression`, `transport`, `expressive`. There is **no
realtime/offline field**, so `PluginAudio::process` cannot be told a bounce is
running, for any format.

The format halves are already built and are reachable only from tests. (A-2
later found this table understated the VST2 gap in one direction and overstated
it in another: the host *did* answer `4` while offline, but no plugin could ask
— the plugin-side `Host` trait had no `get_process_level`. Both ends now exist.)

| Format | Call | Callers |
|---|---|---|
| CLAP | `set_render_mode` → `CLAP_RENDER_OFFLINE` (`instance/ports.rs:272`) | `loaders/clap.rs:1524` — past the `#[cfg(test)]` at 584 |
| AU | `set_offline_render` (`instance.rs:1295`, `offline.rs`) | crate tests only |
| VST2 | `set_offline_render` (`instance.rs:390`) → `get_process_level` answers `4` | crate tests only |
| VST3 | `ProcessMode` (`host/instance.rs:394`) | issue #54 finding 1 |

Consequence: an offline bounce renders in realtime mode in every format. A
look-ahead limiter, an oversampling EQ, or a "higher quality when offline"
plugin silently produces its realtime result in a render the user asked to be
exact. No error is reported.

**This is the one item that must be designed before the per-format items** —
C-3, D-3 and E-2 are all downstream of it.

**Landed** (`RenderMode { Realtime, Offline }` on `PluginAudio`, wire message
`SetRenderMode`, protocol v13, all four format edges). What that does *not*
include is a caller, and investigating why turned the three downstream items
into a different piece of work — see A-2.

Also note two things about issue #54:

- Its finding 1 is **mis-scoped** — it reads as a VST3 defect, but the shared
  vocabulary is what cannot express the distinction.
- Its finding 1 is **stale** — `kOffline`/`kPrefetch` are no longer absent;
  `ProcessMode` exists with a spec-correct Realtime↔Prefetch live toggle
  honouring the exception at `ivstaudioprocessor.h:139-143`. Re-verify the rest
  of #54 before acting on any of it.

### A-2 · No bounce driver can reach a hosted plugin · DONE

Found while looking for A-1's caller. C-3 / D-3 / E-2 were written as "wire the
export path to set the mode", which assumes a seam that does not exist.

- `tutti-export` has **zero** plugin references — "plugin" appears nowhere in
  `crates/core/tutti-export/src/` outside one unrelated doc sentence. It renders
  a `Net`; it has never heard of a plugin.
- `bevy-tutti`'s `export/run.rs` queries entities by `AudioNode`, the generic
  graph node, so a hosted plugin is indistinguishable from any other node there.
- `bevy_tutti::plugin_host` and `bevy_tutti::export` never meet.

So there is no loop over hosted plugins that a bounce could hook. The pieces
that now exist:

- `PluginEmitter { handle: PluginHandle }` is a real component — the queryable
  seam a driver would use.
- `PluginHandle::set_render_mode` and the `HostRenderMode` capability were added
  alongside A-1 so that handle can carry the mode from the control thread. It
  mirrors `HostAutomationState`: `Some(backend)` for subprocess, `None` for
  in-process VST2 (whose node answers
  `audioMasterGetCurrentProcessLevel` from its own `HostState`, reachable
  through `Plugin::set_render_mode`).

What remains is a driver in `bevy-tutti` that, at the start of an offline
render, walks `Query<&PluginEmitter>` and sets `RenderMode::Offline`, then
restores `Realtime` afterwards. That is an export-architecture change, not a
plugin-host one, which is why it is its own item rather than three per-format
ones. **C-3, D-3 and E-2 are therefore closed as duplicates of this** — each
format's half is built and reachable; only the caller is missing.

Open question for whoever takes it: a bounce that fails partway must still
restore `Realtime`, or the session keeps running every plugin in offline mode.
That argues for a guard type rather than two bare calls.

**Fixed — `bevy_tutti::plugin_host::render_mode`.** A system announces
`Offline` while any `ExportInFlight` exists and `Realtime` whenever none does,
registered only when `ExportPlugin` is present (`is_plugin_added`, not a feature
flag) and ordered after both export systems.

Three things changed the shape from what this entry proposed:

- **The guard cannot live on the export entity.** `ExportInFlight::cancel` drops
  the task, and despawning the request does the same implicitly; neither fires
  `ExportDone` and neither is seen by `poll_exports`. A component-based guard
  would be destroyed with the entity, leaving every plugin stuck `Offline` for
  the session. So the mode is a resource, restored by observing that *no* export
  is in flight rather than by being told one ended — one rule covering
  completion, failure, cancellation and despawn, since all four have the same
  observable.
- **The `PluginEmitter` route did not reach in-process VST2.** This entry
  assumed `Query<&PluginEmitter>` was sufficient. It was not:
  `PluginHandle::from_backend` left `render_mode: None` for that path on the
  grounds that "the in-process VST2 node owns the render mode itself", reachable
  through `Plugin::set_render_mode` — but `load.rs` calls `plugin.into_parts()`,
  which *consumes* the `Plugin`. Only the handle survives into the ECS, so no
  ECS-side driver could ever have reached those plugins. Fixed by implementing
  `HostRenderMode` for `InProcessVst2Backend` (it already holds the same
  `Arc<Mutex<Vst2Instance>>` the node renders) and making `render_mode` an
  explicit `from_backend` parameter beside `editor`.
- **No VST2 plugin could ask what mode it was in.** VST2 carries this on
  `audioMasterGetCurrentProcessLevel`, which the host answered
  (`vst-tutti/src/interfaces.rs`) — but the plugin-side `Host` trait had no
  `get_process_level` to send it with. Both halves are needed for the query to
  exist; the plugin-side accessor was added, and the reference probe now asks
  once per render.

Tests: seven in `render_mode.rs` for the decision (which mode a frame resolves
to, and who gets told), four in `tutti-plugin`'s
`tests/vst2_in_process_render_mode.rs` driving real FFI for the delivery.
Mutation-checked at six points, including reverting the `render_mode` slot to
`None` — the original bug — which fails three of the four delivery tests.

One coverage boundary worth stating: the seven decision tests observe the
decision, not the wire. Deleting the `set_render_mode` call in the loop body
leaves all seven green, which is why the delivery half lives one crate down
against a plugin that reads the level back.

---

### A-3 · A runtime latency change never re-plans PDC, in any format · DONE

Split out of E-5, where it was found; it is not an AU gap.

The whole chain from a plugin's latency change to `PluginClient::latency()` is
built and works for VST3, CLAP and (as of E-5) AU: a format callback → an
`AsyncEvent` → a `BridgeMessage` → the latency atomic → `PluginInvalidation::
Latency` fired on the invalidate sink. It stops one step short.
`PluginHandle::on_invalidate` (`host/handles/control_handle.rs:360`) is the
subscription point, and **nothing in the repo calls it** — not `bevy-tutti`, not
any `dawai-*` crate, not an example.

`host/node/mod.rs:432-437` states the consequence in the code: updating what
`AudioUnit::latency()` reports "does **not** re-run PDC on its own — a graph edit
(`Net::commit()`) is required". `envelope.rs:159-160` repeats it. So a plugin
that changes latency at runtime updates its own node's figure while every
compensation delay in the graph keeps the old one, and the chain stays silently
inert until a graph edit happens to occur for an unrelated reason.

One change serves all four formats, which is why it is here and not inside a
per-format finding: a subscriber in `bevy-tutti` that turns
`PluginInvalidation::Latency` into a `GraphDirty` / recompensate. Doing it inside
E-5 would have made an AU fix look like it fixed VST3 and CLAP too.

**Fixed — as a poll, not a subscriber.** `bevy_tutti::plugin_host::latency`
raises `GraphDirty` when a plugin's reported latency differs from what the last
compensation pass was planned against.

Two findings changed the shape from what this entry proposed:

- **`on_invalidate` is the wrong hook.** It is documented as emitted *only by
  the out-of-process backend*, so subscribing would have fixed subprocess-hosted
  plugins and silently not in-process ones. The latency atomic is written on
  both paths, so polling it covers both. (A callback also cannot touch the
  `World` — it would need a channel plus a drain system, a second route to a
  value the graph already owns.) This is the same shape as `plugin_health_poll`,
  which polls the crash flag rather than subscribing.
- **No graph edit is needed.** The entry inherited "a graph edit
  (`Net::commit()`) is required" from `host/node/mod.rs:432-437`. True of the
  engine, not of the Bevy layer: `compensate_graph` is gated on the `GraphDirty`
  *flag*, not on a rewire. Setting the flag is the whole job. The engine-side
  comment is accurate for a direct `Net` consumer and was left as-is.

`CompensatedLatency` is not a copy of the plugin's latency — that would be a
second owner needing invalidation it could not see. It records what the graph
last aligned *for*, which is different state with a different owner; the
difference between the two is exactly the staleness condition.

Coverage limit, stated in the module rather than implied: `PluginClient::new`
launches a subprocess, so no unit test can put a live plugin in a graph. The
decision rule is split into `needs_recompensation` and pinned (unchanged /
changed both directions / never-compensated, including the `None` vs
`Some(Samples(0))` conflation). The flag actually being raised for a live plugin
needs an integration test loading a real binary, and no such harness exists in
`bevy-tutti` yet.

---

## B. VST3 residue (not in issue #54)

### B-1 · `set_sample_rate` calls `setupProcessing` on an active instance · DONE

`formats/tutti-vst3-host/src/host/instance.rs:388`.

`Vst3Instance::load` ends with `set_active(true)` (`:352`). `set_sample_rate` is
a method on that active instance and calls `apply_process_setup()` →
`setupProcessing()` in place.

Spec (`pluginterfaces/vst/ivstaudioprocessor.h:328-330`): *"Called in disable
state (setActive not called with true) before setProcessing is called and
processing will begin."*

Three aggravations:

1. The failure is discarded — `let _ = self.apply_process_setup();`
2. The doc says *"Must be called only when not inside `process`"* — the wrong
   guard. The constraint is **not active**, not *not processing*.
3. Machinery of the right *shape* already exists on the same type —
   `restart_bus_configuration` (`:886`; this doc originally cited it under its
   old name `reactivate_for_latency`).

**Landed, but not by reusing that method.** It is not a generic bracket: inside
its deactivate/activate cycle it also runs `negotiate_bus_arrangements()` and
`reconcile_bus_counts()`, because it serves `kIoChanged`/`kLatencyChanged` —
the plugin has just announced its bus layout changed. A rate change announces
nothing about layout, and re-running `setBusArrangements` there would let a
`kResultFalse` re-resolve the audio scratch from a read-back arrangement, so a
rate change could reshape channel scratch behind an unrelated call.

A separate private `reconfigure(rate)` reuses the four existing primitives
(`stop_processing`, `set_active`, `apply_process_setup`, `read_latency_samples`)
instead, so no VST3 call sequence is written twice. It rolls back to the previous
rate on refusal; a *failed rollback* propagates as its own error rather than
folding into the refusal, because "inactive and unrecoverable" is a worse fact
than "still running at the old rate".

`set_sample_rate` now returns `Result<()>` instead of `&mut Self`. Verified no
code used the chaining — but the crate README documented it, in an example that
also called a `set_block_size` VST3 never had. Doc examples are not
compiler-checked, so that had gone stale unnoticed; corrected in the same change.

Witnessed by a new `kModeSetupWhileActive` probe mode that counts
`setupProcessing` calls arriving while active. `host-checker.vst3` cannot see
this — it validates the `ProcessData` it is handed, not the lifecycle preceding
the call — and neither can the host, since `setupProcessing` returns `kResultOk`
either way. Only the plugin observes the ordering. Mutation-tested: the pre-fix
body yields `11001` against an expected `11000`, i.e. exactly one violation.

Note for anyone touching that probe: `kModeStepCount` is a stepped-parameter
divisor, so adding a mode changes *every* mode's normalized value. Its Rust
mirror `MODE_STEPS` moved 9.0 → 10.0 in the same change. A disagreement between
them does not fail to compile — it silently selects the wrong renderer.

Every other format brackets this correctly, which is what makes it a defect
rather than a house style:

- CLAP `lifecycle.rs:295` → `reconfigure()` = deactivate → activate, with rollback
- VST2 `instance.rs:313` → `suspend_for_reconfigure` / `restore_after_reconfigure`
- AU `instance.rs:1634` → `uninitialize()` → re-apply → re-init, with rollback

Reachable live: IPC `SetSampleRate` → `session.rs:172` → `loaders/vst3.rs:600` →
`instance.rs:388`. So a device rate change on a live VST3 has the plugin
re-sizing buffers while active.

---

## C. CLAP

### C-1 · `gui.closed(was_destroyed=true)` skips the mandated `destroy()` · DONE

`src/host/callbacks.rs:276-287` latches `already_destroyed`;
`src/instance/polling.rs:383-392` reads the latch and returns early, skipping
both `gui.hide` and `gui.destroy`.

Spec (`ext/gui.h:241-242`): *"If was_destroyed is true, then the host **must**
call clap_plugin_gui->destroy() to acknowledge the gui destruction."*

The code comment at `polling.rs:380-385` and `host/state.rs:58-62` both assert
the opposite — that calling `destroy` again would be a double-destroy. The
spec's own lifecycle (`gui.h:20-34`) pairs `destroy()` (step 14) with
`create()` (step 2), i.e. with the **gui resources**, not with the window.
`was_destroyed` reports the window is gone; `destroy()` releases what `create()`
allocated.

Consequence: every plugin that closes its own floating window leaks its GUI
resources for the instance's lifetime, and `flags.gui_created` is cleared so no
later `close_editor` can recover.

Fixing this means correcting the two doc comments as well — they are the reason
the code looks deliberate.

### C-2 · `clap_plugin->reset()` is never called · DONE

No call site in `src/` (verified: greps hit only `drop_in_place` and
`load_preset`).

Spec (`plugin.h:84-90`): *"Clears all buffers, performs a full reset of the
processing state (filters, oscillators, envelopes, lfo, …) and kills all voices
… clap_process.steady_time may jump backward. [audio-thread & active]"*

Aggravating: the host **does** reset `steady_time` to 0 at
`instance/lifecycle.rs:264`. The spec permits that backward jump only because
`reset()` was called. So we take the licence without paying for it.

Consequence: on locate, loop wrap or any discontinuity, stale reverb tails,
ringing filters and hung voices bleed across the jump.

**Landed** as `ClapActive::reset()` — on `ClapActive` rather than `ClapLoaded`
because the annotation is `[audio-thread & active]` and that type's existence
*is* the active half. (`flush_params` sits on `ClapLoaded` because CLAP gives it
a two-branch contract, `[active ? audio-thread : main-thread]`; `reset` has no
inactive contract.) It takes the `AudioThreadClaim` the way `stop_processing`
does rather than a `debug_assert`, because the claim serializes against an
in-flight `process` and an assertion does not. It also zeroes
`scratch.steady_time`, which is what earns the backward jump the spec licences.

**Follow-up, deliberately not done here (C-2a):** the *subprocess* path still
cannot reset a CLAP plugin's DSP state. `PluginAudio` has no reset hook
(`format_host.rs:38-50` — only `process` and `set_sample_rate`), and
`HostMessage::Reset` is handled at `session.rs:183` as `Ok(Reaction::None)`.
That handler's comment is correct about what it covers — host pipeline
bookkeeping, dropped in-flight blocks, sequence numbers — but it is now
incomplete: none of that clears the plugin's filters and voices. Wiring it needs
a `PluginAudio::reset` default-no-op plus a `session.rs` dispatch, which is a
cross-format trait decision rather than part of this fix.

### C-3 · Offline render unreachable · DONE — superseded by A-2

`set_render_mode` (`instance/ports.rs:272`) is correct and has only test
callers. `has_hard_realtime_requirement` (`:292`) likewise — so the host also
cannot know when it must *not* go offline (hardware-proxy plugins).

### C-4 · `set_scale()` is called on macOS, which the spec forbids · DONE

`src/instance/polling.rs:117-121` called `set_scale` unconditionally, with no
platform branch.

Spec (`ext/gui.h:56-57`): `CLAP_WINDOW_API_COCOA` — *"uses logical size, don't
call clap_plugin_gui->set_scale()"*. Same for `UIKIT` (`:59-60`). Only `win32`
and `x11` use physical size and want it. And `ext/gui.h:141`: *"Should not be
used if the windowing api relies upon logical pixels."*

Masked at the time: the scale passed is a hardcoded `1.0`, the identity, so the
bug was latent until real DPI is wired — at which point a Retina editor
double-applies the backing-scale factor and opens 2× oversized. That is why the
fix is pinned on whether the *call happened*, not on the size it produced: an
assertion about the size passes against the bug today and only starts failing
once DPI lands, which is the moment the test exists to protect.

Gated on the window **api string** (`api_uses_logical_pixels`), not on
`cfg!(target_os)`: the api is what the spec keys on, the host already resolves
it through `platform_window_handle`, and an unrecognized string defaults to the
physical-pixel path — both logical-pixel apis are named, so anything else is
physical or future, and 1.0 on an api that wanted none is the identity.

The doc at `polling.rs:79-80` elevated the ordering to a design rule
(*"set_scale must land before get_size"*). Corrected: that holds only on a
physical-pixel api, where the scale is an input to the geometry the plugin then
reports. On cocoa/uikit the call does not happen at all, so its position in the
sequence is vacuous rather than load-bearing.

### C-5 · No `_COMPAT` extension id is ever queried · DONE

`src/instance/extensions.rs:152-222` queries exactly one id per extension; no
`_COMPAT` constant appears anywhere in `src/`. All ten exist in clap-sys 0.5.0
and were available.

Each header states the compat id is 100% compatible and is the SDK's
instruction that a host accept both spellings (e.g. `ext/surround.h:26-28`,
`ext/track-info.h:13-15`, `ext/preset-load.h:7-9`).

Consequence: a plugin built against a pre-1.2 SDK answers `clap.surround.draft/4`
but not `clap.surround/4`, so the host concludes it has no surround map, no
track-info, no preset loading, no context menu. Fails **as silent absence** —
indistinguishable from a plugin that genuinely lacks the feature. Widest blast
radius of anything in this doc, since much of the shipping CLAP corpus predates
1.2.

**Fixed.** `ExtensionCache::get_either` asks the stable id, then the `_COMPAT`
one, at all ten sites that have a compat spelling. The two names are the same
interface at the same version — `clap.surround/4` and `clap.surround.draft/4`
are both `/4` — so this is one interface with two names, not a shim with a
conversion in it.

Stable first, and pinned by a test: a plugin implementing *both* must bind to
its current interface, and a draft-first host would silently prefer the older
spelling for the whole session.

Tested with a fake `get_extension` that answers exactly one id, which is what
lets the draft-only case actually fail — the three tests cover draft-only,
stable-only (asserting the draft is never asked), and neither.

### C-6 · `audio-ports.rescan` flags discarded; `is_rescan_flag_supported` lies · DONE

`src/host/callbacks.rs:301-305` throws the flags away and sets one bool;
`:294-299` returns `true` for every flag without inspecting it.

Spec (`ext/audio-ports.h:84-99`): six flags, five annotated `[!active]`. Only
`RESCAN_NAMES` is safe while active.

Two consequences: (a) a consumer cannot tell a cosmetic name change from a
channel-count change needing deactivate→re-enumerate→re-activate, so
`PortLayout` goes stale against the plugin's real buffer expectations; (b) the
unconditional `true` tells the plugin the host handles every rescan kind, so it
takes the aggressive path instead of a conservative fallback.

Contrast `params.rescan` next door (`callbacks.rs:174-184`), which correctly
accumulates and decodes its flags into `ParamRescan`. The audio-ports side never
got the same treatment.

**Fixed**, by giving it that treatment. `AudioPortsRescan` mirrors `ParamRescan`
— six decoded flags plus `needs_deactivate()`, which is phrased as "anything but
`NAMES`" so a flag added by a later CLAP revision counts as unsafe-while-active
until someone reads its annotation.

`is_rescan_flag_supported` now answers from the mask of flags the host actually
decodes. An unknown bit gets `false`, and a known bit paired with an unknown one
also gets `false` — the host cannot honour the half it does not decode.

`poll_audio_ports_changed() -> bool` was **replaced** rather than kept beside
the new accessor: the poll drains, so two readers over one signal would clear it
for whichever asked second.

Four mutations, and two of them survived the first round of tests — worth
recording, because both were in the half of the finding that is easy to consider
covered by the decoder tests:

- reverting `is_rescan_flag_supported` to unconditional `true` passed everything
  until a test drove the callback directly;
- and so did dropping the flags in `host_audio_ports_rescan`, because every test
  entered at `from_flags` and none went through the callback a plugin calls.

Both are now pinned by tests that build a `HostState`, point a `clap_host` at
it, and call the callback the way a plugin does.

### C-7 · `preferred_dialect` fabricates `Midi2` · DONE

`src/instance/ports.rs:124-132` — the `else` arm returns `NoteDialect::Midi2`,
but `CLAP_NOTE_DIALECT_MIDI2` (`ext/note-ports.h:29`) is never tested for. So
`preferred_dialect == 0` (no preference) and any future dialect both report
MIDI 2.0.

That is the one dialect `host_note_ports_supported_dialects`
(`callbacks.rs:312-314`) tells the plugin we do *not* support. A caller routing
on it sends UMP to a plugin that will not parse it; notes are dropped silently.

This is the "manufacture a host value" antipattern already recorded in
[[param-info-absent-vs-reported]].

### C-8 · `thread_pool` returns `true` and does nothing · DONE

`src/host/callbacks.rs:713-726` — `host_thread_pool_request_exec` returns
`true`, then stores the task count in an atomic nothing reads.

`ext/thread-pool.h`'s contract: a `true` return means the host will call
`exec()` for indices `0..num_tasks`. A plugin splitting voice rendering across
the pool therefore produces **silence for every voice past the first**, having
been told the work completed. Returning `false` would make it work inline.

Most dangerous of the fabricated-success set. `request_show`/`request_hide`
(`callbacks.rs:268-274`) have the same shape but a far milder consequence.

Fixed by rejecting, not by building a pool. `thread_pool_exec`
(`polling.rs:801-812`) is not an abandoned scheduler — it carries the same
"Speculative — gated behind `clap-extras`" comment as every other method in
that block, which are mechanical wrappers over one plugin vtable entry each.
Nothing ever called it, and no worker threads, queue or barrier were ever
written. The header (`:35-39`) also notes a pool "may break hard real-time
rules" and that a host under hard-real-time pressure may decline to offer the
interface at all, so declining is a position the extension anticipates rather
than a stub. The dead `thread_pool_pending` atomic went with it.

### C-9 · Only descriptor index 0 is loadable · DONE

`src/instance/descriptor.rs:128` calls `get_desc(factory_ptr, 0)`;
`get_plugin_count` is called at `:112` only to check non-zero, and its value is
discarded. A bundle shipping a synth plus companion FX exposes only the first,
and there is no API to select by index or id.

**Fixed.** `all_descriptors` enumerates the factory; `select_descriptor` picks
by **id**, not index — `create_plugin` already takes an id string, and ids are
already the catalog/dedup key, so nothing new had to be invented. New surface:
`probe_all`, `load_plugin`, `load_selected`; `probe` and `load` keep their
existing meaning of "the bundle's first plugin".

An unknown id is an error listing what the bundle holds, not a fallback to the
first: falling back would load a *different plugin* than the one asked for, so
a session restoring "the compressor" would silently come back with the synth.

**The fixture was the deliverable here.** The mutation "enumerate only index 0"
survived a full green suite, because the probe shipped a single descriptor —
with one plugin, a host that enumerates and one that hard-codes index 0 are
indistinguishable, and both pass. `tutti-clap-test-plugin` now ships **two**
descriptors, and `clap_multi_plugin_bundle.rs` covers the path end to end
through the real FFI. The mutation now fails two tests.

Not attempted: a cross-format sub-plugin API. VST3 has the same shape (a factory
of classes) and no selection either, but inventing shared vocabulary for it is a
design change, not a gap fix.

### C-10 · `clap_version` compatibility is never checked · DONE

Neither `clap_plugin_entry.clap_version` (`entry.h:61`) nor
`clap_plugin_descriptor.clap_version` (`plugin.h:13`) was read;
`clap_version_is_compatible()` (`version.h:38-40`) was unused. A 0.x-era or
future-major `.clap` was dlopened and read under 1.2 layout assumptions — a
struct-layout misread, not a clean error.

Both sites are checked, in `descriptor.rs::load_descriptor`, which `probe` and
`load_with_library` share. They are two separate claims: the entry's is the
DSO's and must be read *before* `init` (a 0.x entry gives no promise its `init`
pointer sits at the 1.2 offset), the descriptor's is one plugin's, and the spec
never says one implies the other. They report different `LoadStage`s.

The SDK predicate supplies only the floor. `clap_version_is_compatible` is
`major >= 1`, written from the plugin's side — it asks "is this host new
enough?", a question with no ceiling. A host asks the mirror question, so
`major <= CLAP_VERSION_MAJOR` is added: the SDK function alone accepts a 2.0
descriptor and hands it to `descriptor_to_info`, which walks seven `*const
c_char` at 1.x offsets. Minor and revision are unbounded in both directions —
CLAP adds within a major by appending, so a later 1.x reads as a prefix and an
earlier one simply lacks extensions `get_extension` already returns null for.
Bounding them would reject most of the shipping corpus, as a clean error that
looked correct.

### C-11 · Two `[main-thread]` methods lack `assert_main_thread()` · DONE

`polling.rs:540` (`poll_timers`) and `:635` (`on_main_thread`). Ten sibling
methods do assert. These are the two most likely to be driven from a UI tick on
a different thread than `HostState::new()` ran on.

No existing test drove either off-thread, so nothing was asserting the gap.

### C-12 · Floating-window GUI mode unimplemented · DONE (host layer) / HELD (ECS layer)

`embed_editor_sequence` (`polling.rs:88-163`) hardcodes `is_floating = false`;
`get_preferred_api`, `set_transient`, `suggest_title` are absent.

`has_editor()` therefore queries `is_api_supported(api, false)` only, so a
floating-only plugin reports **no editor**.

**One claim in the original entry was wrong.** It said embedding is unsupported
on Wayland "so every CLAP plugin is editor-less there by our reckoning". The
spec half is right (`ext/gui.h:68` — "embed is currently not supported, use
floating windows"), but `platform_window_handle` has **no Wayland arm**: on
Linux it returns `CLAP_WINDOW_API_X11` unconditionally (`polling.rs:51-58`), so
this host never asks the Wayland question and works under XWayland. The real
exposure was narrower — floating-only plugins on *any* platform, not all plugins
on one.

**Fixed at the host layer.** `ClapLoaded` gained:

- `open_floating_editor(transient, title)` — the `ext/gui.h:20-27` sequence:
  `is_api_supported(floating)` → `create(floating)` → `set_transient` →
  `suggest_title` → `show`. Returns no size, deliberately: the plugin owns the
  window, so a size would imply the host should lay it out.
- `has_floating_editor()` — the other half of the editor question.
  `has_editor()` keeps its embedded-only meaning, now documented as such.
- `prefers_floating()` — `get_preferred_api`'s `is_floating` flag. `None`
  (no preference stated) stays distinct from `Some(false)`.

No geometry call is made on a floating window: `set_scale`, `set_parent`,
`can_resize`, `adjust_size` and `set_size` are all `[main-thread & !floating]`
in the header. `close_editor` is unchanged and serves both modes — `hide` and
`destroy` are the two calls not `!floating`-gated.

`Features::EDITOR` is now the **union** of the two questions (`editor_bits` in
`loaders/clap.rs`), which is the user-visible half: the bit means "this plugin
has a UI", and keying it on the embedded answer alone is what made a
floating-only plugin look editor-less. `Features::EDITOR_RESIZE` stays keyed on
the embedded answer, since every resize entry point is `!floating`.

Tests: 7 new in `clap_gui_lifecycle.rs` (28 total, all green) plus 3 on
`editor_bits`. Mutation-checked at 5 points — wrong `is_floating` on the query,
wrong flag on `create`, dropped `suggest_title`, and the union reverted in each
direction — all killed. The `editor_bits` extraction exists *because* the first
attempt at the union mutation survived: nothing tested the loader's capability
bits, and reaching them through the loader needs a live plugin behind a dlopen.

**The ECS layer is still held**, and this is the honest boundary: nothing in
`bevy-tutti` calls the new path. Reaching it means a defaulted
`open_floating_editor` through four traits — `HostEditor` → `PluginBridge` →
`PluginEditor` → `ClapGuiInstance` — three of which are cross-format, so VST3,
AU and VST2 would all carry a CLAP-only concept that answers "unsupported"
forever. Then `plugin_editor_show_hide` spawns a Bevy `Window` unconditionally
and five systems key on `PluginEditorOpen::editor_window`; a floating editor has
no such entity, so the component needs an ownership split.

Held rather than TODO because it cannot be tested here: the reference probe
never opens a window, and no floating-only CLAP plugin is installed to try it
against. Writing untestable plumbing through four shared traits, for a plugin
shape nobody has produced, is worse than the gap. Revisit when such a plugin
turns up — the host layer it would need is built and pinned.

---

## D. VST2

### D-1 · f64 is negotiated and then silently downcast to f32 · DONE

`formats/tutti-vst2-host/src/process.rs:124-137` — `process_block`
unconditionally builds `vst::buffer::AudioBuffer<f32>`. Both `process_f32`
(`:42`) and `process_f64` (`:70`) route through it, so the f64 path is
`prepare_f64` (cast down) → f32 render → `copy_out_f64` (cast back).

The vendor exposes the correct entry point — `vendor/vst-tutti/src/host.rs:1205`
`process_f64` → `processReplacingF64` — and it has **zero callers** anywhere in
the tree.

And it is reachable, not theoretical:

- `instance.rs:208` sets `supports_f64` from `PluginFlags::CAN_DOUBLE_REPLACING`
- `loaders/vst2.rs:59` turns that into `Features::F64_AUDIO`
- `plugin.rs:171-177` negotiates `Float64` when that bit is set

So a 64-bit master chain round-trips through f32 at every VST2 node while the
capability report says otherwise.

Two doc defects ride along: `scratch.rs:9` calls this *"VST2's f32-only
constraint"* — false, the constraint is this crate's — and
`effSetProcessPrecision` (opcode 77) is never sent, so a plugin that switches
internal precision on it stays wherever it defaulted.

### D-2 · `reset()` and `set_sample_rate()` run suspend/resume on the audio thread · DONE

`tutti-plugin/src/format/vst2_in_process/audio_unit.rs:256-263` (f32) and
`:403-407` (f64) call `instance.set_sample_rate(...)` from `AudioUnit::reset()`.

`Vst2Instance::set_sample_rate` (`instance.rs:313`) dispatches
`effStopProcess`(71) → `effMainsChanged(0)`(12) → `effSetSampleRate`(10) →
`effMainsChanged(1)`(12) → `effStartProcess`(72). `effMainsChanged` is where
plugins allocate and free rate-dependent buffers — this crate's own comment at
`instance.rs:91-96` says exactly that. These are main-thread-only opcodes;
`reset()` is an RT call.

The comment at `audio_unit.rs:258-260` ("best-effort no-op trigger") is wrong
twice: it is not a no-op, and it is not a reset. Contrast the out-of-process
client (`host/node/audio_unit.rs:19-22`), which posts a bounded `reset_rt()`.

`assert_main_thread()` is asserted on the load and editor paths only, so nothing
catches this even in debug.

**Landed, and the two halves get different answers** — the title names both
`reset()` and `set_sample_rate()`, and they are not the same problem.

`reset()` now does nothing. **VST 2.4 has no DSP-reset opcode** (verified across
the whole `OpCode` enum: the nearest, `StartProcess`/`StopProcess`, signal a
processing interruption rather than a state clear, and are legal only while
resumed). So there is no RT-safe call to make, and fundsp's own trait default is
an empty body. The suspend/resume cycle moves to a named
`Vst2Instance::reset_processing_state()`, whose doc is honest that a plugin is
obliged to clear nothing on those edges.

`set_sample_rate()` defers instead of dropping. The rate genuinely must reach the
plugin, so deleting the call would trade an RT violation for a permanent silent
rate mismatch. The audio thread parks it in a shared `AtomicU64` — one `Relaxed`
store, no lock, no allocation — and `editor_idle` drains it on the main thread
through the existing bracket. That is the deferral shape the out-of-process
client already had.

This does **not** contradict C-2's CLAP `reset()`, which does call the plugin: CLAP
annotates `reset` `[audio-thread & active]`, VST2 annotates the equivalent
opcodes main-thread-only *and* they allocate. Each puts the operation where its
own spec permits.

`assert_main_thread()` added to `Vst2Instance::set_sample_rate` — on the
dispatching function rather than the call sites, so a future caller inherits it.

Witnessed by wiring the VST2 reference probe into `tutti-plugin` for the first
time (the machinery existed only in `tutti-vst2-host`). The probe counts
`effMainsChanged` per direction, so what crossed the AEffect seam is read
directly rather than inferred from a host-side flag. Every negative assertion is
paired with a positive that moves the same counter — including one proving the
deferred rate *does* arrive, so "nothing dispatched" cannot be satisfied by
silently discarding it.

### D-3 · No offline render mode · DONE — superseded by A-2

`host.rs:159-161` hardcodes `get_process_level` to `2`
(`kVstProcessLevelRealtime`), test-pinned at `:224-229`. `ProcessLevel::Offline
= 4` exists in the vendor (`api.rs:556`) and appears in no `src/`.

Compounding: `effSetTotalSampleToProcess`(73) never sent, and
`MidiEventFlags::REALTIME_EVENT` is hardcoded on every outbound event
(`midi.rs:93`), so during a bounce every event claims to be live-played.

### D-4 · Latency is read before `effOpen` and never refreshed · DONE

`vendor/vst-tutti/src/host.rs:638` reads `initial_delay` inside
`PluginInstance::new` — which runs **before** `init()` / `set_sample_rate` /
`resume()` (`instance.rs:159-162`). `instance.rs:207` reads it once into
`PluginInfo::latency_samples`; nothing re-reads `AEffect::initialDelay` again.

Plugins routinely set `initialDelay` during `effOpen` / `effSetSampleRate` /
`effMainsChanged`, which is why the SDK ties latency changes to
`audioMasterIOChanged` — and `host.rs:186-188` answers that `false`, with a
comment that is right about I/O and wrong about latency.

Consequence: PDC is wrong for exactly the plugins that have latency
(linear-phase EQ, look-ahead limiters, convolution reverb), and a plugin that
changes latency on a rate change is never recompensated.

**Fixed.** `PluginInstance::read_initial_delay` reads the field off the live
`AEffect` instead of the `Info` snapshot, and the host builds
`latency_samples` from it after the init sequence. `Vst2Instance::latency()`
exposes the same live read, because VST2 has no latency-changed callback at all
— `audioMasterIOChanged` is about I/O configuration and a plugin may change
`initialDelay` on a rate change without sending anything, so the host has to
re-ask after `set_sample_rate` / `set_block_size`.

**The fixture defect is the lesson here, and it is the [[vacuous-conditional-tests]]
shape in a new disguise.** The probe gained a `set_late_latency` switch so it
declares only from `effSetSampleRate` onwards — without it no fixture can tell a
host reading the stale snapshot from one that re-reads, since both see the same
number. But the switch set a process-global latch the probe never cleared, so
after the *first* load in a binary every later `PluginInstance::new` snapshot
already contained the late figure, and the buggy and fixed hosts became
identical again.

The mutation therefore passed — **in one test order and failed in another**,
which would have shipped as an intermittent false green rather than an obvious
one. Clearing the latch inside the switch fixed it; the mutation now fails in
both orderings. Worth remembering that a shared-image probe needs its state
reset *per load*, not per test.

### D-5 · `audioMasterUpdateDisplay`(42) and `audioMasterCurrentId`(2) are unroutable · DONE

`vendor/vst-tutti/src/interfaces.rs:385-447` — `host_dispatch` handles 19 of 40
live opcodes; everything else falls to `_ => trace!()` returning 0. Verified: no
`UpdateDisplay` or `CurrentId` arm exists.

Both are **wired but unreachable**, which is worse than merely absent:

- `Host::update_display()` is declared (`vendor/host.rs:283`) with no arm
  calling it. A plugin that changes preset from its own GUI and fires this gets
  0, and our parameter list goes stale.
- `Host::get_plugin_id()` is declared (`vendor/host.rs:208`) **and overridden**
  in our host to return `'DAWI'` (`formats/.../host.rs:108`) — also with no arm.
  A shell plugin (Waves; `Vst2Category::Shell` is a variant we map at
  `instance.rs:53`) calling `audioMasterCurrentId` during `VSTPluginMain` gets 0
  and loads its default sub-plugin. The override is dead code that reads as
  support.

Also worth one line: `audioMasterGetLanguage`(38) returning 0 is out of range —
`HostLanguage` is 1-based — so a plugin indexing a string table by it reads
slot 0. The other unhandled opcodes (`VendorSpecific`, `GetDirectory`,
file-selector, offline family) return an honest "declined".

**Fixed** — all three arms added to `host_dispatch`.

`UpdateDisplay` latches an `AtomicBool` the host drains through
`Vst2Instance::take_display_stale`, rather than re-reading inline: the opcode
arrives on whatever thread the plugin's editor runs on, and a synchronous
re-read would dispatch opcodes from it. A latch also matches the signal —
payload-free and idempotent, so ten preset changes between polls need one
re-read.

`CurrentId` returns the `get_plugin_id` override that was already there;
`GetLanguage` returns `English` (1) rather than the fall-through's out-of-range
0.

Only `UpdateDisplay` is test-covered, and the module says so. `CurrentId` is
asked from inside `VSTPluginMain` — before the host has an instance to observe
through — so witnessing it needs a probe that *is* a shell plugin, a different
fixture rather than a switch on this one. `GetLanguage` is never asked by the
probe, and what a plugin does with the answer is not observable host-side.

### D-6 · Subprocess VST2 editors are never idled · DONE (as a deletion)

`tutti-plugin-server/src/loaders/vst2.rs` implements `open_editor` (`:303`) and
`close_editor` (`:326`) but **not** `editor_idle`, so it inherits the no-op
default (`tutti-plugin-types/src/format_host.rs:139`). An editor opened through
that path never repaints and never processes input.

The in-process path is fine — `control_backend.rs:94-105` → `editor.rs:68-74` →
`effEditIdle`(19), driven every frame by `bevy-tutti/src/plugin_host/editor.rs:130`.

Same class as VST3's unpumped run loop (issue #54 finding 2). Decide whether the
subprocess path should implement `editor_idle` or stop implementing
`PluginEditorHost` at all.

**Re-investigated — this finding is mis-scoped, and the real gap is wider.**
Three facts, all verified on the current branch:

1. **The `PluginAudio`-side `editor_idle` has no caller for *any* format.**
   `grep` for it across `tutti-plugin-server/src/` returns nothing. The VST2
   loader is not the exception; it is the rule. VST3/CLAP/AU loaders do not
   implement it either, so all four inherit the same no-op.
2. **A *different*, working `editor_idle` exists on the control surface.**
   `PluginHandle::editor_idle` (`control_handle.rs:244`) →
   `HostEditor::editor_idle` (`capabilities.rs:84`), implemented by the
   composite backend (`composite.rs:363`) and the in-process VST2 backend
   (`control_backend.rs:99`), with per-format GUI impls at `format/gui/{vst3,
   clap,au}.rs`. `bevy-tutti/src/plugin_host/editor.rs:130` drives *that* one
   every frame. Editors are pumped — through the host-side GUI instance, not
   through the loader.
3. **The subprocess VST2 editor path is unreachable from the public API when
   the `vst2` feature is on**: `Plugin::open` (`host/plugin.rs:143-146`) routes
   every `.vst` to the in-process client before the subprocess branch. It is
   reachable only in a build without that feature — where `tutti-vst2-host` was
   not compiled at all, so there is no in-process host to prefer.

So `PluginAudio::editor_idle` is a **vestigial trait method**: a default no-op
with no implementors in the server and no callers anywhere. The honest fix is to
delete it, not to implement it — implementing it would add a second editor-pump
path beside the working one, which is the "one write path per thing" rule this
codebase already holds.

Retitled work: **remove `PluginAudio::editor_idle`** and the doc comment that
says "Only the in-process VST2 host needs this" (`format_host.rs:139`), which is
what made this look like a VST2-specific gap. Check first whether any
out-of-tree consumer could implement it — this is a library.

**Resolved as a deletion.** `PluginEditorHost::editor_idle` is gone, and the
trait doc now says where idle ticking actually happens. Re-verified at the point
of removal: a repo-wide grep for `editor_idle` returns two disjoint sets — the
host-side surface (`capabilities.rs:84`, `gui/mod.rs:36`, four format impls, and
the `bevy-tutti` per-frame system) and this one declaration site. No implementor
and no caller.

The out-of-tree question the entry asked to check first: removing a defaulted
method is breaking for an external implementor that overrode it, but such an
implementor was writing a body nothing in the pipeline would ever call, so the
break surfaces a bug rather than causing one.

### D-7 · `effEditGetRect` result partly discarded, and the `Rect` leaks · DONE

`editor.rs:36-49` uses width/height only; `position()` is never called, so the
plugin's requested origin is dropped. The vendor leaks on both sides:
`interfaces.rs:190-195` does `Box::into_raw(...)` with a literal
`// TODO: free memory`, and `host.rs:406` does `Some(unsafe { *rect })` with
`// TODO: Who owns rect?`. One `Rect` leaks per editor open, minimum.

**The leak is fixed; the dropped origin is not a defect.** They turned out to be
two different things, only one of them a bug.

`interfaces.rs` now writes into a thread-local `Cell<Rect>` reused on every
call. The lifetime constraint is what forced the leak in the first place:
`effEditGetRect`'s `Rect**` has no companion "free this" opcode and the host
cannot know the plugin's allocator, so whatever it points at must outlive the
call — `Box::into_raw` satisfies that and leaks one `Rect` per call, and hosts
call it repeatedly (before opening, and on every resize). Thread-local rather
than a `static`, since a host may drive editors for several instances; the
opcode is main-thread, so the reader is the writer's thread.

The origin is **deliberately** unread. `left`/`top` are where the plugin would
like its window; this host embeds the view into a `parent` it owns, so the
parent decides placement and there is nothing to act on. That would matter for
a floating editor, which this path does not offer — now said in a comment at
the read site rather than left looking like an omission.

`host.rs:406`'s `// TODO: Who owns rect?` is the *reading* side of the same
question and is answered by the above: the plugin owns it, and the host must
copy rather than free. Left as-is; the comment is the finding, not a leak.

Pointer identity is the testable form — a fresh allocation gives a different
address, a reused buffer gives the same one. The `vst-tutti` test plugin gained
a minimal editor, without which the whole arm is unreachable from any test.

### D-8 · `effCanBeAutomated` is surfaced but never called · DONE

`parameters.rs:188-189` ships `ParamFlags::empty()` with `known` empty. The doc
at `:123-126` says the vendored crate "does not surface" `effCanBeAutomated` —
**inaccurate**: `vendor/host.rs:1371` implements `can_be_automated`. Mild
consequence (empty `known` correctly signals unprobed), but the stated
justification is wrong and will mislead.

**Fixed.** `can_be_automated` is on `PluginParameters`, which the host already
holds as `Arc<dyn PluginParameters>` via `SendParams` — nothing stood in the
way. Probed per parameter (the opcode takes an index), and `AUTOMATABLE` now
ships in the `known` mask. The other flag bits have no VST2 opcode and stay
out of it.

Test-fixture limit, stated in the test rather than papered over: the probe
answers `in_range(index)` and `parameter_list` enumerates only in-range
indices, so every listed parameter answers `true`. The test pins "probed and
marked known" — the part that regressed — not "a `false` answer is carried
through", which no available input can witness.

### D-9 · Preset/program support absent despite full vendor coverage · HELD

`probed::VST2` omits both preset bits, yet the vendor implements
`change_preset`(2), `get_preset_num`(3), `set_preset_name`(4),
`get_preset_name`(29), and `numPrograms` is read into `Info::presets`. Also
unsent: `effBeginSetProgram`(67)/`effEndSetProgram`(68), which suppress
parameter-change storms during a switch.

Held: defensible as scope. But the README table should say `○` (we didn't) and
not read as `✕` (can't).

### D-10 · Smaller opcode gaps · HELD

`effSetBypass`(44) never sent, so host bypass must be a hard mute (there is no
`Features::BYPASS` bit at all). `effGetEffectName`(45) never sent — we use
`GetProductName`(48), a different string. `effGetNumMidiInputChannels`(78) /
`OutputChannels`(79) never sent, so `emits_midi` (`instance.rs:195`) has a dead
`false` term and a plugin answering `Maybe` to `sendVstMidiEvent` is classified
as MIDI-silent, dropping its output. `effSetSpeakerArrangement`(42) never sent.

---

## E. AU

### E-1 · AUv3-only units are unreachable, and the flag that says so is discarded · DONE (reported, not routed)

`src/component.rs:180` reads `componentFlags` into `comp_desc`;
`AuComponentInfo` (`:106-123`) drops the field. `src/handle.rs:44-45` then always
calls `AudioComponentInstanceNew`.

`AudioComponent.h:198-201` — `kAudioComponentFlag_RequiresAsyncInstantiation`;
`:500-502` says it *"must be used to instantiate any component with
[that flag] set"*.

Measured on this machine (138 components): 5 set the flag, and all 5 return
**-10863** from `AudioComponentInstanceNew`. `canLoadInProcess = 0` for all 138,
so `kAudioComponentInstantiation_LoadInProcess` is not an escape.

The residue is specific: we discard the one flag needed to *detect* and report
the case, so the user gets a bare "cannot do in current context"
(`src/error.rs:240`). Related: `kAudioUnitProperty_RequestViewController` (56,
the v3 editor path) is absent from `src/`, so even a successfully instantiated
v3 unit has no editor route.

**Fixed as far as this host can go.** `AuComponentInfo` now carries
`componentFlags` with `requires_async_instantiation()` / `is_v3()` accessors,
and `AuHandle::new` refuses a flagged component *before* calling
`AudioComponentInstanceNew`, with a named `AuError::RequiresAsyncInstantiation`.

**Not routed** — implementing `AudioComponentInstantiate` is a different piece
of work: it is asynchronous with a completion handler on an arbitrary thread,
and the header warns that blocking the main thread waiting for it deadlocks. So
this closes the *reporting* half of the finding (a caller can now tell "this
host cannot load that" from "that failed"), and leaves loading v3-with-view
units open.

The corpus reproduces the audit's measurement exactly: **138 components, 5 v3,
5 async-only** — so the refusal test exercises all five rather than taking an
empty-set early return.

### E-2 · Offline bounce renders in realtime · DONE — superseded by A-2

`src/offline.rs` implements `set_offline_render` / `set_render_quality`
correctly; nothing calls them. Structurally blocked by A-1.

### E-3 · The render timestamp is a free-running counter `reset()` does not reset · DONE

`src/instance.rs:1969` stamps each block from `self.scratch.advance(num_frames)`;
`src/buffer.rs:157-164` returns then increments. `sample_position` is initialised
to `0.0` at `buffer.rs:131` and written nowhere else — no reset, no seek.

`AuInstance::reset()` (`instance.rs:326-332`) calls `AudioUnitReset`, which
flushes the AU's internal state, but the next block still carries a monotonic
`mSampleTime` unrelated to where the playhead went — despite the method's own
doc saying a host "must call this on every discontinuity".

It also diverges from the transport clock: `src/transport.rs:258` publishes
`info.position.samples` while the render stamp is an independent counter, so a
plugin reading both gets two answers for "where are we". `offline.rs:326,376,485`
duplicates the pattern in `PushScratch`.

**Landed.** `RenderScratch::reset_position` sets the cursor to zero, and
`AuInstance::reset` calls it — **only after `AudioUnitReset` returned `noErr`**.
A refused flush leaves the AU holding its history, and restarting the clock
beside it would produce the one state neither branch describes.

**Zero, not a settable playhead**, and the header settles it. `AudioUnitRender`'s
own doc says `inTimeStamp` is what lets a unit "determine without doubt that this
the same render operation" — a per-instance render clock, a continuity signal.
The project timeline has a *different* channel: `outCurrentSampleInTimeLine` on
the transport callbacks, which an AU asks for separately and which
`TransportState::set_transport` already publishes. Writing a playhead into
`mSampleTime` too would answer "where are we" twice, in two places, with no bit
anywhere telling the AU which clock it got. The divergence the finding notes is
therefore not two answers to one question; it is two questions, and only one of
them was being answered wrong.

CLAP draws the same line and this repo already implements it there:
`clap_process::steady_time` is documented as a counter that "may be specific to
this plugin instance and have no relation to what other plugin instances may
receive", and `tutti-clap-host`'s `reset` zeroes it (`instance/lifecycle.rs:222`).

**`PushScratch` gets its own `reset_position` rather than being folded in.** The
two cursors have different owners: the pull cursor lives inside the instance, so
`reset` can reach it; the push one is host-owned — constructed by the host and
lent per render — and `reset` never sees it. Giving `reset` an optional scratch
would apply a discontinuity to whichever session was passed and leave the others
running, which is worse than the caller making two calls. Pinned by
`an_instance_reset_leaves_the_push_clock_alone`.

Two things the fixture needed, both in `tests/support/probe_au.rs`:

- **The probe now records the `mSampleTime` it is handed**, on every behaviour,
  through a vendor-private property. No Apple unit reports the stamp it received,
  so the only way to assert what the *host sent* is a component that records it.
- **The probe now implements `kAudioUnitProcessSelect`.** Six corpus effects
  implement the push selector and none reports its timestamp, so the push clock
  had no fixture that could observe it at all.

### E-4 · `verify_block_size` is not on the load path · DONE

`stream.rs:266` exists precisely because *"a successful set is not proof the
value stuck — an AU that clamps to its own maximum returns `noErr` while keeping
a smaller figure… the render is admitted and the AU writes past buffers it sized
for fewer frames."*

Its only call site is `instance.rs:1732`, inside `set_block_size`. The
constructor `with_layout` (`instance.rs:1806-1815`) calls `config.apply(&handle)?`
and stores the result **without** verifying — and that is the path every load
takes (`loaders/au.rs:265` → `AuHostInstance::new`).

So the exact hazard the function was written to prevent is unguarded where it
matters. Memory-safety consequence, cheapest fix in this doc.

**Landed.** One line in `with_layout`, the tail both constructors funnel through:
`config.verify_block_size(&handle)?;`. A refusal is an `Err` rather than a silent
adoption of the accepted figure, matching how `apply` already treats a rejected
sample rate.

Two things worth recording:

- **No unit on this machine clamps.** `au_channel_config.rs:539` already says so
  in its own caveat, so a corpus-driven test would have been vacuous
  ([[vacuous-conditional-tests]]). The guard is pinned by a `ClampsBlockSize`
  probe that genuinely drives the branch, plus a positive control on the same
  probe so the assertion cannot pass by everything failing.
- **The probe harness was itself an accidental clamping AU**: it returned a
  hardcoded `4096u32` for `MaximumFramesPerSlice` while discarding writes. Once
  the load path verifies, that made *every* probe test fail to construct — so
  fixing the probe to store and report honestly was a precondition for the fix
  being testable at all.

### E-5 · Latency, tail and parameter list are read once and cached forever · DONE (scoped)

`loaders/au.rs:288` (latency), `:303-314` (tail) and `:374` (param ranges) all
capture at load. Both latency and tail are dynamic `Float64` seconds properties.

`listener.rs` has a complete, working `watch_property` mechanism — and **no
production code constructs an `AuParameterListener`**. Only
`tests/au_api_surface.rs` does, and it watches
`K_AUDIO_UNIT_PROPERTY_LATENCY` specifically (`:329`), proving the path works
and is unused.

Consequence: a plugin that changes latency on a mode switch (linear-phase EQ,
oversampling toggle) leaves PDC compensating the load-time figure permanently.
`loaders/au.rs:426` already anticipates the parameter-list half in a comment
while having no mechanism to refresh.

**Landed, up to a line drawn deliberately.** The AU loader now installs one
`AuParameterListener` per instance, watching `kAudioUnitProperty_Latency`,
`_TailTime` and `_ParameterList`. A notification arrives on a GCD queue thread
and raises one of three `AtomicBool`s; `AuInstance::poll_changes`, drained
between blocks by `Plugin::poll_async_events`, consumes each flag with a `swap`,
re-reads the changed property, refreshes the cached `LoadedPlugin`, and emits an
`AsyncEvent`.

**Where the line is.** This wires AU into an existing, already-consumed path —
it builds no new plumbing. `AsyncEvent::{LatencyChanged, TailChanged,
ParamTitlesChanged}` already cross IPC as `BridgeMessage`s, already reach
`PluginClient`'s latency atomic, and already fire `PluginInvalidation::Latency`.
VST3 and CLAP have been feeding that path; AU's `poll_async_events` arm was a
`_ => {}` fallthrough. So this is not a change nobody consumes, and it is not
half-wiring — it is the missing arm.

**What is out of scope, and why it is not this finding's debt.** The invalidation
does not currently cause a PDC re-plan for *any* format: `PluginHandle::
on_invalidate` has no subscriber anywhere in the repo, which
`host/node/mod.rs:432-437` states outright. Fixing that is one change for all
four formats, and doing it inside an AU finding would hide it. Recorded as its
own item rather than folded in.

**Two things the fixture needed**, both in `tests/support/probe_au.rs`:

- **The probe stores property listeners instead of discarding them.**
  `probe_add_listener` returned `noErr` and dropped the proc, which made every
  property-notification assertion against a probe vacuous by construction:
  registration always succeeded, nothing ever arrived, and a host watching the
  *wrong* property was indistinguishable from one watching the right one.
- **Three vendor-private writable properties** move the probe's latency, tail and
  parameter count and post the corresponding public notification, value first.
  Nothing installed on this machine changes any of the three on request, so
  without them the "host notices" half has no fixture at all.

The split across crates is deliberate: `tutti-au-host`'s `au_property_watch.rs`
owns *"the AU told us"* (a real notification raises the flag, an unrelated
property does not, a dropped listener stops receiving);
`tutti-plugin-server`'s `plugin.rs` owns *"the host passed it on"* (the flag
becomes an `AsyncEvent`, and `loaded()` and the event agree).

### E-6 · Cocoa editor: hardcoded preferred size, and a leak on the error path · DONE (size); leak REFUTED

`editor/cocoa.rs:97-101` hardcodes `NSSize { 800.0, 600.0 }` as
`inPreferredSize` (`AUCocoaUIView.h:47-48`), so every AU editor opens at 800×600
regardless of host window; `AuEditor::open` has no size parameter to thread one
through.

Same path: `cocoa.rs:87-94` allocates the factory and returns `Err` on the null
check at `:89` **without** the `release` the success path does at `:123`.

Teardown ordering itself is correct (`editor/mod.rs:87-96`) — no gap there.

**Size fixed; the leak claim does not survive re-reading the code.**

`AuEditor::open` now takes a `preferred: EditorSize` threaded to
`uiViewForAudioUnit:withSize:`. It is a *hint* — `AUCocoaUIView.h:47-48` calls
it `inPreferredSize` — so the caller still reads the real frame back rather than
assuming the request was honoured.

The leak is **refuted**. Two paths were named and neither leaks:
`instantiate_factory`'s null check follows a failed `init`, and ObjC convention
is that a failing `init` releases the receiver, so there is nothing left to
release. `make_view` sends `release` *before* its null check, so the error path
releases too. The line numbers in the finding predate a layout that has since
changed.

Both real callers pass 800×600 because `PluginEditorHost::open_editor` carries
only a parent handle — there is no size at that layer to forward. Making one
available is a change to that trait's signature, i.e. a separate piece of work;
the constant is now a stated default at a call site rather than buried in the
Cocoa code.

### E-7 · SysEx is dropped; `MusicDeviceSysEx` is absent · DONE

Explicitly **not** a deprecation finding: `MusicDeviceMIDIEvent`
(`MusicDevice.h:212-218`) carries no `API_DEPRECATED`, so our use of it is
legitimate.

The gap is `instance.rs:511`'s `_ => continue`, which drops every message with
no 3-byte legacy form. `MusicDeviceSysEx` (`MusicDevice.h:237-241`,
`CA_REALTIME_API`, not deprecated) is absent from `src/` entirely, so no AU
instrument can receive a patch dump or vendor-specific message.

The MIDI-2.0 downscale on the same path is a *recorded* deferral
(`identity.rs:51-61` explains why `kAudioUnitProperty_HostMIDIProtocol` is not
negotiated), not an oversight — noted only because it costs per-note controllers
and 32-bit velocity on v3 instruments.

**Fixed.** `send_midi` reassembles SysEx7 across UMP packets (a packet carries
at most 6 bytes, so anything longer arrives START/CONTINUE.../END) and sends
complete messages through `MusicDeviceSysEx`. An orphaned `CONTINUE` — this host
joining a stream mid-message — is dropped rather than handed over as a fragment
with no header.

Reassembly state is per-call, not carried across blocks: a run left open at a
block boundary has no defined resumption point in this delivery model, and a
stale prefix would corrupt the next message.

Two things the tests found that are worth keeping:

- **`DLSMusicDevice` traps (SIGTRAP) inside `MusicDeviceSysEx`** and never
  returns, taking the process down. `AUSampler` accepts the identical call and
  returns `noErr`. Reproduced with DLS alone, with the corpus lock held, on the
  first call, with and without `0xF0`/`0xF7` framing. It is excluded from the
  suite rather than worked around in `send_midi`: nothing in an AU's properties
  says "my SysEx entry point aborts", and suppressing SysEx for every instrument
  to dodge one broken unit would deny it to the ones that work. Surviving
  arbitrary AUs is what the plugin-server's process isolation is for.
- **The delivery tests cannot fail.** Deleting the `send_sysex` call leaves all
  four green, because an AU has no channel to report receipt on. Stated in the
  module rather than left implied; what *is* pinned is the packet arithmetic.

The inbound test `sysex_is_dropped_without_derailing_the_rest` was justified as
*symmetry* with the outbound drop. That justification is now void and was never
the real reason — inbound drops SysEx because `from_midi1_bytes` would have to
allocate on the CoreMIDI read thread. Its doc comment is corrected.

### E-8 · Three properties worth having · HELD

`kAudioUnitProperty_PresentationLatency`(40) never written — a plugin doing
look-ahead metering cannot align its display. `ShouldAllocateBuffer`(51) never
set `false` even though this host always supplies its own buffers
(`instance.rs:1970-1977`) — pure per-instance waste, not a correctness bug.
`DependentParameters`(45) absent — a meta-parameter silently moves others and
our cached ranges go stale with no notification.

---

## Checked and clean

Recording these so nobody re-audits them.

**CLAP** — the audio-thread guard is the strongest part of the crate:
`AudioThreadClaim` (`host/state.rs:404-421`) is a genuine mutex-backed
implementation of `ext/thread-check.h`, correctly wrapping `process`,
`start_processing`, `stop_processing` and active `params.flush`; the
`flush_params` gate on `active` rather than `processing` (`params.rs:275-280`)
exactly matches `ext/params.h:303`. Host callbacks
(`request_restart`/`process`/`callback`) are all wired with matching pollers.
**No FFI return value is discarded anywhere in `src/`** — `activate` failure
propagates with rollback, `adjust_size == false` is fatal rather than "no snap",
`state_context` failure is a hard error. Embed sequence order matches
`gui.h:20-32` and is test-pinned. Transport flag gating verified against
`events.h`.

**VST2** — `audioMasterGetTime` is genuinely well done: re-entrant-safe seqlock
(`transport_cell.rs`), correct `*_VALID` gating including the zero-tempo guard,
`TRANSPORT_CHANGED` as a true edge, tests pinning it. `audioMasterAutomate` /
`BeginEdit` / `EndEdit` all wired and drained on the GUI thread. Chunk state
(`effGetChunk`/`effSetChunk`) checks every return, rejects null/negative, and
propagates `effSetChunk`'s refusal. `effGetParameterProperties`(56) and the
MIDI-metadata family (62–66) are the best-covered area of the crate. Editor
open/close idempotence is guarded and test-pinned. `effClose` vs `dlclose`
ordering is correct and documented. The prior TRANSPORT over-declaration is
**fixed and test-pinned in both paths**.

**AU** — the render path is clean: pre-allocated `RenderScratch`, no allocation,
no lock, no property write on any steady-state RT path; the input callback is
`catch_unwind`-wrapped, null-checks `ref_con` and `io_data`, and clamps writes to
the AU's *declared* `mDataByteSize` rather than the requested frame count
(`:2233-2247`). Error discards are deliberate and documented — teardown paths,
plus best-effort stream-format writes validated by explicit read-back rather
than by status, which is the better discipline. Editor teardown ordering is
correct.

---

## Suggested order

1. **A-1** — designs the field the three offline items need. Nothing downstream
   can be done first.
1b. **A-0** — independent of everything else, and cheap while the accessors are
   already written and no consumer matches on the variants. It gets more
   expensive with every consumer that learns to destructure them.
2. **E-4** — memory safety, one call, cheapest fix here.
3. **B-1**, **D-2** — both are spec-violating calls on the wrong state/thread,
   both have a correct sibling implementation to copy.
4. **D-1** — capability we claim and don't have.
5. **C-1**, **C-2**, **C-8** — CLAP correctness; each needs a doc correction as
   well as a code change.
6. **D-4**, **E-5** — both are "read once, cached forever" against a dynamic
   property; likely one shared shape.
7. **C-5** — widest blast radius, but mechanical once someone commits to the
   `_COMPAT` fallback pattern.
8. The rest, by area.

**D-6** needs a decision before it needs code: does the subprocess VST2 path own
editors at all?

**Closed out.** 31 DONE, 1 part-done, 3 HELD, no TODO remaining.

**C-12** (CLAP floating-window GUI) is **done at the host layer** and held at the
ECS layer — see its entry for what remains and why it is not worth building
against a plugin shape nobody has produced.

The three fully-held items are scope decisions rather than blocked work:
**D-9** (VST2 preset/program support), **D-10** (smaller VST2 opcode gaps — note
its `effGetNumMidiInputChannels` half hides a live bug, not just missing scope:
a plugin answering `Maybe` to `sendVstMidiEvent` is classified MIDI-silent and
its output dropped), and **E-8** (three AU properties).
