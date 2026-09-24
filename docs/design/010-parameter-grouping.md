# Parameter grouping: four formats, one label

## The problem

A plugin with 400 parameters presents as one flat list. Every format we host has a
mechanism to prevent that, all four format crates decode it correctly, and all four
answers are discarded when the loader builds the shared
[`ParameterInfo`](../../crates/plugin/tutti-plugin-types/src/parameters.rs).

The loss is uniform, which is what makes it worth one change rather than four:

| format | native mechanism | decoded at | reaches `ParameterInfo` |
|---|---|---|---|
| VST3 | `ParameterInfo::unitId` + the `IUnitInfo` tree | `Vst3ParameterInfo::unit_id`, `Vst3Loaded::units` | no |
| CLAP | `clap_param_info.module`, a `/`-separated path | `ClapParamInfo::module` | no |
| VST2 | `effGetParameterProperties` category index + label | `ParameterCategory` | no |
| AU | `kAudioUnitParameterFlag_HasClump` + `clumpID` | `AuParameter::clump`, `clump_name` | no |

The format crates are not at fault here, and this note should not read as though they
were. Each decodes its mechanism more carefully than a bare field read would:

- AU's `clump` is `Option<u32>` gated on the `HasClump` flag, so an AU that never
  declared a clump reads as `None` rather than as clump 0.
- VST2's category index is documented as numbered from 1, with 0 meaning uncategorised.
- VST3 resolves a unit's `programListId` against the published lists so a dangling id
  becomes `None`.

Measured evidence that the mechanism is used in practice, from the AU host's own docs
(macOS 15.6): AUDistortion groups its 22 parameters into 7 clumps ("Delay", "Ring
Modulation", "Decimation", …); AUMultibandCompressor uses 6.

## The decision: a label, not a tree

Three of the four formats have no hierarchy at all — AU's clump is one integer, VST2's
category is one integer, CLAP's `module` is a path string but is documented as a display
hint. Only VST3 has a genuine tree, via units naming a `parent`.

Modelling the union would mean carrying VST3's tree and synthesising a degenerate
one-level tree for the other three. That fails the repo's own trait test — *a type earns
its name if you can state its boundary in one sentence without "and"* — and it would put
a shape on the wire that three of four producers cannot fill.

`Vst3Loaded::units` already made this call for the format layer, and the reasoning
transfers verbatim:

> Assembling a tree is the caller's job: a host that only wants to label parameter
> groups never needs one, and the shapes a plugin can report (orphans, cycles) have no
> single right resolution to bake in here.

**So the shared type carries the resolved group label, and nothing else.** A caller that
wants VST3's tree reaches past the shared vocabulary into `tutti-vst3-host`, which is
where the tree still is. This is the same layering as `ClapParamInfo`, whose doc already
says the rich native info "stays inside".

```rust
pub struct ParameterInfo {
    …
    /// The group this parameter belongs to, as a display label, or empty when
    /// the plugin declared none.
    pub group: String,
}
```

### Why `String` and not `Option<String>`

`unit: String` beside it already spells "the format carries none" as empty, and grouping
has no third state to distinguish: a parameter is either in a named group or it is not.
This is deliberately unlike `ParamSteps::Unknown` / `ParamFlags::known`, where "the
format never said" and "the format said no" are genuinely different facts a capability
report must keep apart. A group has no such split — a plugin that declares no clump and
a plugin whose clump has no name are the same thing to every consumer, which is a flat
list.

### Why not the group id

AU clump 3, VST2 category 3 and VST3 unit 3 are three unrelated numbers. Carrying the
id would invite exactly the mistake `ParamAddress` exists to prevent — a bare number
that does not say which model it belongs to. The label is the part that means the same
thing in all four formats.

The cost is real and worth stating: two groups a plugin genuinely distinguishes but
names identically collapse into one. That is a display-grouping artefact, and it does
not affect addressing, which still goes through `ParamAddress`.

## Per-format mapping

| format | label source | when empty |
|---|---|---|
| VST3 | the `name` of the unit whose `id == info.unit_id` | no `IUnitInfo`, `unit_id` is the root unit (0), or no unit matches |
| CLAP | `clap_param_info.module` verbatim | the plugin left it empty |
| VST2 | `ParameterCategory::label` | no `effGetParameterProperties`, or category index 0 |
| AU | `clump_name(unit, clump)` | `clump` is `None` (no `HasClump`), or the AU names no label |

Two of these need a lookup rather than a field copy, and both lookups are per-plugin,
not per-parameter: VST3 builds one `unit_id → name` map from `units()`, AU builds one
`clump → name` map from the clumps its parameters actually declare. Doing it per
parameter would be a round trip into the plugin per parameter — the shape the AU host's
docs already warn about for `kAudioUnitProperty_ParameterInfo`.

VST3's root unit is id 0 and means "the plugin itself". Mapping it to its name would
label every ungrouped parameter with the plugin's own name, which is noise, so the root
maps to empty.

## Protocol

`ParameterInfo` crosses the IPC wire inside `BridgeMessage::ParameterList` and
`ParameterInfoResponse`. It is a **struct**, so bincode writes every field positionally
with no tag: a peer built against the old layout stops reading before the new field, and
an old *server* sends a payload one field short. `serde(default)` does not rescue that —
bincode is not self-describing, so a short payload is a decode error rather than a
defaulted field.

This is precisely the v15 case (`LoadedPlugin` gaining `input_topology` /
`output_topology`), and it gets the same answer: **`PROTOCOL_VERSION` 15 → 16**, so the
skew becomes a clean refusal at the handshake instead of a decode failure later.

## Why this is not the `PluginTail` mistake

`tutti-vst3-host`'s `loaded.rs` records the rule this change has to clear:

> Decoding a flag and dropping it inside one crate is a gap; shipping six across a
> versioned boundary to no receiver is the `PluginTail` mistake, which stayed write-only
> for a release cycle.

`PluginTail` crossed the wire and nothing read it. The test is therefore not "is the
data good?" but "does a consumer exist?", and the consumer must land with the field, not
after it.

The consumer here is `ParameterInfo::qualified_name`, which
renders `"Delay / Mix"` where a group exists and `"Mix"` where it does not. It is the
one operation every consumer of grouping performs, and it means no arm of this change is
write-only on arrival.

## What grouping is, and is not, for

Grouping is a **display** fix: a host renders sections instead of 400 flat rows. That
is the whole win, and it is complete with this change.

It was tempting to also read it as unblocking a name-keyed parameter lookup — a
conservative `find` returning `None` on an ambiguous name, plus a `find_in(group, name)`
that grouping would disambiguate. That was dropped, for three reasons in increasing
order of decisiveness:

- **Grouping does not make a name unique.** Two parameters named "Gain" in *the same*
  group still collide, and no format prevents that. A lookup that is correct except
  sometimes is worse than none, because the failure is silent and data-dependent.
- **Addressing is not the broken part.** [`ParamAddress`] already does this job, with a
  compile-time refusal to confuse VST3's opaque handle with VST2's dense index. A
  name-keyed lookup would be a second, weaker addressing scheme beside a working one —
  the "two names for the same behaviour is not a type" case.
- **Nothing calls it.** Every by-name lookup in the tree is in a test
  (`au_aupreset.rs`, `au_param_display.rs`, both `.iter().find(|p| p.name == …)`).
  Production call sites either pass the list through wholesale — `session.rs` ships it
  over IPC, CLAP's `lifecycle.rs` caches it — or address by id. The API would have
  guarded against a call nobody makes.

Should a UI later want it, the question to answer first is what it does with a
duplicate, not what the vocabulary can carry.

## Deliberately not in scope

- **VST3's unit tree.** Stays in `tutti-vst3-host`, reachable by a caller that needs it.
- **`unitId` for anything other than a label.** VST3 also uses units to scope program
  lists and note expressions; that is unit *addressing*, a different job from display
  grouping, and it has no cross-format meaning.
- **AU's non-GLOBAL scopes.** The AU host's default of `ParamAddress::GLOBAL` is
  documented and correct for effects and instruments — "none of which put parameters
  anywhere else" — with `_at` variants for a mixer-hosting caller. Not a gap.
- **Re-grouping on `kParamTitlesChanged` / `restartComponent`.** A plugin may restructure
  its parameters at runtime. The list is re-fetched wholesale when that fires, so the
  group comes back with it; no separate invalidation path is needed.

## Verification

Each format's mapping is testable without a plugin, because each is a pure projection
from an already-decoded native struct.

- a VST3 parameter naming a unit gets that unit's name
- a VST3 parameter naming the root unit (0) gets no group — pins the noise case
- a VST3 parameter naming a unit the plugin never published gets no group, rather than
  a panic or a wrong neighbour's label
- a CLAP module path passes through verbatim
- a VST2 category index of 0 is uncategorised, not category zero's label
- an AU parameter with no `HasClump` flag gets no group, and one with a clump gets its
  name — the two halves of the `Option<u32>` gate
- `qualified_name` renders the group where there is one and the bare name where there
  is not
- the group survives the bincode round trip

Mutation-test each: break the projection, watch the test fail. The VST3 dangling-unit
case and the AU flag gate are the two most likely to pass for the wrong reason, since
both have a plausible implementation that returns empty for every input.
