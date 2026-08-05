# 007 · Channel topology: which speaker, not how many

Closes **D-11** from `004-plugin-host-gap-audit.md`.

> Numbering note: `005` is used twice on `main` + the unmerged presets branch
> (`005-panel-style.md` and `005-plugin-presets.md`). `006` is left free for
> that collision to be resolved into; this doc takes `007`.

## The finding that shapes everything

The audit entry reads as a VST2 gap — "opcode 42 is unimplemented". It is not.
**Every format already reaches speaker placement, and every one of them throws
it away at its own FFI boundary.**

| Format | Reaches topology? | What survives the boundary |
|---|---|---|
| **AU** | Fully — `AuLayoutTag`, get *and* set, 19 tests | The tag… but **zero consumers** outside `tutti-au-host` |
| **VST3** | Both directions | `count_ones()`, on the line after the mask arrives |
| **CLAP** | `clap.surround` + ambisonic getters | Nothing — those getters have **no callers at all** |
| **VST2** | No | The `VstSpeakerArrangement` struct does not exist |

The root cause is one type. `ChannelLayout` is `pub struct ChannelLayout(u16)`
(`tutti-types/src/channels.rs:73`) — a **width, not a layout** — and that was
deliberate. Its module docs say so (`channels.rs:8-13`): foreign layout types
"convert to and from this at their crate boundary, **losing the placement
deliberately**".

So this is the same shape D-9 found: data fetched, then dropped for want of a
consumer. **Adding a field to `LoadedPlugin` accomplishes nothing** — the
information is already destroyed at `clap ports.rs:660`, `vst3 instance.rs:78`,
and `vst2 instance.rs:236` before anything could populate it.

This doc therefore proposes a vocabulary change, and says plainly what it costs.

## The decision this reopens

`ChannelLayout` being count-only is a *settled decision with a written
rationale*. Nothing below overturns it. The proposal is **additive**:
`ChannelLayout` keeps answering "how many", and a new type answers "which
speaker". They are different questions with different algebras, which is exactly
the units rule's test for whether a second type is earned.

Concretely, `ChannelLayout::QUAD` is currently documented as "quad / ambisonic
B-format" (`channels.rs:108`) — one constant for two incompatible meanings.
That is the cost of width-only, already visible in the vocabulary.

## The load-bearing question: set or list?

Surveyed at the primary headers, not our wrappers.

- **VST3 is a SET.** `SpeakerArrangement` is a 64-bit mask (`vstspeaker.h:28`).
  Channel order is not stored — it is *derived*: `getSpeakerIndex`
  (`vstspeaker.h:689-704`) returns the popcount of set bits below a speaker, so
  buffer order is strictly ascending bit index. VST3 **cannot** express two
  orders of the same speaker set.
- **AU, CLAP and VST2 are ORDERED LISTS.** CLAP is the clearest:
  `channel_map[i]` is the speaker fed by channel `i` (`surround.h:66`). AU tags
  are ordered templates — Apple defines `MPEG_5_1_A/B/C/D` as four *different
  orders* of the identical six speakers (`CoreAudioBaseTypes.h:1287-1290`).
  VST2 carries a tag plus a per-channel `speakers[]` array.

**A list is strictly more expressive than a set, so the shared type is a list.**
A set cannot represent `MPEG_5_1_B`; a list represents every VST3 arrangement
without loss.

This makes the lossy direction explicit and singular: **shared → VST3**. Every
other conversion is total.

## The type

In `tutti-types`, beside `ChannelLayout` — not in a plugin crate. Tail is
already a precedent for engine-wide vocabulary living here, and the engine has
its own consumer (below).

```rust
/// Which speaker one channel feeds.
///
/// Open catalog: formats keep adding positions, so an unnameable one is
/// carried rather than rejected.
pub enum Speaker {
    FrontLeft, FrontRight, FrontCenter, LowFrequency,
    BackLeft, BackRight, /* … */
    Unknown(u16),
}

/// The speaker each channel of a bus feeds, in channel order.
///
/// `positions[i]` is the speaker fed by channel `i`, so `len()` is the width.
pub struct ChannelTopology { positions: SmallVec<[Speaker; 8]> }

impl ChannelTopology {
    pub fn layout(&self) -> ChannelLayout;   // the width — never stored twice
    pub fn index_of(&self, s: Speaker) -> Option<usize>;
    pub const fn smpte(layout: ChannelLayout) -> Option<Self>;
}
```

Three properties, each answering a rule this repo already enforces:

- **No stored width.** `layout()` derives it from `positions.len()`. A stored
  count beside the list is a second owner needing invalidation.
- **`Unknown(u16)` holds its slot.** The CLAP fix in PR #198 is the worked
  example: dropping an unnameable position from a *positional* list renumbers
  every channel after it. Same reasoning as `AuLayoutTag::Unknown`.
- **`smpte()` is the engine's own order**, named once. Today that order is
  implicit in a `match` on width in three places
  (`downmix.rs:54-98`, `spatial/nodes.rs:84`, `spatial/nodes.rs:101`).

### What this is *not*

Not a replacement for `ChannelLayout`. Every existing signature keeps taking a
width; only code that genuinely routes *by speaker* takes a topology. If a
change turns a `ChannelLayout` parameter into a `ChannelTopology` one without a
routing reason, that is a mistake.

**Ambisonics is not speaker positions** and must not be modelled as such —
B-format `W X Y Z` is a spherical-harmonic encoding, not four speaker feeds.
All three formats treat it as a separate axis (VST3 ACN bits, AU
`HOA_ACN_SN3D`, CLAP `CLAP_PORT_AMBISONIC`). It gets its own variant later or
stays unrepresented; it does not get four fake speakers. This is precisely the
ambiguity `ChannelLayout::QUAD`'s doc comment currently papers over.

## What it fixes on day one

This is the part that decides whether the type earns itself. Two concrete
defects, both already recorded in the audit:

1. **VST3 proposes the wrong speaker set at 4 and 8 channels.**
   `(1u64 << n) - 1` (`vst3 instance.rs:69`) gives `L R C Lfe` for quad, where
   `k40Music` is `L R Ls Rs`. For 8 it gives a front-centre pair, not rear
   surrounds. Only 5.1 is right, and only because bits 0–5 are contiguous. The
   fix is `ChannelTopology → SpeakerArrangement`, which is the named-layout
   table this type exists to hold.
2. **CLAP's surround getters have no callers.** They decode into a positional
   vector that nothing consumes. `ChannelTopology` is what they decode *into*.

And one latent trap it closes: `downmix.rs` infers meaning from width, so a
4-channel B-format buffer would fold as quad. No ambisonic source exists today,
so this is correct by construction — but the inference has no way to *say* what
it assumes. `smpte()` gives it one.

## Scope, honestly

**Reachability.** None of this is reachable by a user today. `AuLoaded::new`
widens to at least stereo (`au instance.rs:1919`) — a *default*, not a guard,
which `new_with_config` bypasses, though that constructor has no caller outside
the crate. More decisively: nothing above the format hosts ever proposes a
layout, and the ECS→Net seam is unbuilt. This is foundation work, not a
user-visible fix. Worth doing now *because* it is unreachable — the conversions
can be corrected before anything depends on them.

**Ceilings.** `MAX_ROOT_CHANNELS = 8` and `MAX_NET_CHANNELS = 12` bound what the
engine can carry, so the `SmallVec<[Speaker; 8]>` inline capacity covers every
layout that can reach the graph.

**Wire.** `LoadedPlugin` gaining per-bus topology is a bincode field addition →
`PROTOCOL_VERSION` bump. Note it is **15**, not 14: the presets branch (#196)
takes 14 and is unmerged.

**Coverage limits, stated in advance.** The AU suite can test topology against
Apple's system units. For VST3 the corpus is the SDK samples, which are stereo —
so the round-trip conversions will be table-tested directly, and any rule no
available plugin can witness gets an explicit "no fixture can reach this"
comment rather than an assertion that always passes. That is the
`fixture-is-the-deliverable` rule; D-9 needed the same treatment twice.

## Steps

Each compiles, tests, and is independently revertable.

1. **`Speaker` + `ChannelTopology` in `tutti-types`**, with `smpte()` and unit
   tests. No consumer yet. → `cargo test -p tutti-types`
2. **VST3 conversions**, replacing `(1<<n)-1` with the named-layout table, and
   `getBusArrangement` decoding into a topology instead of `count_ones()`.
   Fixes defect 1. Round-trip table tests. → `-p tutti-vst3-host`
3. **AU conversions** — `AuLayoutTag ↔ ChannelTopology`, keyed on the tag, never
   on the count. Apple's per-tag orders are the table. → `-p tutti-au-host`
4. **CLAP conversions** — `channel_map` → topology, giving the getters from
   defect 2 their first consumer. → `-p tutti-clap-host`
5. **Carry it up**: `LoadedPlugin` per-bus topology, IPC frames,
   `PROTOCOL_VERSION` 15. → `-p tutti-plugin`
6. **The API surface** — a `PluginHandle` accessor, shaped like `PresetSupport`:
   report what the plugin is running, and whether a layout can be *proposed*
   (AU and VST3 yes, CLAP and VST2 no). → integration tests

Steps 2–4 are independent of each other; each is worth landing alone.

**Explicitly out of scope:** VST2's opcode 42. It needs a variable-length FFI
struct that does not exist, and until step 6 gives it a caller it would be a
surface with no policy behind it — the audit's original objection, which stands.
It becomes a small, well-defined follow-up once 1–6 land.

**Deferred:** an ambisonic variant, and the engine adopting `smpte()` in
`downmix.rs`/`spatial`. Both are safe to do later; neither blocks the above.
