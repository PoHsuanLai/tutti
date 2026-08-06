# Outcomes: `Result` where there is a reason, an enum where there are states

## The problem

Nine methods on the plugin layer's host-facing surface answer a question with a
bare `bool` or with nothing at all. Each merges two or more outcomes a caller
would act on differently, and in several cases the distinguishing information
*already exists* one layer down and is discarded on the way up.

```
handles/capabilities.rs:133   fn load_state(&self, data: &[u8]);              // no answer at all
handles/capabilities.rs:280   fn load_preset(&self, id: &PresetId) -> bool;
handles/capabilities.rs:232   fn set_render_mode(&self, mode: RenderMode) -> bool;
handles/control_handle.rs:312 pub fn set_render_mode(..) -> bool;
plugin.rs:239                 pub fn set_midi_source(..) -> bool;
plugin.rs:254                 pub fn set_midi_out(..) -> bool;
plugin.rs:280                 pub fn set_render_mode(..) -> bool;
plugins.rs:287                pub fn unblacklist(&mut self, path: &Path) -> bool;
node/mod.rs:514               pub fn set_render_mode(..) -> bool;
```

Below them, the `_rt` fire-and-forget family in `ipc_client/` returns `bool`
from seven more.

This is not a style preference. The codebase has already made the argument for
sum types over collapsed answers, repeatedly and in writing —
[`ParamRange`](../../crates/tutti/crates/plugin/tutti-plugin-types/src/parameters.rs),
`ParamSteps`, `ParamFlags`+`known`, `EditorPresence`, `PluginTail`,
`LayoutSupport`, `PresetSupport`. `PresetSupport` puts it most sharply, in a
doc comment on a variant **no format produces today**:

> `ListOnly` … exists because the two halves are genuinely independent
> capabilities, and collapsing this case into `Full` would have a caller offer a
> load that fails.

A variant kept for a case nothing reaches, because the states are distinct.
That is the standard this note applies to the methods that did not get it.

## The rule

Two questions, in order:

1. **Does a reason exist that the answer is destroying?** If a layer below
   produced an error — a message, a status code, a `Result` — the answer is
   `Result<T, E>`, and `E` carries what that layer said. Anything less throws
   away work already done.

2. **If there is no message, are the failure states distinct enough to act on
   differently?** If yes, a **fieldless enum**. If the caller's response is the
   same in every failing case, `bool` is honest and an enum is ceremony.

Note what this does *not* say: it does not say "never `bool`". A predicate —
`is_crashed`, `has_editor`, `accepts` — answers a yes/no question and returns
the right type already. The rule is about *outcomes*, where `false` is standing
in for "something went wrong" and refusing to say what.

## Case by case

### 1. `HostState::load_state` — `Result`, and the worst of them

Returns **nothing**. The outcome exists at every layer beneath and is dropped at
the last step:

| layer | signature | carries it? |
|---|---|---|
| `PluginState::set_state` (subprocess) | `-> Result<()>` | yes |
| `Session::handle_load_state` | emits `BridgeMessage::Error { message }` | yes |
| `dispatch.rs` `Command::LoadState` | `reply.send(true)` | **no — fabricated** |
| `AudioBridge::load_state` | `-> bool` | no (always `true`) |
| **`HostState::load_state`** | **`-> ()`** | **no** |

The subprocess formats `"Failed to load state: {e}"` and puts it on the wire —
but as a **fire-and-forget** `BridgeMessage::Error`, not as a reply to the
request. Meanwhile the host waits on a different channel, and the dispatcher
never waits at all:

```rust
Command::LoadState { data, reply } => {
    ipc::send(stream, &HostMessage::LoadState { data })?;
    reply.send(true);          // ← unconditional, before any answer
}
```

Compare `Command::SaveState` three lines above, which calls `recv_reply` and
matches the response. So the `bool` two layers up is not a *narrowed* outcome —
it is a **constant**. It reports that the message was written to a socket.

This is worse than "the reason is dropped at the boundary", which is how this
note first described it. There is nothing to drop: no `StateLoaded`
acknowledgement variant exists on `BridgeMessage`, so the success path has never
been observable out-of-process.

**Failure:** a user saves a preset, upgrades the plugin, loads it back. The
plugin rejects the chunk — routine on a version bump. Nothing reports it; the
plugin sits at defaults and the user believes the patch was restored. Same for
a truncated file or a chunk from a different plugin.

This is not confined to a CLI. `bevy-tutti/src/plugin_host/load.rs:250` is the
**project-load path**:

```rust
if let Some(blob) = &request.state {
    handle.state().load_state(blob);
}
```

Open a project, a plugin rejects its state, the DAW shows a loaded plugin at
defaults and says nothing.

```rust
fn load_state(&self, data: &[u8]) -> Result<(), StateError>;
```

`StateError` follows [`EditorError`](../../crates/tutti/crates/plugin/tutti-plugin-types/src/editor.rs)'s
shape — a `thiserror` enum whose variants name the cause rather than one opaque
string:

```rust
pub enum StateError {
    PluginCrashed,
    Rejected(String),   // the plugin's own message, already formatted upstream
    NoStateRoute,       // this backend cannot carry state at all
}
```

**The two backends need different amounts of work**, and only one of them is a
signature change:

| backend | what it takes |
|---|---|
| in-process VST2, `dawai-wasm-plugin` | signature only — the `Result` is already in hand and discarded on one line (`let _ = self.inner.lock().load_state(data);`) |
| subprocess (IPC) | a new `BridgeMessage::StateLoaded { error: Option<String> }` ack, `dispatch.rs` waiting on it like `SaveState` does, and a `PROTOCOL_VERSION` bump |

Worth doing in that order — the in-process half is small, self-contained, and
closes the data-loss hole for every plugin the host loads in-process. The IPC
half is the larger piece and carries the version bump.

The bump is unavoidable and mandatory in both directions: a v17 server would
have no arm for a request that expects an ack, and appending a `BridgeMessage`
variant shifts no existing discriminant but does introduce a tag a v17 host
cannot decode. Same argument as v11–v17.

### 2. The `_rt` family — a fieldless enum

```rust
pub fn set_parameter_rt(&self, param_id: ParamAddress, value: f32) -> bool {
    !self.lifecycle.is_crashed() && self.channels.push_command(..)
}
```

Two causes merge: **the plugin is dead** and **the command queue is full**.
They want opposite responses — one means stop using this plugin, the other means
retry next block. A caller reading `false` will either spin forever against a
corpse or abandon a transient backlog.

`Result` is the wrong tool here: there is no message, and this is RT-adjacent.
A fieldless enum costs nothing.

```rust
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivered {
    /// Queued for the audio thread.
    Yes,
    /// The queue was full. Transient — the same call may succeed next block.
    Dropped,
    /// The plugin is gone. Permanent; every later call will answer the same.
    PluginDead,
}
```

*This case was argued the other way first.* The initial reading was "one failure,
no message, so `bool` is honest" — wrong, because it read the return type rather
than the body. The two conditions are `is_crashed()` and `push_command()`, and
they are not the same failure.

**Then it was argued too far the other way.** "Convert the `_rt` family" is what
this section originally said, and reading the call sites before doing it showed
why that was wrong too: there are **ten** such functions, and **nine of them have
no caller that reads the answer** — every site either `let _ =`s it or forwards
it unread, and nothing outside `tutti-plugin` calls any of them at all. Only
`set_automation_state_rt` is consumed, by `SubprocessBackend::set_automation_mode`,
which turns it into a user-visible error.

So `Delivered` landed on that one (`e2f71fbcc`) and the other nine kept the bool.
This is the rule above applied honestly rather than abandoned: an enum earns its
place when *the caller's response differs*, and for nine of these there is no
response to differ. Converting them would have been ~20 edits serving zero
branches, on the RT-adjacent path.

The general lesson, which cost two wrong readings on one case: **the signature
does not tell you whether a collapse matters — the body tells you whether one
happened, and the call sites tell you whether anyone is harmed by it.** Both
have to be read.

### 3. `set_midi_source` / `set_midi_out` — **leave**, on inspection

This section originally claimed `false` merges "this plugin does not accept
MIDI" with "the wiring failed". Reading the body refutes it: there is a single
`accepts(Features::MIDI_IN)` gate, and the backend calls below it return `()`.
Wiring cannot fail. One cause, no message — `bool` is honest, and `#[must_use]`
is already on both.

### 4. `load_preset` — **already solved**, before this note was written

The three-way distinction this section asked for exists: `presets()` returns
`Option<&dyn HostPresets>`, so `None` is "no preset route", `Some(false)` is "a
route that refused" and `Some(true)` is acceptance. It is pinned by
`only_an_accepted_load_reports_true`, whose doc comment makes exactly the
argument this note makes:

> Reaching presets through the capability keeps three answers apart that the old
> `handle.load_preset(..) -> bool` collapsed into two … a UI that wants to report
> "this plugin cannot load presets" needs to tell them apart.

Listing it here was a miss — the survey read the signature and not its
surroundings, which is the same error recorded under the `_rt` case.

### 5. `unblacklist` — leave

`false` means "no such record", which is the only failure and is exactly what
`bool` says. A predicate-shaped answer to a predicate-shaped question.

### 6. `set_render_mode` — leave, for now

The only one of the nine whose collapse is **argued in writing** rather than
defaulted:

> `false` when this handle carries no render-mode route *or* the plugin
> declined. Those collapse deliberately — both mean the render is unchanged —
> and a caller that needs to tell them apart reads `Features::RENDER_MODE`. Kept
> rather than left to `render_mode` precisely because that collapse is the
> useful answer: **every caller so far wants the one bool.**

It also carries `#[must_use]`, which is what actually prevents the bug. The
escape hatch (`Features::RENDER_MODE`) exists, so nothing is destroyed.

Worth recording that "every caller so far" is already weakening: a CLI reporting
why an offline bounce did not take offline mode would want the split. Revisit
when a second caller needs it — not before.

## What this is not

**Not a rule against `bool`.** Predicates keep it. The test is whether `false`
is standing in for a reason that exists.

**Not `Result` everywhere.** A `Result` whose error is fieldless and single-
variant is worse than an enum: it implies a message that is not there, and costs
an allocation on paths that cannot afford one.

**Not a sweep of the format crates.** ~80 `bool`-returning functions exist
across `formats/*`, and most are internal to one crate where the collapse is
visible to its own author. This note covers the ~16 on the *boundary* — the
host-facing surface plus the `_rt` family — where a caller in another crate
cannot see what was merged.

## Verification

Each replacement needs a test that the *distinction* survives, not merely that
the type changed:

- `load_state` on a plugin that rejects a chunk returns `Rejected` carrying the
  plugin's message, not a generic failure. The available fixture is a valid
  chunk from one plugin fed to another.
- **The same test run against the subprocess backend**, not only in-process.
  This is the one that would have caught the fabricated `reply.send(true)`:
  every existing state test either runs in-process or asserts a byte-exact
  *round-trip* (`save_load_state_round_trips_byte_exact`), and a round-trip that
  never fails cannot distinguish "loaded" from "sent". A rejection fixture is
  the missing case.
- `set_parameter_rt` against a crashed plugin returns `PluginDead`, and against
  a full queue returns `Dropped` — two tests, because one of them passing
  proves nothing about the other.
- A caller that ignores the outcome fails to compile (`#[must_use]` on the enum,
  which `Result` gets for free).

Mutation-test each: collapse the new variants back into one and confirm the test
fails. A test that passes with `Dropped` and `PluginDead` merged is testing the
type, not the behaviour.

## Order

1. ~~`load_state`~~ — **DONE**, both halves in one change (`eb95009cb`). The
   in-process and IPC parts turned out to be hard to separate: the trait
   signature is shared, so changing it forces both implementors at once.
   `StateError` landed in `tutti-plugin-types` beside `PluginError`, and the
   wire gained `BridgeMessage::StateLoaded` at `PROTOCOL_VERSION` 18.
2. ~~The `_rt` family~~ — **DONE**, but only for the one member whose answer is
   read (`e2f71fbcc`). See the case above for why the other nine were left.
3. ~~`set_midi_source`/`set_midi_out`, `load_preset`~~ — **no work needed**;
   see the two cases above. One has a single honest cause, the other was
   already solved by the capability accessor.

Deliberately separate from the parameter-grouping PR, which is at eight commits
across four themes already.

## Outcome

Nine methods were surveyed. **One was a real bug** (`load_state`, silent data
loss — and worse than described here, since the reply was fabricated three
layers down rather than merely narrowed). **One was a real collapse worth an
enum** (`set_automation_state_rt`). **Seven needed nothing**: two were already
solved, one had a single honest cause, one had its collapse argued in writing
with an escape hatch, and the rest have no caller that reads the answer.

That ratio is the finding. A survey by signature flagged nine; reading bodies
and call sites left two. The rule at the top of this note is sound, but it can
only be applied after both — the signature shows a collapse might exist, the
body shows whether one does, and the call sites show whether anyone is harmed.
Applied to a signature alone it produces churn that looks like rigour.
