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

The format halves are already built and are reachable only from tests:

| Format | Call | Callers |
|---|---|---|
| CLAP | `set_render_mode` → `CLAP_RENDER_OFFLINE` (`instance/ports.rs:272`) | `loaders/clap.rs:1524` — past the `#[cfg(test)]` at 584 |
| AU | `set_offline_render` (`instance.rs:1295`, `offline.rs`) | crate tests only |
| VST2 | — | `get_process_level` returns a hardcoded `2` (`host.rs:159`), test-pinned |
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

### A-2 · No bounce driver can reach a hosted plugin · TODO

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

### C-5 · No `_COMPAT` extension id is ever queried · TODO

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

### C-6 · `audio-ports.rescan` flags discarded; `is_rescan_flag_supported` lies · TODO

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

### C-9 · Only descriptor index 0 is loadable · TODO

`src/instance/descriptor.rs:128` calls `get_desc(factory_ptr, 0)`;
`get_plugin_count` is called at `:112` only to check non-zero, and its value is
discarded. A bundle shipping a synth plus companion FX exposes only the first,
and there is no API to select by index or id.

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

### C-12 · Floating-window GUI mode unimplemented · HELD

`embed_editor_sequence` (`polling.rs:88-163`) hardcodes `is_floating = false`;
`get_preferred_api`, `set_transient`, `suggest_title` are absent.

`has_editor()` therefore queries `is_api_supported(api, false)` only, so a
floating-only plugin reports **no editor**. Per `ext/gui.h:66-68` embedding is
unsupported on Wayland — so every CLAP plugin is editor-less there by our
reckoning. Held rather than TODO: correct today on macOS/Windows/X11, and the
code degrades gracefully.

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

### D-2 · `reset()` and `set_sample_rate()` run suspend/resume on the audio thread · TODO

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

### D-4 · Latency is read before `effOpen` and never refreshed · TODO

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

### D-5 · `audioMasterUpdateDisplay`(42) and `audioMasterCurrentId`(2) are unroutable · TODO

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

### D-6 · Subprocess VST2 editors are never idled · TODO

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

### D-7 · `effEditGetRect` result partly discarded, and the `Rect` leaks · TODO

`editor.rs:36-49` uses width/height only; `position()` is never called, so the
plugin's requested origin is dropped. The vendor leaks on both sides:
`interfaces.rs:190-195` does `Box::into_raw(...)` with a literal
`// TODO: free memory`, and `host.rs:406` does `Some(unsafe { *rect })` with
`// TODO: Who owns rect?`. One `Rect` leaks per editor open, minimum.

### D-8 · `effCanBeAutomated` is surfaced but never called · TODO

`parameters.rs:188-189` ships `ParamFlags::empty()` with `known` empty. The doc
at `:123-126` says the vendored crate "does not surface" `effCanBeAutomated` —
**inaccurate**: `vendor/host.rs:1371` implements `can_be_automated`. Mild
consequence (empty `known` correctly signals unprobed), but the stated
justification is wrong and will mislead.

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

### E-1 · AUv3-only units are unreachable, and the flag that says so is discarded · TODO

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

### E-2 · Offline bounce renders in realtime · DONE — superseded by A-2

`src/offline.rs` implements `set_offline_render` / `set_render_quality`
correctly; nothing calls them. Structurally blocked by A-1.

### E-3 · The render timestamp is a free-running counter `reset()` does not reset · TODO

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

### E-5 · Latency, tail and parameter list are read once and cached forever · TODO

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

### E-6 · Cocoa editor: hardcoded preferred size, and a leak on the error path · TODO

`editor/cocoa.rs:97-101` hardcodes `NSSize { 800.0, 600.0 }` as
`inPreferredSize` (`AUCocoaUIView.h:47-48`), so every AU editor opens at 800×600
regardless of host window; `AuEditor::open` has no size parameter to thread one
through.

Same path: `cocoa.rs:87-94` allocates the factory and returns `Err` on the null
check at `:89` **without** the `release` the success path does at `:123`.

Teardown ordering itself is correct (`editor/mod.rs:87-96`) — no gap there.

### E-7 · SysEx is dropped; `MusicDeviceSysEx` is absent · TODO

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
