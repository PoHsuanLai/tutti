# Engineering notes

The reasoning and history behind the rules in [`CLAUDE.md`](../CLAUDE.md).
`CLAUDE.md` keeps the rules short, and this file keeps *why*. Every section
except the command reference was moved here verbatim on 2026-09-24, so where a
section says "this file", it means `CLAUDE.md`. These are dated records: when
the code changes, fix the rule in `CLAUDE.md` and add a note here rather than
rewriting history.

## Origin

Guidance for Claude Code (claude.ai/code) working in the tutti audio engine.

This repo was extracted from the dawai app repo, where the engine lived at
`crates/tutti/` and the Bevy adapter beside it at `crates/bevy-tutti/`. Every
command below used to need a `--manifest-path` to cross that boundary; none do
now. If you find a comment or a doc that still says otherwise, **the comment is
the bug** — see "Extraction residue" at the bottom.

## The Bevy-free floor

The engine must stay usable without Bevy. `bevy` is an optional, **off-by-default**
feature on every engine crate that has one (`default = []`), and `tutti-units` has
no such feature at all. `bevy-tutti` is the only Bevy-mandatory member.

So `--no-default-features` does **not** test this — it drops the codec and midi
features instead. Name the feature:

```bash
cargo check -p tutti-core
cargo check -p tutti-core --features bevy
```

## The import-path gate

```bash
scripts/check-canonical-paths.sh
```

Engine types are imported from a crate's root or prelude, never through its
modules. Neither rustc nor clippy has a lint for a redundant path, and doc
comments in ` ```text ` / ` ```ignore ` blocks are never compiled — so this grep
is the only enforcement there is. It searches `*.md` as well as `*.rs` for that
reason.

## The submodule-pin gate

```bash
scripts/check-submodule-pins.sh
```

The three VST3 SDK submodules are documented as pinned at `v3.8.0_build_66` in
`.gitmodules`, in this file, and in `tutti-vst3-host/build.rs`. Prose cannot
notice a bump moving the tree out from under it, and a silently changed SDK
surfaces as an unexplained C++ compile error in `audio-probe` rather than as a
version complaint — so this compares those statements against the commits the
superproject actually records. Runs in the `canonical paths` CI job.

**It asserts SHAs, not `git describe`.** The 3.8.0 tag was never fetched into
this clone (a submodule fetch brings the recorded commit, not nearby tags), so
`git describe` falls back to the nearest ancestor and reports
`v3.7.3_build_20-NN-g…`. The content is 3.8.0 — the MIT `LICENSE.txt` is
Copyright (c) 2025, and `docs/reference/vst3-3.8.0-interface-surface.md`
agrees. A describe-based gate would fail on a correct tree, and a gate that
cries wolf gets deleted.

## Submodules

**`git clone` without `--recursive` leaves the VST3 SDK empty.** It is three
Steinberg repos pinned at `v3.8.0_build_66` under
`crates/plugin/vendor/vst3-sdk/`. `tutti-vst3-host` compiles its own reference
plugin, `audio-probe`, against them — which is what lets the VST3 suites run on a
bare checkout instead of needing `VST3_SDK_DIR` to point at an external SDK. The
build script asserts with `git submodule update --init --recursive` when they are
missing, and CI passes `submodules: recursive`.

## Linux system deps

`tutti-cpal` needs ALSA headers. Without them every build that reaches
`alsa-sys` dies in its build script, which reads as a cargo failure but is not
one:

```bash
sudo apt-get install -y libasound2-dev pkg-config   # Debian/Ubuntu
sudo dnf install -y alsa-lib-devel pkgconf-pkg-config  # Fedora
```


## Workspace Structure

One workspace, rooted at the repo root. 33 members, grouped by subsystem.

```
crates/
  tutti               # THE BEVY-FREE UMBRELLA. Re-exports only — see below.
  bevy-tutti          # THE BEVY UMBRELLA. Adapter + engine re-exports; the only
                      #   Bevy-mandatory member.
  core/
    tutti-core        # Audio graph runtime (Net, Transport, Metering, PDC)
    tutti-types       # Engine value vocabulary + io::{AudioIn, AudioOut, pump}
    tutti-cpal        # Device layer: CPAL stream, RT callback, driver lifecycle
    tutti-io          # Live I/O edge: mic monitor node, WavOut, Recorder.
                      #   Device-free, so tutti-cpal depends on it, not vice
                      #   versa. Peer of tutti-export (the offline edge).
    tutti-mod         # Pure modulation (audio-free mod matrix, curves)
    tutti-node        # Planar block buffers + the routing/contract arithmetic
    tutti-export      # Offline rendering and export
  dsp/
    tutti-nodes       # DSP nodes (LFO, dynamics, convolution, automation).
                      #   Infallible — it exports no error type at all.
    tutti-spatial     # The engine's only geometry. Two renderers, each named for
                      #   its ALGORITHM, not the category: `vbap` (speakers, N
                      #   out) and `hrtf` (headphones, 2 out). Each owns its error
                      #   type — there is no crate-level `Error`, and nothing is
                      #   called `SpatialPanner`. Depends on tutti-nodes; the
                      #   arrow runs geometry → DSP and never back.
    tutti-sampler     # Sample playback (streaming, audition)
    tutti-polysynth   # Polyphonic subtractive/wavetable synth
    tutti-soundfont   # SoundFont (.sf2) playback. Its own crate, not a polysynth
                      #   feature: a .sf2 player shares no voice engine with a
                      #   subtractive synth.
    tutti-analysis    # Audio analysis (waveform, transient, pitch)
  midi/
    tutti-midi-types    # MIDI value types (MIDI 2.0 / UMP native)
    tutti-midi-runtime  # MIDI engine (routing, allocation, expression)
    tutti-midi-hardware # OS MIDI I/O, native UMP (CoreMIDI / ALSA seq-UMP).
                        #   No features — the crate IS the OS edge. midir is gone.
    tutti-midi-file     # SMF + MIDI 2.0 Clip File codecs. OS-free, so a consumer
                        #   that only reads .mid does not link CoreMIDI.
  plugin/
    tutti-plugin        # Plugin hosting core
    tutti-plugin-types / tutti-plugin-server
    tutti-shm-model     # Test-only loom model of tutti-plugin's shm header
                        #   protocol. A separate crate because `--cfg loom` is
                        #   global and breaks a transitive dep of tutti-plugin.
    formats/            # vst2-host / vst3-host / clap-host / au-host
    vendor/vst-tutti    # Vendored VST2 bindings
    vendor/vst3-sdk     # The three Steinberg submodules
  vendor/
    fundsp-tutti                  # Vendored FunDSP
    rustysynth-tutti/rustysynth   # SoundFont synth (inner crate is the member)
    tutti-clap-test-plugin
```

The two vendored forks (`fundsp-tutti`, `rustysynth-tutti`) are excluded from
clippy and rustdoc in CI: they are third-party code we do not restyle, and they
carry pre-existing lint debt that is not ours to pay.

## Unit Types (MANDATORY)

`tutti_types::value::units` defines the engine's measurement vocabulary. **Use
the unit newtype, never the bare float**, for any quantity one covers — in struct
fields, function signatures, `Param<U>`, and return types alike.

The roster: `Hz` · `Seconds` · `Db` · `Amplitude` · `Mix` · `Feedback` · `Depth` ·
`Drive` · `Spread` · `StereoWidth` · `Bpm` · `Beat` / `BeatDuration` ·
`Semitones` / `Cents` · `Azimuth` / `Elevation` / `ArcDegrees` · `Phase` /
`PhaseIncrement` / `Radians` · `SamplePosition` / `Samples` · `SrcRatio` /
`PlaybackRate` / `StretchFactor` · `CompressionRatio` / `Q` / `Resonance` ·
`Confidence` / `Correlation` / `Pan` · `SampleRate` (in `fundsp-tutti`).

The last three are **measurements** — values the engine reports back — as opposed
to the controls above them. That is why `Correlation` and `Pan` are not `Depth`
and `Confidence` is not `Mix` despite the coinciding ranges: swapping a reading
for a control is silent, and the compiler should refuse it. Same split `Q` and
`Resonance` make.

**Operators are opt-in per type, and every omission is deliberate.** `Db * 2.0`
squares the amplitude; `Beat + Beat` has no origin; `Azimuth` has no ordering
because a circle has no ends. Before adding an operator, read the omission ledger
in `units.rs`'s tests — if the operator is listed there, the answer is a named
method, not an `impl`.

**An omission must ship with its replacement.** This rule was paid for: `Degrees`
omitted `Ord` and `Add`, named `shortest_arc_to`/`wrap_signed` as the
substitutes, and never wrote them — so call sites escaped to raw `f32` and
shipped two angular bugs. If you remove an operator, write the method that
replaces it in the same change.

**Convert with the named converters**, never by hand: `Db::to_amplitude`,
`Seconds::to_samples` (and `_floor` / `_ceil` — the rounding is in the name
because allocation, measurement and counting want different answers),
`Cents::to_semitones`. Hand-rolling `x.get() / 100.0` on a scalable unit compiles
and returns the *wrong unit's* value.

**Where the types stop** (do not migrate these): C ABI / IPC / WIT boundaries,
and any site whose precision the unit cannot carry — `Seconds` is f32, so SMPTE
timecode and hour-long render durations stay f64. When you stop, say why in a
comment.

If no existing type fits a quantity, add one rather than falling back to a float
— but only if it has a distinct range *or* a distinct algebra. Two names for the
same behaviour is not a type.

## Publishing to the Audio Thread (MANDATORY)

Non-scalar state handed from a control thread to the audio thread — a routing
table, a `MeterMap`, a coefficient set — goes through
`tutti_types::{RtPublish, RtRef}`. Scalars keep using `Param<U>`.

**The invariant: the audio thread never holds an owning handle to published
state.** `read()` returns an `RtRef` — a borrow, `!Send`, lifetime-tied to the
cell, with no way to get an owning `Arc` back out. `publish()` is control-thread
only: it blocks until in-flight readers are done with the outgoing value, then
frees it *there*. (That is `ArcSwap::store`'s behaviour — there is no
`wait_for_readers` function to grep for.)

This exists because an owning read (`ArcSwap::load_full`) on the audio thread can
leave the callback holding the last reference to a retired value and free its
`Vec`s inside the block. It is not a hypothetical — `ClickSettings::meter` did
it, and the `assert_no_alloc` test meant to catch it was single-threaded and
published outside the gate, so it passed either way. **Don't try to pin this
property with a no-alloc test**; the hazard is a race, and sampling schedules
cannot exhaust one. That is why the guarantee lives in the return type.

Rules:
- **Read once per block, never per sample.** The read is a thread-local lookup
  plus two `SeqCst` loads — far heavier than the atomics beside it.
- **Never park an `RtRef`** in a struct field or hold one across blocks (now a
  compile error rather than a review rule).
- **Avoid nested reads** — loading a value, then loading another through it.
  Guards occupy a small number of per-thread fast slots; exceeding them silently
  drops to a slower writer-coordinating path. (The exact count is `arc-swap`'s
  internal detail, not a tutti invariant — don't code against a number.)
- **Never `publish` from the audio thread** — it stalls the callback *and* frees
  inside it.
- Nullable hot-swap trait-object slots (`SharedReader`, `InputSlot`, `Midi::out`)
  stay on `ArcSwapOption`; `RtPublish` doesn't model them. Control-thread-only
  cells (device enumeration in `tutti-midi-hardware`) stay plain.

`RtPublish` wraps `ArcSwap` today. That makes RT deallocation very unlikely and
bounded, not *impossible* — a guard whose debt a writer settles concurrently
degrades into an owning reference. Making it structural means an `AtomicPtr` +
retirement queue inside `RtPublish`, with no call site moving. Keeping that
option open is much of why the wrapper exists.

## Two buffer vocabularies — edges vs graph nodes

The engine has **two** buffer interfaces, and which to use is decided by *what
the buffer is for*, not by whether the width is known at compile time. Nothing in
the engine fixes a channel count in a type any more.

- **`AudioIn` / `AudioOut`** (`tutti-types::io`) — engine **edges**: mic in, WAV
  out, `TapIn`, `FileIn`. Both take a **flat interleaved `&[S]`** and report
  their width as a runtime `ChannelLayout` from `layout()`. **Every count on this
  boundary is denominated in FRAMES** — `poll_into` and `pump` return a
  `Samples` (the frame count), `write` receives
  `frames.interleaved_len(layout)` samples. That rule is the whole risk surface: a consumer
  compares a returned count against a loop range or a file position, both in
  frames, so leaking samples makes a 6-channel looped clip wrap at a sixth of its
  length and present as "the loop points are wrong".
- **`AudioUnit::process(BufferRef, BufferMut)`** — anything that is a node in the
  graph.

**The const-generic width was a mistake, and its removal is settled — do not
reintroduce it.** `AudioIn<S, const CH: usize>` put the frame width in the type.
A const parameter can only carry a width that is a property of the *code* (a
stereo device, a stereo ring); it cannot carry one that is a property of the
*data*. The sampler hit that wall first: `WaveIn`/`RegionOut` **dropped** their
`AudioIn`/`AudioOut` impls in `50c02cd6` precisely because "those traits fix frame
width as a const parameter, which cannot carry a runtime width", and hand-rolled
the identical slice-plus-stride shape instead. That was evidence *against* the
const, not for it. `tutti-sampler`'s `RegionOut::push_interleaved` /
`RegionReader::read_into` is the proven convention the traits now generalize —
flat slice in, runtime `channels` field, frames out.

**That drop was temporary, and half of it has been undone — do not cite it as
current state.** Once the width went runtime, `RegionOut` **re-adopted
`AudioOut<f32>`** (`d79f2621`); the impl is live in `butler/prefetch.rs`. So the
sampler is not an example of a subsystem the traits cannot reach. Two things to
carry from how that landed:

- **The re-adoption is additive.** `write` must return `()`, but the refill path
  needs the landed frame count to advance `file_position`, so `push_interleaved`
  stays public and `write` is written in terms of it.
  `write_interleaved_reversed` / `set_decoder` / `file_position` have no trait
  counterpart at all — reverse refill is butler policy, not something every sink
  can do. Forcing those through `AudioOut` would produce trait methods whose
  meaning depends on which tier you are in, which is the failure that got
  `ClipReader` deleted.
- **`WaveIn` stays inherent, and the width is no longer the reason.** Its reason
  is now `ON_EMPTY`: `fill_interleaved` always fills the whole buffer and returns
  `out.len() / ch` unconditionally, so it never returns a short count and neither
  verdict is true of it — `EndOfStream` is a promise about the *first* zero (it
  pads silence forever past the end, so a consumer would never stop) and
  `Starved` promises a producer that will catch up (the `Wave` is resident; a
  retry returns the same silence). A third "unbounded — pads rather than ending"
  variant would fit it, and is the wrong trade: a case every existing consumer
  must handle, in `tutti-types`, to describe a source whose count is already
  constant. A source that never returns a short count is not answering the
  question `AudioIn` exists to ask. Full argument on the type in
  `butler/io/wave_io.rs`.

**A sampler *voice* is a graph node and not an edge — the split above decides it,
not the width.** `MemorySource` implements `AudioUnit` (`inputs() = 0`,
`outputs() = channels.count()`) and will not grow an `AudioIn` impl. Three
reasons, none about channel count: `poll_into` has nowhere to carry
`offset_in_block`, which the placed path needs because a transport advances once
per *block* (drop it and a placed clip emits DC across the block); `ON_EMPTY` has
no honest value, since a placed voice outside its window fills zeros and then
sounds again when the playhead re-enters; and `AudioIn` deliberately carries no
rate/length/seek vocabulary, so `window_position`, `read_rate` / `window_rate`,
loop wrap and `rebind_offline` would all stay outside the trait anyway. It also
renders into fundsp's **planar** `BufferMut`, so an interleaved impl would be
de-interleaved right back. The sampler's genuine edge is the butler ring, and
that is the thing wearing the trait.

Going runtime cost one compile-time guarantee: "a stereo source cannot feed a
6-channel sink" was a type error, because the two `CH`s had to unify. Per the
units rule that **an omission must ship with its replacement in the same
change**, it was replaced by two runtime checks, both landed together: `pump`
carries a `debug_assert_eq!` on the two layouts, and `tutti_io::Recorder::start`
— the one place both endpoints are in scope before a frame moves — returns an
error. Neither is optional bookkeeping; deleting either re-opens the hole.

`AudioIn` also carries `const ON_EMPTY: OnEmpty`, which is **orthogonal to
width** and survived the change untouched. It exists because the traits unify
live and finite sources, and a 0-frame poll means "not yet" for one and "never
again" for the other. That const is the *price* of the unification, not a free
win — a new impl must state which it is, and getting it wrong ends a recording
milliseconds in with no error anywhere.

**Adding a trait, when the type system is the point.** The units rule ("distinct
range *or* distinct algebra; two names for the same behaviour is not a type") has
a trait analogue worth applying: a trait earns its name if you can state its
boundary in one sentence without "and". The plugin capability split (`PluginMeta`
/ `PluginAudio` / `PluginParams` / `PluginState` / `PluginEditorHost`, reassembled
by the blanket-impl `PluginInstance`) passes — each answers a *different* "why is
this not just a fundsp node?", which is why they are five doc comments and not
one. The failure mode to watch is reaching for a **compile-time** guarantee where
the quantity is genuinely runtime, which is exactly how `AudioIn` ended up with a
vocabulary it could not cover.

## Wiring is declared, not called

`spawn_audio_node` adds an *unwired* node. `AudioSources` on a sink names what
feeds each of its input ports; the `MasterSources` resource names what feeds each
global output channel. The rebuild resolves entities → `NodeId` each frame (a
stored id goes stale on crossfade) and diffs against `Net::source` /
`output_source`, so the adapter keeps no shadow state.

Keying on the *sink port* is what makes fan-in unrepresentable: `Net` holds one
source per input port, and so does the declaration. Summing is a node's job —
`Net` has no summing bus. The old `AudioFeedsTo` edge component is deleted; it
kept a tracked map to know what to disconnect.

`bevy_tutti::graph` owns the `GraphReconcileSystems` set hierarchy —
`Spawn → Params → Despawn → Compensate → Commit` — one file per duty (`schedule`,
`spawn`, `despawn`, `commit`, `wire`, `param`, plus `io`, `metering`, `tap`,
`transport`, `plugin`, `resources`). `AudioGraphRes` is the graph handle
(fundsp's `Net`, no `Deref` so the mutate/commit boundary stays visible),
`AudioConfig` the sample-rate / channel config, `GraphDirty` the per-frame
commit-coalescing flag. Node removal is an `On<Remove, AudioNode>` observer, not a
despawn system.

Params are `AudioParam<U, const P: u16>` — one generic component per scalar,
registered with `App::add_audio_param::<U, P>()` and written through `Net::set` in
the `Params` phase. `AudioNode` is the only thing `tutti-core` gates behind its
`bevy` feature: one `derive(Component)` on one struct.

## Testing Policy

- **NEVER simplify, skip, or modify tests to make them pass** — fix the
  underlying bug.
- **Always report failing tests and their root causes** — don't silently remove
  tests.
- **Fix bugs, don't work around them** — tests reveal broken functionality; fix
  the functionality.
- **Mutation-test new tests.** Break the thing the test covers and watch it fail.
  A test that cannot fail is worse than no test: it claims coverage it does not
  have. When a property genuinely cannot be covered here, say so in a comment
  rather than writing an assertion that always passes.

## Check the constraint before designing around it

Before writing anything that *works around* an obstacle, **name the obstacle in
one sentence and then verify it**. Read the `Cargo.toml`, grep for the type, run
the probe — it takes under a minute and has been wrong more often than right.

Two specific traps, both paid for:

- **An existing dependency edge is not evidence it points the right way.**
- **An indirection whose only job is to avoid a change is the finding, not the
  design.** A cache to dodge a signature change, a resolver to dodge a
  dependency, a wrapper crate to dodge an import — make the change instead.

When the constraint *is* real: derive at the point of use rather than mirror and
reconcile. A second owner of a value something else already owns needs
invalidation, and the invalidation always has a case it cannot see.

## Consumers, and the two pins that must stay in step

The engine is consumed as a pinned git dependency — `dawai` is the first such
consumer. Two things must not drift between this repo and a consumer that also
names them directly:

- **`audio-automation`'s rev.** Two revs are two disjoint copies of `Curve`, and
  the error surfaces at the *consumer's* seam as `expected Curve, found Curve`,
  nowhere near here.
- **The bevy 0.19 line**, for anything that touches `bevy-tutti`.

Both are declared in this repo's `[workspace.dependencies]`.

## Platform status, and what it cost to get there

**All three platforms pass, and Windows is the one to know about.** Linux,
macOS and Windows are green in CI along with every static gate. Windows had
*never been compiled* before the extraction — the app repo's CI ran from a root
that excluded this workspace — so its first run reported 69 failures that were
accumulated reality, not regression. Getting to zero turned up six causes, and
**not one of them announced itself**; each returned a plausible wrong answer
rather than an error, which is why they survived so long. They are worth
knowing because the same shapes recur:

- **`interprocess` refuses a receive timeout on a named pipe** —
  `no_timeouts()`, unconditionally, in its source. Tolerating that left every
  control read blocking inside the syscall while the deadline was only checked
  *between* syscalls, so a budget of any size was unenforceable. The fix is
  `PeekNamedPipe`: ask how many bytes are buffered, and only read when the
  answer is "some". See `util::transport::control`.
- **`set_nonblocking` is not the way out, and was tried.** It took Windows from
  37 failures to 47. `PIPE_NOWAIT` makes `ReadFile` report *success with zero
  bytes*, which this transport reads as EOF, so a quiet peer was reported as a
  crashed one. The reasoning is kept at `Wakeup::arm` so nobody spends the round
  trip again. `PeekNamedPipe` is safe where that is not because it answers "is
  there data" rather than "give me data now", and so cannot confuse silence with
  death.
- **`std::getenv` does not see `SetEnvironmentVariableW`.** The MSVC CRT keeps
  its own copy of the environment, built at CRT startup and changed only by
  `_putenv`. A Rust test calling `std::env::set_var` therefore sets a variable
  that a C++ plugin in the same process cannot read. This made twelve
  misbehaviour tests each fail on their own subject rather than on the cause.
  `GetEnvironmentVariableA` reads what Rust wrote.
- **A VST3 UID has two byte layouts.** Windows builds the SDK with
  `COM_COMPATIBLE 1`, laying the first two words out like a COM `GUID`. A
  hardcoded `[u8; 16]` is therefore one platform's spelling of a
  platform-independent id, and passing the wrong one returns `None` — exactly
  what an unmapped id returns. Build UIDs with `inline_uid(l1, l2, l3, l4)`.
- **`$HOME` is a Unix convention**; Windows sets `USERPROFILE`.
- **A `.vst3` bundle on Windows contains more than the module.** MSVC drops
  `.lib`, `.exp`, `.pdb` and `.ilk` beside it, and `read_dir` order is
  arbitrary, so "take the first file" hands the loader an import library and
  reports a healthy plugin as corrupt. See the bundle rule below.

**One live platform difference remains, and it is not a bug.**
`tutti-core`'s `render_is_bit_identical_to_the_audionode_era` is gated off MSVC.
The digest pins this crate's DSP against a refactor; it cannot pin it across C
runtimes. Every operation in `generate_click` is one IEEE-754 requires to be
correctly rounded *except* `sin`, which is libm quality-of-implementation and
differs in the last ulp between MSVC's CRT and glibc. The portable properties
are asserted everywhere.

## Check Windows before you push, not after

**You do not need a Windows machine, and both halves work on a stock Linux
box.** This is the single highest-leverage thing in this file: every Windows fix
that was checked this way landed first try, and the one that was not cost a
regression.

```bash
# Rust — typecheck and lint the real Windows cfg paths.
rustup target add x86_64-pc-windows-msvc          # once
cargo clippy -p tutti-plugin --all-targets --target x86_64-pc-windows-msvc -- -D warnings

# C++ — zig ships a complete windows.h, so the probe and any SDK header
# compile for Windows with no MSVC and no mingw.
zig c++ -target x86_64-windows-gnu -std=c++17 -c -o /tmp/o.obj tu.cpp -I <sdk>
```

Neither runs the code, so they catch *compile*-shaped mistakes only — a
semantic claim about the MSVC CRT still needs CI. Check that the branch you
meant was taken: `nm` the object and look for the symbol (`GetEnvironmentVariableA`
rather than `getenv`), because a `#[cfg]` or `#ifdef` that silently excluded your
code also compiles clean.

## One bundle walk, not nine

**`tutti_plugin_types::bundle` is the only place that knows the
`Contents/<arch>/` layout.** Nine near-copies of that walk had drifted apart:
four omitted `aarch64-linux`, all of them omitted the three `arm64*-win`
spellings the production resolver handled, and one omitted Windows entirely.
`subprocess::bundle`'s own doc comment had predicted exactly this — "a test
helper that hardcodes its own copy passes on ARM while production fails" — and
was ignored nine times.

Two entry points, and the difference is deliberate:

- **`native_module_in_bundle`** probes only what *this* target can load. Hosts
  want this: a foreign-arch module fails at `dlopen` anyway, so "no module for
  this architecture" is the more useful answer.
- **`any_module_in_bundle`** probes every arch directory and falls back to
  enumerating, skipping linker by-products. Corpus scans want this, so a
  cross-built fixture "lands in the slower branch instead of a wrong answer".

Do not add a tenth copy. If a bundle layout surprises you, fix it there and
every caller gets it.

## Extraction residue

This tree spent three and a half months inside the dawai app repo with **no
working CI** — the workflow it carried named crates that had been renamed or
deleted (`tutti-midi`, `tutti-units`) and tested a property that was dropped
(`no_std`), so it did not fail, it just stopped testing anything. Expect drift in
comments and docs, and treat it as a bug when you find it:

- **The engine must not reference `dawai` at all.** All of `crates/` is now
  clear of it — the one remaining mention is the provenance note at the top of
  `crates/bevy-tutti/Cargo.toml`, which is deliberate. What is left sits in
  `docs/`: historical audit records (`fundsp-fork-audit.md`,
  `fundsp-comparison.md`, `unit-adoption-backlog.md`) that surveyed what the app
  workspace reached for, plus a few plugin-hosting plans. Those are *dated
  records of an audit*, so rewriting their findings would falsify them; leave the
  findings and fix only what claims to be current.

  When you do clear one: delete the reference or restate the point in engine
  terms ("a host", "a consumer"). Do **not** "update" it to a new app-side path —
  that re-creates the coupling the extraction removed.
- **Dead crate names**: `tutti-units` is `tutti-nodes` (`91bb7540`),
  `tutti-synth` was split into `tutti-polysynth` + `tutti-soundfont`
  (`6bc0e77b`), `tutti-midi` was split into the four `midi/` crates
  (`786a4c24`).
- **There are two umbrellas, and `tutti` holds no code.** A Bevy app depends
  on `bevy-tutti`; a headless consumer depends on `tutti`.

  The history matters, because a `tutti` package was deleted once and the
  reason is easy to misread. The one at `9c75ec54` was the **workspace root
  package** — the root `Cargo.toml` carried both `[workspace]` and
  `[package] name = "tutti"` — and it held real logic: `TuttiEngine` ("a flat
  bundle of owned subsystems returned from the builder"), `TuttiGraph`,
  `TuttiEngineBuilder`, `TuttiDriver`, `audio_io`, `midi_export`, an error
  type and a `no_std` lattice. `0a4adf68` moved that logic into
  `bevy-tutti/src/engine/` and `4b5bd2fd` deleted the package. **The problem
  was two stacked umbrellas where the lower one owned logic the upper one
  needed — not a crate that re-exports other crates.**

  So `crates/tutti` exists again, with one rule: **every item in it is a
  `pub use`.** No builder, no engine bundle, no error aggregation. If
  something wants to live there it belongs in the crate that owns the thing
  it wires — device bootstrap in `tutti-cpal`, graph logic in `tutti-core`.
  `crates/tutti/tests/no_logic.rs` enforces this by reading `lib.rs`, and
  `scripts/check-canonical-paths.sh` additionally requires every whole-crate
  re-export there to be aliased (`pub use tutti_core as core`), so
  `tutti::tutti_core::…` is unspellable.

  `bevy-tutti` does **not** depend on `tutti`, deliberately: it would keep
  its direct edges anyway for their `bevy` features, ending with two edges to
  each crate.

  There is no `tutti-plugin-host` package either — `bevy_tutti::plugin_host`
  is a *module*.
