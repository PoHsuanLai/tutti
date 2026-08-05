# 005 · Plugin presets: a cross-format surface

Closes **D-9** from `004-plugin-host-gap-audit.md`.

## The finding that shapes everything

The audit entry reads as a VST2 gap. It is not. Every format already implements
presets at its own layer:

| Format | Enumerate | Load | Read current |
|---|---|---|---|
| AU | `factory_presets()` | `load_factory_preset(number)` | `current_preset()` |
| VST3 | `program_lists()` + `program_name(list, idx)` | *no direct call — the `kIsProgramChange` param* | *read that param back* |
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

### 1. CLAP cannot enumerate, and VST3 has no load *call*

These are *different* formats failing *different* halves, which is why one
`PRESETS` capability bit cannot express it:

- **CLAP** loads by **filesystem path** (`CLAP_EXT_PRESET_LOAD`). Discovery is a
  separate *factory-level* extension this host does not bind, so a CLAP plugin
  can only ever be pointed at a path the host already knows.
  `supports_preset_load()` already documents exactly this.
- **VST3** enumerates richly and has **no load call at all**. A program is
  selected by writing a *parameter* — the one flagged
  `kIsProgramChange` (`info.rs:173`) — through the ordinary parameter path. The
  format layer now does that write itself, so a caller sees a normal load; what
  survives of the asymmetry is that VST3's two bits move together, where CLAP's
  do not.

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
| **VST3** | `program_lists()` × `program_name` → `Program{..}`, `bank = list.name` | write the `kIsProgramChange` param — see below | read it back |
| **CLAP** | `Vec::new()` | `Location(p)` → `load_preset(&p)` | `None` |

**VST3's load goes through its program-change parameter.** *(Revised: this
section first said `load_preset` returns `false`, and the follow-up it deferred
was done immediately — see "How VST3's load landed" below.)*

Selecting a program means writing the parameter flagged `kIsProgramChange`.
That reasoning was right about not routing it through `load_preset` **blindly**;
it was wrong to stop there. Done inside the format layer, where the owning
parameter is identifiable, it is still a single write path — the parameter
remains the only thing written — and it is what makes the four formats one API
instead of three plus a special case.

### How VST3's load landed

The link is **parameter → unit → program list**: a program-change parameter
belongs to a unit, and that unit names the list it selects from. Matching on the
*flag* rather than a name or position is what makes it work for a plugin with
several lists.

`kIsProgramChange` was read at the VST3 layer and **dropped at the mapping** —
`build_param_info` carried five flags and not this one. It now maps to a shared
`ParamFlags::PROGRAM_CHANGE`, which is what lets the loader find the parameter.

The normalization is `index / step_count`, and **the divisor is the finding**.
`StringListParameter::toNormalized` is `value / stepCount` (`futils.h:87-90`)
and `appendString` increments `stepCount` per entry from zero, so 128 programs
report `step_count == 127` and index 2 is `2/127`, not `2/128`. Measured against
the SDK sample, which reports exactly 127.

That off-by-one **survives a round-trip test**: `get_current_preset` inverts the
same formula, so a wrong divisor agrees with itself. Only pinning the absolute
normalized value the plugin holds separates them.

### VST2's begin/end bracket

`change_preset` is wrapped in `effBeginSetProgram`(67) / `effEndSetProgram`(68).
Without them a preset switch looks to the host like forty individual parameter
edits rather than one atomic event — which matters here specifically, because
`audioMasterAutomate` is already wired and drained (audit: "Checked and clean").
An unbracketed switch would flood that path.

Both opcodes exist in the vendor enum; neither is currently sent.

### `Features` wiring

No new bits — and **less new wiring than this design first assumed.** Three of
the four masks are already correct and already test-pinned in
`features.rs`'s test module, which anticipated this exact asymmetry:

- **AU** — probes both, from the one property that backs both. Correct.
- **CLAP** — probes `PRESET_LOAD`, deliberately **not** `PRESET_LIST`; the doc
  on `probed::CLAP` already says discovery is a factory-level extension this
  host does not bind. Correct, and `a_format_can_load_a_preset_without_being_
  able_to_list_one` pins it.
- **VST3** — probes **both**, and now answers **both from one question**: does
  the plugin publish program lists. A plugin with lists can do both halves,
  since selecting is writing the parameter that owns the list; one without can
  do neither. *(Revised: this said "reports `Some(false)` for each", true only
  while VST3's load was unimplemented.)*

That correction stands on the *probing* half, which was an earlier draft's
error: it proposed leaving `PRESET_LOAD` unprobed on the `EDITOR_FLOATING`
precedent. Wrong here — there, the format has **no way to be asked** whether it
embeds or floats, so the bit is genuinely unprobed; VST3 *has* been asked, and
now answers yes or no from its program lists rather than always no.

So only one mask changes:

- **VST2** — currently probes neither, correctly, because this host binds
  neither opcode. `the_vst2_loader_does_not_claim_presets` pins that and **must
  be updated in the same commit** that binds them. After step 3 both bits are
  probed: `PRESET_LIST` = `Info::presets > 0`, `PRESET_LOAD` = the same, since
  VST2 offers no separate refusal.

**A consequence worth stating:** `presets()` returning empty is not the same
question as `PRESET_LIST`. VST3 will return a non-empty list while reporting
`PRESET_LIST = Some(false)` — the bit answers "is there a separate preset
mechanism", the method answers "what can I show a user". A caller building a
browser reads the method; a caller deciding whether to offer a *load* button
reads `PRESET_LOAD`.

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
- ~~**VST3 program-change parameter routing.**~~ Done — see "How VST3's load
  landed". Deferring it would have left one format needing caller-side special
  handling, which is the thing this design exists to remove.
- **Any ECS layer.** Nothing in `bevy-tutti` yet. Following the C-12 lesson:
  land the host surface, add ECS when a UI exists to drive it.

## Implementation order — **all shipped**

Each step compiled and tested alone. Noted below: what each step found that the
plan did not predict.

1. ✅ **Vocabulary.** `PresetId`, `Preset` in `tutti-plugin-types`. Round-trip test
   per variant, plus a test pinning each variant's *wire discriminant* — a
   symmetric round-trip cannot catch a reordering, because both ends of
   `serialize`/`deserialize` move together.
   → `cargo test -p tutti-plugin-types --features serde`

   **The `PROTOCOL_VERSION` bump belongs in step 7, not here.** Nothing crosses
   the wire until the frames exist; bumping now would refuse a v13 pairing over
   a capability neither side can yet use. The version gates the *frames*, and
   the frames are step 7.
2. ✅ **The trait**, plus `PluginHandle::presets()` / `load_preset()` /
   `current_preset()` and the `HostPresets` accessor, mirroring
   `render_mode()`'s `Option<&dyn>` shape for a format that does not implement
   it. → `cargo check -p tutti-plugin`
3. ✅ **VST2** — the richest case, and the one with the bracket. Probe bits, the
   begin/end wrap, and the enumeration hole to respect: the probe already models
   `serviced_programs` below `programs`, so a host walking `0..Info::presets`
   and trusting every answer reads names the plugin never had.
   → `cargo test -p tutti-vst2-host`
4. ✅ **AU** — mapping only, both methods exist. Test against the corpus, and pin
   that a sparsely-numbered unit round-trips (`Number` carries the selector, not
   a position). → `cargo test -p tutti-au-host`
5. ✅ **VST3** — `presets()` with banks. Shipped in two parts: first
   `load_preset` → `false` as planned, then the load itself once it was clear a
   coherent API could not leave one format with a special case.
   → `cargo test -p tutti-plugin-server --features vst3`
6. ✅ **CLAP** — `presets()` → empty, `load_preset` on `Location`. Pin that an
   empty list plus a set `PRESET_LOAD` bit is a coherent state.
   → `cargo test -p tutti-clap-host`
7. ✅ **IPC frames + loader wiring**, `PROTOCOL_VERSION` 13 → 14.
   → `cargo test -p tutti-plugin-server`
8. ✅ **README capability table** — VST2's two preset cells moved `○` → `◐`.
9. ✅ **`PresetSupport`** — not in the original plan. Added because steps 1–8
   produced a capability *report*, not an API: a caller still had to
   cross-reference two bits against two method returns and get the three-state
   reading of each right. One `handle.preset_support()` answers it.

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
- *a VST3 program loads and reads back* — replaces the planned "declines a
  direct load", which stopped being true. Round-tripped **and** pinned against
  the absolute normalized value, because the round trip alone cannot catch the
  `i / (N-1)` vs `i / N` divisor: `get_current_preset` inverts the same formula.

**Plus:**

- a VST2 preset switch is bracketed by begin/end, **in order** — the probe
  records an ordered trace, because a `begin` arriving after the change it
  brackets is as wrong as one that never arrives, and two counters cannot tell
  those apart
- the probe's enumeration hole is respected — `serviced_programs` below
  `programs` must not produce named presets the plugin never had, and an unnamed
  slot is *kept*, since dropping it renumbers every program after it
- every `PresetId` variant round-trips through bincode, **and keeps its wire
  discriminant** — a symmetric round-trip cannot catch a reordering
- the v14 frames are pinned at absolute tags, not merely "later than" the v13
  ones: a variant inserted *between* two existing ones survives a relative check
- a refused load returns `false` rather than reporting success
- `PresetSupport` maps each format's real mask to the right variant, and no
  variant overclaims `can_list` / `can_load`

**Fixture work needed up front:** the VST2 probe must record `effBeginSetProgram`
/ `effEndSetProgram` arrivals — the vendor dispatcher has no arm for either
today, so no test can currently witness the bracket. Same shape as the
`set_bypass` / `get_effect_name` hooks added in D-10: a defaulted `Plugin`
method preserving current behaviour, overridden by the probe.

## Coverage gaps, measured

Two claims the code makes that no available fixture can witness. Both were found
by mutation — the mutant survived — and are recorded at the code rather than
papered over with a weaker assertion.

- **AU's sparse selector.** `PresetId` carries `AuPreset::number` rather than a
  vec position because a unit may number sparsely. All four preset-bearing Apple
  effects number densely `0..n` on this machine, so replacing `p.number` with
  `.enumerate()`'s index leaves the round trip green. Pinned instead where the
  value *can* be constructed, in `presets::tests`.
- **VST3's program-change flag.** `program_change_param` matches on the flag, not
  on position. The SDK's `multiple_programchanges` sample gives each unit exactly
  one parameter, so "first parameter of the unit" coincides. The fixture that
  would separate them is `mda-vst3` — its controllers add Bypass to the same root
  unit ahead of the preset parameter — but its shell exposes the base controller
  with a dangling program-list id, and `load_class` cannot re-open the bundle
  while the first load holds it.

One *equivalent* mutation, distinct from the above: swapping
`report.get(f) == Some(true)` for `report.enabled(f)` in `PresetSupport` cannot
be distinguished by any input, because `FeatureReport::new` stores
`features & probed`. Not a gap — the two are provably the same here.

Also uncovered, for want of a plugin rather than a fixture design: a **CLAP
preset actually loading**. No `.clap-preset` file exists on this machine, so only
the refusal path is exercised.
