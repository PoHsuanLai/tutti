# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **File I/O moved from `tutti-core` (the fundsp fork) to `tutti-io`.** Old →
  new: `tutti_core::Wave` → `tutti_io::Wave`, `tutti_core::FileIn` →
  `tutti_io::FileIn`, `tutti_core::{WaveAsset, WaveMetadata, WaveError}` →
  `tutti_io::{WaveAsset, WaveMetadata, WaveError}`,
  `tutti_core::{can_decode, decodable_extensions}` →
  `tutti_io::{can_decode, decodable_extensions}`. Through the `tutti` umbrella
  they are `tutti::io::…`, which is now present with any codec feature as well
  as with `audio-io`. Decoded samples are bit-identical
  (`tutti-io/tests/decode_golden.rs` pins them against fixtures in
  `assets/audio/`).

  Features moved with them. `tutti-core` has no `wav`/`flac`/`mp3`/`ogg` or
  `bevy_asset` features any more; name `tutti-io/wav` etc., and
  `tutti-io/bevy` for `WaveAsset`'s `Asset` derive. `tutti-sampler`,
  `bevy-tutti` and `tutti` keep their codec features and forward them to
  `tutti-io`. `tutti-sampler` now depends on `tutti-io`.

  The API was trimmed to what the engine calls. `Wave` keeps `new`,
  `with_capacity`, `zero`, `from_samples`, `sample_rate`, `channels`,
  `channel`, `channel_mut`, `at`, `set`, `len`, `is_empty`, `duration`, `load`
  and `probe_metadata`. `Wave::push(frame)` (a typenum tuple or a broadcast
  scalar) is `Wave::push_frame(&[f32])`, one sample per channel. Gone, with no
  engine caller: `render*`, `filter*`, `multifilter*`, `resample_fir`,
  `fade*`, `normalize`, `amplify`, `amplitude`, `retain`, `append`, `mix*`,
  `set_sample_rate`, channel insert/remove/push, `load_slice*`,
  `load_track*`, `load_with_progress`, `load_with_peaks`, and the WAV writers
  (`WavOut` is the engine's sink). `FileIn::open(path, track)` is
  `FileIn::open(path)`; every caller passed `None`. The fork's own `Wave` stays
  for its internal nodes and no longer decodes; its `files`/codec/`bevy_asset`
  features, symphonia dependency and `pub use symphonia` are gone.

- **Every count on the `AudioIn`/`AudioOut` edge is a `Samples`, not a
  `usize`.** `AudioIn::poll_into` and `pump` return `Samples` — the engine's
  existing frame count, the same type latency and `Seconds::to_samples` use —
  and so do `tutti_io::PumpPass::Wrote`, `ManualPump::pump_until_dry` and
  `tutti-sampler`'s `RegionOut::{push_interleaved, write_interleaved_reversed,
  write_space, capacity}`. `AudioPump::start`'s `capacity` is a `Samples`.
  The frame ↔ slice-length crossing is named: `Samples::interleaved_len(layout)`
  and `Samples::from_interleaved_len(len, layout)`.

  Migrating an implementor: return `Samples(n)` instead of `n`, or
  `Samples::from_interleaved_len(out.len(), self.layout())` for "the whole
  buffer". A caller comparing against zero uses `.is_zero()`.

- **`ChannelLayout` no longer implements `Default`.** It defaulted to `STEREO`,
  and that guess leaked through every `#[derive(Default)]` on a struct holding
  one — which is how `Topology::default()` once declared two global inputs.
  Name the width: `ChannelLayout::STEREO`, `::EMPTY`, `::from(n)`. Knock-on:
  `tutti_vst2_host::PluginInfo` no longer derives `Default`; `EncodeConfig`
  (stereo), `PeakBlocks` and `PeakAccum` (empty) have hand-written defaults
  that name their width.

- **`tutti-midi-io` is renamed `tutti-midi-hardware`.** The crate is the OS port
  edge and nothing else; `-io` invited the reading that it also covered file I/O,
  which is `tutti-midi-file`'s job. `bevy-tutti`'s `midi-hardware` feature now
  shares the crate's name, which is the intent — the feature does nothing but
  pull the crate in.

- **`tutti-midi-hardware` no longer re-exports the SMF / Clip File codecs.** The
  `tutti_midi_io::smf` and `::clip` spellings are gone, along with the
  `tutti-midi-file` dependency behind them. Under the old name passing the file
  API through was merely odd; under `-hardware` it was a category error, since a
  file is not a device. Depend on `tutti-midi-file` directly — `bevy-tutti` and
  the known hosts already did, and nothing used the re-export.

  `Error::File(tutti_midi_file::Error)` went with it. It existed only to make
  the re-exported codecs share this crate's `Result`, and nothing ever
  constructed or matched it; a consumer that reads files owns that error itself
  (`bevy_tutti::midi::file::MidiFileError::Smf`).

- **The MIDI delivery traits split on arity.** There are now four, in two pairs,
  one pair per direction — keeping the `In`/`Out` axis `AudioIn`/`AudioOut` set:

  |          | one stream | one of many, by id |
  |----------|------------|--------------------|
  | **push** | `MidiOut`  | `MidiRouter`       |
  | **pull** | `MidiIn`   | `MidiUnitIn` *(new)* |

  `MidiIn` previously spanned both columns, carrying a `unit_id` for the fan-out
  half. Three of its four implementors treated that parameter as dead weight —
  two checked it against an id they already owned, one ignored it — and the
  hardware edge had to be polled with a `MidiUnitId::new(0)` **sentinel**. Since
  `0` is a real id, a per-unit source installed at that seam would silently have
  received the entire hardware stream. That install is now a compile error.

  Migrating an implementor: if it ignored `unit_id`, implement `MidiIn` and drop
  the parameter (`poll_into` → `poll_block`). If it dispatched on it, implement
  `MidiUnitIn` (`poll_into` → `poll_unit`). `MidiInPort::install` and
  `MidiPreBlock::set_input` now take the respective trait.

  `MidiReceiver` no longer implements a read trait at all. Its inherent
  `poll_into(&self, out)` is unchanged and is what every caller already used —
  the trait impl's only added behaviour was a check its sole caller deliberately
  routed around.

- **`MidiOut::queue` returns the accepted count** (`()` → `usize`). A sink that
  cannot fail returns `events.len()`; a short count means the rest were dropped.

  This fixes a live bug: `MidiSession::send` returned `events.len()` whenever an
  output was open, under a doc promising an *accepted* count, so a device
  refusing every event reported full success. Both OS backends now count what
  they wrote and stop at the first failure, so the number names an unbroken
  prefix rather than a subset with holes.

  **The return value's meaning changed without its type changing** — a consumer
  treating `send(&e) == e.len()` as "connected" now correctly gets `false` when
  the device is refusing.

- **`InputConnection` carries no methods.** Its doc said exactly that and then
  declared `fn endpoint()`, which had no callers — the id is already the key of
  the map connections are stored under. The trait remains as the RAII marker its
  doc describes: dropping it closes the port.

- **`tutti-midi-io` is native-UMP; `midir` is gone.** MIDI reaches the wire as
  UMP words on every supported platform, so MIDI-2-only messages — per-note
  controllers, per-note pitch bend, JR Timestamps — survive. They were
  previously dropped in silence by `to_midi1_bytes` returning `None`.
  - macOS: CoreMIDI, no new dependencies.
  - Linux: ALSA UMP sequencer, needs alsa-lib ≥ 1.2.10. `build.rs` probes the
    version and degrades to a stub with a `cargo:warning` below that floor, so
    older distros still build.
  - Windows: a stub. No `windows` crate version binds Windows MIDI Services.
  - JR-stamped output used to be macOS-only, because `UmpOutRes` wrapped a
    CoreMIDI type. It now takes an erased sink, so every backend gets it.

### Fixed

- **`FileIn` streamed every Ogg Vorbis file as empty.** Vorbis's first packet
  decodes to zero frames, and the streamer took any zero-frame packet for
  end-of-stream, so a streamed `.ogg` clip played silence from its first block
  (and a seek's preroll stopped early). `tutti_sampler::probe` still reported
  such files streamable, since the container carries a frame count. Zero-frame
  packets are now skipped; a streamed read now yields the same samples as
  `Wave::load`, bit for bit, for every fixture in `assets/audio/`.

- **Four defects in `Routing::route`, the arithmetic plugin delay
  compensation is computed from.** Each was inert in the tree as it stood,
  because every in-tree caller happened to pass the argument that makes the
  wrong answer and the right one coincide — which is exactly why they
  survived, and why a fix now costs nothing and later costs a version.

  - **`Routing::Arbitrary` added the node's own latency once per input**, so
    a node's reported latency depended on the *order its input channels were
    wired*: sources at 0 and 10 with a node latency of 5 gave 10 one way
    round and 5 the other. `route` now folds the inputs with no extra
    latency and adds the node's own once at the end. Live case:
    `fundsp-tutti`'s `resynth.rs`, the only caller passing a non-zero value.
  - **`Routing::Generator` was unreachable for zero-input generators**, i.e.
    all of them — the empty-input guard ran before the match, so `noise`,
    `envelope`, `wave`, `sequencer`, `shared` and `ring` all reported
    `Unknown`. Handled ahead of the guard now. Every caller passes `0.0`
    today and `tutti-export` does `unwrap_or(0.0)`, so nothing observable
    changes; the first generator to declare a non-zero latency would have
    had it silently dropped.
  - **`Routing::Join` panicked on two shapes**, both reachable from
    `AudioUnit::latency`/`response` rather than from audio processing: more
    outputs than inputs indexed past the end, and zero outputs divided by
    zero. It now answers an empty frame for zero outputs (as every other
    variant does) and leaves un-fed outputs `Unknown`.
  - The same guard also swallows `Routing::Reverse`'s `assert_eq!`. **Left
    as is, deliberately**: an empty frame means "no signal information", not
    a width mismatch, and making the assert fire there would put a new panic
    on the graph-commit path. Documented at the guard and at the test.

- **`PolySynth` was capped at 16 voices for a removable reason.**
  `PolySynth::new` rejected any `max_voices` above 16 because
  `finished_indices` was a `SmallVec<[usize; 16]>` and a spill would have put
  a `malloc` in the audio callback. The reasoning was sound and the tool was
  wrong: a `Vec` sized to `max_voices` at construction and only `clear()`ed
  never reallocates, which is the same guarantee with no ceiling. 16 voices
  is low for a sustain-pedal part, and the cap is gone; `smallvec` is no
  longer a dependency of this crate.

  The steady-state no-alloc test now runs at **64 voices**, which makes it
  strictly stronger — at 16-of-16 the collection was always inline, so it
  passed whether or not the drain touched the heap.

  A second test covers what that one structurally cannot: it warms the
  *thread* and gates a *fresh instance*, including a `Clone` (the shape
  `Net::commit` hands a running callback). Both construction sites are
  mutation-covered; an earlier draft covered only `new` and the `Clone`
  mutation passed against it.

- **Found while doing the above, not fixed: the first block on a cold thread
  allocates.** A `PolySynth` that has never been processed, with no MIDI and
  no active voices, allocates 128 bytes on its first `process` — and it is
  **per thread, not per instance** (a fresh synth on an already-used thread
  allocates nothing). The path is `poll_midi_events_sorted` ->
  `MidiInPort::poll` -> `self.source.load()` on an `ArcSwapOption`;
  `arc-swap` initialises its per-thread fast slots lazily. It lands on the
  **first callback of any new audio thread**, and `CpalDriver::restart`
  makes a new one on every device switch. The whole `rt_no_alloc` suite was
  blind to it because every test warmed the instance, and therefore the
  thread, before opening the gate. Recorded as an `#[ignore]`d test at
  `tutti-polysynth/tests/rt_no_alloc.rs`; the fix belongs in `MidiInPort`.

- **`Recorder` could not await a finite take's natural end.** `stop()` clears
  the run flag *before* joining, which is right for a live source and wrong
  for a finite one: a source that has not reached its end is cut off wherever
  the pump happened to be, and the take is silently truncated — a short file,
  no error anywhere. `FinalizeStatus::is_done` is set only by the
  `Drop`/`stop` shutdown, not by the thread breaking on
  `OnEmpty::EndOfStream`, so it could not be polled for this either. New
  `Recorder::wait()` joins without touching the flag, so the loop breaks
  where it was always going to. It shares `shutdown`'s join half rather than
  copying it; the one line that differs is the one that truncates. On a
  `Starved` source it blocks forever — deliberately, since a timeout would
  report a complete take with no idea whether it was one.

- **Clip-file round trips duplicated their metadata.** Tempo and time
  signature are Flex Data *events* in M2-116, so `read_clip_file` hands them
  back inside `ParsedClipFile::events` — and `write_clip_file_with_header`
  then emitted them a second time from the `ClipHeader`. Parse, write, parse
  grew a duplicate pair every cycle. Nothing errored and a reader takes the
  *first* declaration, so the file kept playing correctly while accumulating
  junk; an open-and-save loop was quietly corrupting user data.
  `write_clip_file_with_header` now **replaces** a header the events already
  carry rather than prepending to it, stripping only the leading zero-delta
  run so a mid-clip tempo change is untouched. New `ParsedClipFile::header()`
  returns the pair to hand back, and is `Some` only when the file declares
  both halves — substituting `ClipHeader::default`'s 120/4-4 for a missing
  one would write a tempo the file never claimed.

- **`reset_owners` has been a no-op, and three doc comments said otherwise.**
  Found by mutation-testing: deleting the `reset_owners()` call from a stream
  restart changes nothing. Every call in the chain bottoms out in
  `AudioThreadCell::reset_owner`, whose own doc reads "the cell pins no owner
  thread, so a device switch needs no reset" — the cell's debug check detects
  a *concurrent borrow*, not a foreign thread. `RtEventBuf::reset_owner`
  already admitted this; `AudioCallbackState::reset_owners` and
  `MotionFsm::reset_owner` still claimed "the owner checks would otherwise
  flag the new thread as an intruder". The comments are corrected. The calls
  stay — they are public API, and the property is one a future cell might
  reinstate — but nothing should be written that depends on them acting.

- **Plugin MIDI-out reached nothing.** `tutti-cpal` held an
  `Option<MidiPostBlock>` and called `run()` in the audio callback, but nothing
  anywhere *constructed* one — so the whole outbound path was assembled, tested,
  RT-safe, and unreachable. `bevy-tutti`'s engine build now creates it from the
  same routing snapshot and bus the inbound phase uses, so a node's MIDI-out is
  routed by exactly the rules a hardware input is.

  The new `MidiOutSinkRes` publishes the collection point; a host hands it to
  whatever emits (`plugin.set_midi_out(sink.handle())`). It is deliberately
  **not** installed automatically: an inbox is an *address* and costs one map
  slot, but a sink is a *routing decision* — and a plugin's MIDI-out capability
  is a per-instance negotiated fact that the node's Rust type cannot answer.

  `tutti-midi-runtime`'s new `outbound_block_path` test assembles the entire
  round trip with no Bevy in scope, pinning the path as engine-side: if it ever
  needs an adapter type to compile, the adapter has stopped being a wrapper.

### Added

- **`just check-features` / `just test-features`, and a `dark features` CI job
  — the feature-gated code nothing was compiling.**
  `cargo tree --workspace -e features -i tutti-cpal` reported only `default`:
  nothing in this workspace turned `tutti-cpal/capture`, `tutti-cpal/midi` or
  `bevy-tutti/audio-io` on, so `just test`, `just lint` and every CI job
  typechecked none of them. All of `tutti-cpal/src/mic.rs` was uncompiled, so
  were the `pre_block`/`post_block` arms of `process_audio` — the ordering that
  module's header calls "the design" — and `bevy-tutti/tests/audio_io_pump.rs`,
  twelve tests, **had never once run in this repo.** (They pass. That is luck
  rather than evidence, which is the point.) This is the same hole
  `just check-windows` exists to close and has the same failure mode: a cfg
  block nothing compiles is a cfg block nothing lints, and it rots in silence.
  Both recipes are in `just ci`. Adding a feature means adding a line there.

- **`tutti` — the Bevy-free umbrella, and it contains no code.**
  `bevy-tutti` was the only one-dependency entry point, so a headless
  consumer hand-wired a dozen path deps. `tutti-export`'s own showcase
  example names five crates; through the façade it names one, and
  `crates/tutti/examples/headless_export.rs` is that rewrite (the original
  stays put — it proves `tutti-export` is usable standalone).

  The façade also reaches strictly more than `bevy-tutti` does: that umbrella
  depends on neither `tutti-analysis` nor `tutti-node`, which is why
  `export.rs` could not have been written through it either.

  **The history is easy to misread and the docs now say so.** A `tutti`
  package was deleted once — but it was the *workspace root package*
  (`9c75ec54`: root `Cargo.toml` with both `[workspace]` and `[package]`),
  and it held `TuttiEngine`, a builder, `TuttiDriver` and the CPAL callback.
  `0a4adf68`/`4b5bd2fd` dissolved it because `bevy-tutti` needed that logic
  and two stacked umbrellas, where the lower owns what the upper needs, is
  one too many. **Re-exporting was never the problem; owning logic was.**

  So the rule is enforced, not merely written: `crates/tutti/tests/no_logic.rs`
  reads `lib.rs` and rejects any statement that is not a `pub use` or
  `pub mod` (by shape, not by keyword blacklist — a blacklist misses a type
  alias or a const), and `scripts/check-canonical-paths.sh` gains a check that
  every whole-crate re-export there is aliased, so `tutti::tutti_core::…` is
  unspellable. Both were verified by making the violation and watching them
  fail.

  `just check-bevy-free` and the CI job gain a **negative dependency
  assertion** — `cargo tree -p tutti --features full -e normal` must contain
  no bevy crate at any depth. A compile proves the crate builds; it says
  nothing about what came in with it, and the repo had no guarantee of that
  shape before. Also verified by making it fail.

  `bevy-tutti` deliberately does not depend on it: it would keep its direct
  edges anyway for their `bevy` features, ending with two edges to each crate.
  Its own `full` is otherwise transcribed unchanged, minus every `/bevy`
  forward — that difference *is* the crate.

- **Benchmarks, and the first numbers this engine has ever had.**
  Five criterion suites (`engine_render`, `audio_callback`, `polysynth`,
  `voice_pool`, `offline_render`), a `docs/benchmarks.md` baseline naming the
  machine it was taken on, and `just bench` / `bench-save` / `bench-cmp` /
  `bench-smoke`. `criterion` is unified on 0.8 in `[workspace.dependencies]`;
  `tutti-core` carried a dead 0.5 that never had a `[[bench]]` while the
  vendored fork was already on 0.8, so the tree resolved two of each.

  What the numbers say, on a Ryzen 9 9950X:
  - **~5,500 simple filter nodes** fill a 64-frame block's 1.333 ms budget,
    single-threaded, and scaling is linear.
  - **The real callback costs ~34% more than the graph render** — metering
    and the stereo fold, not the format conversion (i16 adds only 4% over
    f32). A graph-only benchmark understates the audio thread by a quarter.
  - **The phase vocoder costs 16×** the bypass path (10.2 µs → 165 µs at 8
    voices). It is by far the most expensive thing in the sampler.
  - **A FLAC export is ~98% encoder, ~2% engine.**
  - `PolySynth` was **hard-capped at 16 voices** (`FINISHED_NOTES_CAPACITY`).
    The cap is now removed — see Fixed.

  Three drafts produced *wrong* numbers before these, and the reasons are
  recorded in the bench headers because each is a trap the next person will
  hit: `max_voices` defaults to 8 so every polysynth case above 8 measured
  identically; `BufferArray<U2>` is 64 frames wide so a "512-frame" axis was
  reporting the 64-frame cost; and detuning each sampler voice by a cent put
  all but the first through the pitch shifter, making plain playback look
  105× superlinear.

  **CI gates none of it.** Runners swing 30–50% and `profile_stretch_clone`
  documents an 81× spread on a quiet machine; a flapping perf gate earns
  `continue-on-error: true` within a month and then tests nothing. The
  `bench-smoke` job proves the harnesses still *run*, and the real gate is
  the new `tutti-core/tests/alloc_budget.rs` — allocation counts are
  machine-independent, so a budget on them survives a shared vCPU.

- **The device layer has a seam, a host selector and a fault sink.**
  `tutti-cpal` had four `cpal::default_host()` calls, no device abstraction of
  any kind, and 6 tests. JACK was unreachable even with cpal's `jack`
  dependency compiled in, because `default_host()` returns ALSA regardless —
  the host has to be *named*. It now has 29 tests, none of which opens a sound
  card.

  - **`StreamDriver` / `RunningStream`, with `CpalDriver` and
    `ManualStreamDriver`** — the direct analogue of `tutti_io`'s
    `PumpDriver`/`ThreadDriver`/`ManualDriver`, and it earns the same claim:
    `CpalDriver`'s closure body is `move |data, _| block.render(data)`, so it
    is not a second implementation of the callback. `AudioEngine::from_spec`
    plus a manual driver gives a complete lifecycle — start, render, fault,
    stop, restart — with no device. `AudioEngine` holds a
    `Box<dyn RunningStream>` rather than a type parameter, for the reason
    `Recorder`'s field doc gives: a generic would push the driver choice into
    `TuttiDriver`, into `bevy-tutti`'s `NonSend`, and into every host field.

  - **`AudioHost` / `DeviceHost` / `DeviceSelector`**, and the `jack` feature.
    Every `AudioHost` variant exists on every platform on purpose — cpal's own
    `HostId` is cfg-generated, so mirroring it would make a host's config
    struct a different type per OS; an unreachable host is
    `Error::HostUnavailable` at runtime instead. `DeviceSelector::Name`
    survives the re-enumeration that invalidates an index, which is the best
    available answer while cpal 0.15 exposes no hot-plug notification.
    `just check-jack` ships with the feature rather than after it: cpal
    declares no `jack` feature of its own (it is the implicit feature of an
    optional dep in cpal's Linux/BSD target table), so the code is invisible
    to every other recipe — the same shape as the `#[cfg(windows)]` gap that
    once took Windows from 37 failures to 47.

  - **`StreamFaults`** — both error callbacks were literally `|_err| {}`, so a
    device unplugged mid-session surfaced *nowhere*: `is_running()` stayed
    true and the host went on reporting a healthy stream to a user hearing
    silence. Faults are now accumulated behind a handle taken before anything
    goes wrong (the `FinalizeStatus` shape, publication order and all), and
    `bevy-tutti`'s `AudioDeviceState` mirrors them per frame.

    **`AudioEngine::is_running()` is a behaviour change without a signature
    change**: it can now return `false` while a stream object exists, because
    it consults the disconnect flag. That was the defect, not the contract.

  - **`MicIn::open` takes the graph's sample rate.** A breaking change, and
    deliberately not offered as an opt-in overload, because an opt-in safe
    path reproduces the bug it fixes. `MicMonitorNode` does not resample — its
    `set_sample_rate` is a documented no-op resting on "the device layer opens
    the mic at the graph's rate" — and *nothing enforced that*: `MicIn` took
    whatever the input device reported while `AudioEngine` took whatever the
    output device reported, and the two were never compared. A 44.1 kHz mic on
    a 48 kHz graph drifted silently for the length of the take. Now
    `Error::SampleRateMismatch`, decided by a free `choose_input_config` that
    needs no device to test, with a paired `debug_assert` in
    `MicMonitorNode::set_sample_rate` — the two-check shape `pump`'s layout
    assert and `Recorder::start`'s error already use.

- **Coverage where a silent wrong answer reaches a user's recording.**
  The plugin subsystem carried ~1,731 tests; `tutti-io` had 27 and
  `tutti-midi-file` 13, with no integration tests and no input files of any
  kind. Those are the crates every consumer hits on day one. Now 36 and 27.

  `tutti-io` gains `tests/recorder_thread_driver.rs` — the **production**
  driver's first tests ever; every existing `Recorder` test drives a
  `ManualDriver`, so `thread::spawn`, the Acquire/Release stop handshake, the
  `PumpPass::Ended` break, `IDLE_PARK`, and `impl RunningPump for JoinHandle`'s
  "recording thread panicked" arm had never run. Plus
  `tests/tap_to_wav_roundtrip.rs`, the `AudioTap → TapIn → Recorder → WavOut`
  end-to-end that existed only as a README doctest nextest does not run, and
  the first coverage of `FinalizeStatus::error()` returning `Some` — the whole
  reason that handle carries an error rather than just a done flag.

  `tutti-midi-file` and `tutti-midi-types` gain an independent SMF
  encoder/decoder in `tests/support/`, written from the spec. `midly` is the
  wrapped dependency so it cannot be its own second opinion, and the
  alternatives do not qualify (`nodi` wraps midly, `rimd` is unmaintained);
  for MIDI 2.0 Clip Files there is no second implementation anywhere. The
  builder doubles as the fixture generator, including the malformed cases a
  committed corpus could not carry. Highest-value additions: a delta on a
  sysex must still advance the beat grid, LIFO pairing of overlapping notes,
  per-channel pairing, and — for the 1,154 hand-rolled lines of clip codec —
  "no prefix of a valid file is valid, and none panics", which sweeps every
  length bound at once.

  Every test was mutation-checked by actually running the mutation. Two drafts
  **passed** under the mutation they were written to catch and were rewritten:
  waiting for a source to drain proves nothing about the `Ended` break (a
  spinning driver still finalizes correctly), and two notes opened
  simultaneously pair identically whether or not the channel is in the key.
  Both cases are recorded in the test headers, because the next person will
  reach for the same first draft.

- **`MicMonitorNode::tick` no longer discards a frame on a short buffer.**
  It called `next_frame()` — which pops the ring — and *then* checked
  `output.len() >= 2`, so a narrow buffer consumed a captured frame and wrote
  nothing. Unreachable through fundsp, which always hands a 2-out node a 2-wide
  buffer, so this was latent rather than live; fixed because a discard is never
  the branch you want on a path whose job is not losing frames, and ordering
  the check first costs nothing.

- **`tutti_cpal::OutputBlock`** — the output callback, liftable out of CPAL.
  `process_audio` was only ever the *inner* seam. Everything around it lived
  inside the closure handed to `build_output_stream`: the `MAX_FRAMES` clamp,
  the zero-fill, the stereo metering fold, `meter_output`, and the eight-way
  sample-format conversion. Nothing but CPAL with a real sound card open could
  run any of it, which is why none of it has a test. `OutputBlock::render` is
  that body, and CPAL's closure is now `move |data, _| block.render(data)` —
  not a second implementation of the callback, the same claim
  `tutti_io::ManualDriver` makes about `PumpLoop::pump_once`.

  The `debug_assert!` on the callback size deliberately stays at the CPAL
  boundary rather than moving into `render`: it is a claim about *CPAL's*
  contract, and keeping it out is what will let a debug-build test observe the
  clamp instead of panicking before it.

- **`tutti-midi-file`** — the SMF and MIDI 2.0 Clip File codecs, split out of
  `tutti-midi-io`. Reading a `.mid` needs no OS MIDI port, and pairing the two
  behind one feature flag meant a consumer wanting only the codecs linked
  CoreMIDI unless it knew to pass `default-features = false`. `tutti-midi-io`
  re-exports the file API, so `tutti_midi_io::smf` still resolves.
- `MidiSession` replaces `MidiIo`: same job, no driver code, no background
  threads, and an output sink that is **absent** rather than silently
  discarding when nothing is connected. `send` returns the accepted count.
- `UmpCapability` records what an endpoint can carry — protocol and function
  blocks — read from the OS rather than assumed.

### Removed

- **`tutti-midi-io`'s `midi-hardware` feature.** With the file codecs split out,
  the crate contains nothing that is not OS MIDI, so the flag no longer named an
  axis: turning it off left the whole session/port surface compiled with no
  backend to drive it. Platform gating is `cfg(target_os)`, which is what
  actually decided this all along. `bevy-tutti` keeps its own `midi-hardware` —
  that axis is still real.
- The MIDI-1.0 `VirtualMidiSource` / `VirtualMidiDestination` pair, which had no
  consumers.

### Fixed

- SysEx reassembly, extracted to `tutti-midi-io`'s `Sysex7Assembler` and now
  unit-testable off the driver thread. Three bugs it had been hiding: bytes
  after the terminating `0xF7` were discarded (a device packing two dumps into
  one buffer lost the second); the buffer had no ceiling, so a lost `0xF7` grew
  it for the lifetime of the connection; and each completed message allocated
  twice on the driver callback thread. A mid-run `0xF0` now restarts the run
  rather than being kept as payload, where the 7-bit mask silently turned it
  into `0x70`.

## [0.0.1] - 2025-01-29

### Added
- Initial release of Tutti audio engine
- Core audio graph runtime with FunDSP integration
- MIDI subsystem with I/O, MPE, and MIDI 2.0 support
- Sample playback with Butler thread and time-stretch
- DSP building blocks: LFO, dynamics, envelope followers, spatial audio
- Plugin hosting for VST2, VST3, and CLAP (multi-process with crash isolation)
- Neural audio synthesis and effects (GPU-accelerated)
- Audio analysis tools: waveform, transient detection, pitch detection
- Offline audio export (WAV, FLAC)
- Real-time transport with tempo mapping
- EBU R128 LUFS metering
- Plugin Delay Compensation (PDC)
- Modular feature flags for flexible builds
- Ergonomic graph API with `pipe()`, `node_mut()`, `add_split()`, `add_join()`
- 9 comprehensive examples showcasing core features

### Architecture
- Workspace with 8 independent crates
- Lock-free audio thread design
- Framework-agnostic (works without Bevy/egui)
- MIT OR Apache-2.0 dual license

[Unreleased]: https://github.com/PoHsuanLai/Tutti/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/PoHsuanLai/Tutti/releases/tag/v0.0.1
