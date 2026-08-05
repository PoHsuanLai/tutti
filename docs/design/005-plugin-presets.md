# 005 · Plugin presets: a cross-format surface

Closes **D-9** from `004-plugin-host-gap-audit.md`.

## The finding that shapes everything

The audit entry reads as a VST2 gap. It is not. Every format already implements
presets at its own layer:

| Format | Enumerate | Load | Read current |
|---|---|---|---|
| AU | `factory_presets()` | `load_factory_preset(number)` | `current_preset()` |
| VST3 | `program_lists()` + `program_name(list, idx)` | *no call — see below* | *no call* |
| VST2 | `get_preset_name(i)` × `Info::presets` | `change_preset(i)` | `get_preset_num()` |
| CLAP | **cannot** | `load_preset(path)` | *no call* |

What does not exist is anything **above** them: no capability trait, no
`PluginHandle` method, no IPC frames. `Features::PRESET_LIST` and `PRESET_LOAD`
are declared and **nothing in the workspace reads either bit**.

So this is one surface over four existing implementations, not four
implementations. The risk is not FFI; it is picking a shape that fits one format
and mangles the other three.

## The two asymmetries that kill the naive design

The obvious surface is `list() -> Vec<Preset>` + `load(index)`. Both halves
break.

### 1. CLAP cannot enumerate, and VST3 cannot load

These are *different* formats failing *different* halves, which is why one
`PRESETS` capability bit cannot express it:

- **CLAP** loads by **filesystem path** (`CLAP_EXT_PRESET_LOAD`). Discovery is a
  separate *factory-level* extension this host does not bind, so a CLAP plugin
  can only ever be pointed at a path the host already knows.
  `supports_preset_load()` already documents exactly this.
- **VST3** enumerates richly and has **no load call at all**. A program is
  selected by writing a *parameter* — the one flagged
  `kIsProgramChange` (`info.rs:173`) — through the ordinary parameter path.

  `IUnitInfo::setUnitProgramData` exists in the bindings and is **not** it: it
  takes an `IBStream` of preset *bytes* and writes them *into* a program slot,
  which is the inverse operation. Named here because it is the obvious thing to
  find later and mistake for a load path.

The existing `PRESET_LIST` / `PRESET_LOAD` split already anticipates this. It
was written for AU, where one property backs both, but the two-bit shape is
exactly what CLAP-loads-but-cannot-list needs. **Keep both bits; do not collapse
them.**

### 2. A preset id is not an index

Three of the four number their presets in a space that is *not* a position in
the returned list:

- **AU** — `AuPreset::number` is an AU-assigned selector. Its doc is explicit:
  presets are commonly `0..n` but a unit may number sparsely, so *"never derive
  one from a position in the `factory_presets` vec"*.
- **VST3** — `Vst3ProgramListInfo::id` is a list id, and `program_name` takes
  `(list_id, program_index)`. Two spaces, and `program_name`'s doc already warns
  the id is *"not an index into `program_lists`"*.
- **CLAP** — the identifier is a path.

Only VST2 is genuinely positional (`change_preset(i)` against `Info::presets`).

**Consequence:** the id must be carried opaquely from list to load. A `usize`
index would work for VST2, silently corrupt sparse AU numbering, and cannot
represent VST3 or CLAP at all.

## Design

### `PresetId` — opaque, format-shaped

```rust
/// How a format names one preset. Opaque to the host: produced by `presets()`,
/// handed back to `load_preset()`, never constructed by a caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresetId {
    /// AU factory-preset selector, VST2 program index. A *number the format
    /// chose*, not a position — AU numbers sparsely.
    Number(i32),
    /// VST3: a program inside a named list. Two coordinates, because
    /// `getProgramName` takes both and neither is derivable from the other.
    Program { list_id: i32, index: u32 },
    /// CLAP: a filesystem location. The only identifier its load path accepts.
    Location(PathBuf),
}
```

Three variants because the formats genuinely have three *shapes* of identifier,
not three names for one — which is the units rule ("distinct range *or* distinct
algebra") applied to an id type. `Number` covers AU and VST2 together because
both are a single format-chosen integer with identical algebra; splitting them
would be two names for one behaviour.

**Why an enum and not an opaque `u64` token plus a side table:** a side table is
a second owner of the mapping, needs invalidation when the plugin reloads its
list (`kAudioUnitProperty_FactoryPresets` can change; VST3 sends
`kProgramListChanged`), and the invalidation always has a case it cannot see.
The enum carries the format's own identifier, so there is nothing to keep in
sync.

### `Preset` — one entry

```rust
pub struct Preset {
    pub id: PresetId,
    pub name: String,
    /// Which named set this belongs to. `None` when the format has one flat
    /// set (AU, VST2, CLAP); `Some` only for VST3, whose programs live in
    /// named lists attached to units.
    pub bank: Option<String>,
}
```

`bank` is `Option<String>` rather than a flattened prefix on `name` so a UI can
group without string-splitting, and so a format with one set does not have to
invent a bank name. Do **not** fold it into `name`: that is lossy in exactly the
direction `boundary-layers-dont-drop-data` warns about.

### The capability trait

```rust
/// Preset enumeration and loading. Two halves because no format has both
/// unconditionally — CLAP cannot enumerate, VST3 cannot load directly.
pub trait HostPresets: Send + Sync {
    /// Every preset the plugin advertises. Empty when the format cannot
    /// enumerate (CLAP) — which is why `Features::PRESET_LIST` is the thing to
    /// check, not `is_empty()`: a plugin with genuinely zero presets and one
    /// that was never asked are different answers.
    fn presets(&self) -> Vec<Preset>;

    /// Ask the plugin to load one. `false` when it refuses or the format has no
    /// direct load path (VST3 — the caller writes the program-change parameter
    /// instead).
    fn load_preset(&self, id: &PresetId) -> bool;

    /// Which preset the plugin considers current, when it will say.
    /// `None` is "did not answer", never "the first one".
    fn current_preset(&self) -> Option<PresetId>;
}
```

Returning `bool` from `load_preset` rather than swallowing follows the
`set_bypass` precedent landed in D-10: a refusal changes what the host must do
next (leave the UI selection where it was), so it cannot be dropped.

### Per-format mapping

| Format | `presets()` | `load_preset()` | `current_preset()` |
|---|---|---|---|
| **AU** | `factory_presets()` → `Number(p.number)` | `load_factory_preset(n)` | `current_preset()` → `Number` |
| **VST2** | `0..Info::presets` × `get_preset_name(i)` → `Number(i)` | `change_preset(i)`, bracketed | `get_preset_num()` → `Number` |
| **VST3** | `program_lists()` × `program_name` → `Program{..}`, `bank = list.name` | `false` — see below | `None` |
| **CLAP** | `Vec::new()` | `Location(p)` → `load_preset(&p)` | `None` |

**VST3's `load_preset` deliberately returns `false`.** Selecting a program means
writing the `kIsProgramChange` parameter through the existing parameter path,
which is a *different mechanism* with its own automation and undo semantics.
Routing it through `load_preset` would give one operation two write paths — the
thing "one writer per field" exists to prevent. The trait reports honestly that
it has no direct load; a follow-up exposes the program-change param id so a
caller can drive it deliberately. `Features::PRESET_LOAD` stays clear for VST3,
which is what the bit is *for*.

### VST2's begin/end bracket

`change_preset` is wrapped in `effBeginSetProgram`(67) / `effEndSetProgram`(68).
Without them a preset switch looks to the host like forty individual parameter
edits rather than one atomic event — which matters here specifically, because
`audioMasterAutomate` is already wired and drained (audit: "Checked and clean").
An unbracketed switch would flood that path.

Both opcodes exist in the vendor enum; neither is currently sent.

### `Features` wiring

No new bits. The probes each loader must add:

- **AU** — already declared, already correct.
- **VST2** — add both to `probed::VST2`. `PRESET_LIST` = `Info::presets > 0`,
  `PRESET_LOAD` = the same (VST2 has no separate refusal).
- **VST3** — add `PRESET_LIST` only, set from `!program_lists().is_empty()`.
  `PRESET_LOAD` stays **unprobed**, not clear-and-false: VST3 was never asked
  whether it can load, because the question does not apply. This is the
  `EDITOR_FLOATING` precedent from C-12 — a clear-but-probed bit would spell
  "the plugin said no" for a question the format cannot be asked.
- **CLAP** — add `PRESET_LOAD` only, from `supports_preset_load()`.
  `PRESET_LIST` unprobed, same reasoning.

### IPC

Two frames beside the existing `GetParameterList` / `ParameterList` pair, which
they mirror exactly:

```
Request:   GetPresetList
           LoadPreset { id: PresetId }
Response:  PresetList { presets: Vec<Preset> }
           PresetLoaded { ok: bool }
```

`PresetId` and `Preset` derive `Serialize`/`Deserialize` and live in
`tutti-plugin-types` beside `ParameterInfo`.

**`PROTOCOL_VERSION` must bump.** The control channel is length-prefixed
**bincode** (`transport/control.rs:4`), which encodes an enum as a leading
discriminant — so appending arms shifts nothing already on the wire, but a new
arm is still unreadable to an older peer. The repo already settled this:
`loaded.rs:133-135` states that carrying six restart flags further "means new
`AsyncEvent` variants, and bincode encodes a discriminant, so appending one is a
`PROTOCOL_VERSION` bump". Same rule, same reason. The bump is not optional and
is the first thing to write.

That same comment is also the standing warning against this whole design going
wrong: it names `PluginTail`, which "stayed write-only for a release cycle", and
concludes **"plumbing follows a consumer, not the other way round"**. This
design only clears that bar because the consumer is the preset browser that
motivated it. If that UI is not being built, stop here — the four format layers
already work, and a fifth dead-ended surface is worse than the gap.

`current_preset` is deliberately **not** an IPC frame: it is a read of state the
host already tracks after a `LoadPreset`, and a round trip per query would make
a UI poll the subprocess. In-process backends answer it directly.

## What is deliberately not here

- **Preset *saving*.** `save_state`/`load_state` already round-trip a plugin's
  full state, which is what a session needs. User-preset files are a separate
  feature with its own file-format question.
- **CLAP preset discovery.** Binding the factory-level discovery extension is
  its own piece of work; without it CLAP stays load-only, which the two-bit
  capability split represents honestly.
- **VST3 program-change parameter routing.** Follow-up, per above.
- **Any ECS layer.** Nothing in `bevy-tutti` yet. Following the C-12 lesson:
  land the host surface, add ECS when a UI exists to drive it.

## Implementation order

Each step compiles and tests alone.

1. **Vocabulary + protocol bump.** `PresetId`, `Preset` in
   `tutti-plugin-types`; `PROTOCOL_VERSION` += 1. Round-trip test per variant.
   → `cargo test -p tutti-plugin-types`
2. **The trait**, plus `PluginHandle::presets()` / `load_preset()` /
   `current_preset()` and the `HostPresets` accessor, mirroring
   `render_mode()`'s `Option<&dyn>` shape for a format that does not implement
   it. → `cargo check -p tutti-plugin`
3. **VST2** — the richest case, and the one with the bracket. Probe bits, the
   begin/end wrap, and the enumeration hole to respect: the probe already models
   `serviced_programs` below `programs`, so a host walking `0..Info::presets`
   and trusting every answer reads names the plugin never had.
   → `cargo test -p tutti-vst2-host`
4. **AU** — mapping only, both methods exist. Test against the corpus, and pin
   that a sparsely-numbered unit round-trips (`Number` carries the selector, not
   a position). → `cargo test -p tutti-au-host`
5. **VST3** — `presets()` with banks; `load_preset` → `false` with a test
   pinning *that* rather than treating it as unimplemented.
   → `cargo test -p tutti-vst3-host`
6. **CLAP** — `presets()` → empty, `load_preset` on `Location`. Pin that an
   empty list plus a set `PRESET_LOAD` bit is a coherent state.
   → `cargo test -p tutti-clap-host`
7. **IPC frames + loader wiring** in `tutti-plugin-server`.
   → `cargo test -p tutti-plugin-server`
8. **README capability table** — VST2 presets currently read `✕` ("the format
   can't"); the truth is `○` ("we didn't"). Correct for every cell this changes.

## Verification

Property-named tests, prose doc above each, mutation-tested.

**The four that pin the asymmetries** — these are the design, and each fails a
naive implementation:

- *a sparse AU preset number survives the round trip* — build the id from
  `AuPreset::number`, not the vec position. Mutating to `enumerate()` must fail.
  Needs a unit numbering sparsely; if the corpus has none, say so in the test
  rather than asserting something weaker.
- *a VST3 program keeps its list id* — `Program { list_id, index }` with a
  plugin exposing ≥2 lists, so collapsing to a bare index is witnessable.
- *a CLAP plugin reports no presets and still loads one* — empty `presets()`
  with `PRESET_LOAD` set. Pins that empty-list ≠ no-preset-support.
- *VST3 declines a direct load without claiming failure* — `load_preset` is
  `false` **and** `PRESET_LOAD` is unprobed, not `Some(false)`.

**Plus:**

- a VST2 preset switch is bracketed by begin/end (probe records both opcodes)
- the probe's enumeration hole is respected — `serviced_programs` below
  `programs` must not produce named presets the plugin never had
- `PROTOCOL_VERSION` round-trips every `PresetId` variant through postcard
- a refused load returns `false` rather than reporting success

**Fixture work needed up front:** the VST2 probe must record `effBeginSetProgram`
/ `effEndSetProgram` arrivals — the vendor dispatcher has no arm for either
today, so no test can currently witness the bracket. Same shape as the
`set_bypass` / `get_effect_name` hooks added in D-10: a defaulted `Plugin`
method preserving current behaviour, overridden by the probe.
