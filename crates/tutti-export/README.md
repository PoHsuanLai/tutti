# tutti-export

**The OFFLINE edge**: render a Tutti graph to a file, or to buffers, faster (or
slower) than real time. The `tutti` facade re-exports this crate as
`tutti::export` (feature `export`).

## What is here

The graph is *pulled* to a known frame count that `RenderConfig` fixes up front,
then resampled, dithered and encoded on the way out. The entry points, and the
names say which path a caller is on:

- `render_to_file` — render straight to disk. Streams: the pipeline pulls the
  graph one block at a time and no PCM is held whole.
- `render_to_buffers` — render into memory, as planar `Vec<f32>` planes.
- `write_buffers` — encode planes a caller already holds.
- `render_normalized_to_file` — render, measure, apply a gain, write: two
  passes, so the whole render is held in memory. It is a separate function on
  purpose — see below.

The three that render take a `Control`: a `CancelToken` they check (before
every block, between stages, during the measurement and before the file is
put in place) and a callback that is handed a `Progress`, a `Stage` and one
fraction of the whole render. `Control::none()` is a render nobody watches or
stops. A cancelled render is `Outcome::Cancelled`, with every temporary
removed and every target as it was. Encoding is WAV, FLAC, AIFF or OGG Vorbis, each
behind its own feature (`wav`, `flac`, `aiff`, `ogg`). **None is on by default**:
`ogg` links the C Vorbis library, so a consumer names the formats it writes. The
umbrellas (`tutti`, `bevy-tutti`, `tutti-session`) forward them as `export-wav`,
`export-flac`, `export-aiff` and `export-ogg`; their `export` feature implies
`export-wav` only, so `export-ogg` is the one path to C.

One `ExportConfig` serves all of them, and each reads only some of it; the
list is on `ExportConfig`.

## What this crate does not own

- **Live capture.** Its peer is the **LIVE** edge — a microphone, a
  tap, a `Recorder` (`tutti-session`) pushing blocks into a `FileOut` as they arrive. The distinction is
  structural rather than stylistic: a live capture has no total frame count (it
  ends when someone stops it), so it cannot be expressed as an export, and an
  export needs a total (to size a plan, trim latency, cap output), so it cannot
  be expressed as a live pump. Reach here to bounce a mix; reach for `Recorder`
  to record one.
- **Loudness measurement.** EBU R128 lives in `tutti-measure`, which is where
  the engine's `Config`/`State`/step analysis vocabulary is;
  `render_normalized_to_file` measures with it. The edge is acyclic —
  `tutti-measure` does not depend on this crate.
- **Threads.** Every entry point is synchronous and runs on the calling
  thread. A host that wants a render off the main thread runs it on its own
  task pool.
- **A buffering strategy.** Every format streams, because every codec library
  used here supports incremental encoding. There is no buffered-versus-streaming
  mode to pick.

## Example — bounce a graph

A config is a struct literal, so a caller states what it means and lets
`..Default::default()` cover the rest. The clock is not optional:
`FrozenClock` is how a caller *says* "this graph has no time-dependent nodes",
so forgetting a transport is a compile error rather than a silently silent
render.

The graph is `tutti_graph`'s, built with its `GraphBuilder` and
prepared at the render's rate (`RenderGraph::prepare`): a graph prepared at
another rate is refused rather than re-rated. A host exporting its live graph
forks it instead, with `RenderGraph::fork`.

```rust
use tutti_transport::FrozenClock;
use tutti_types::{Hz, SampleRate, Seconds};
use tutti_export::{
    render_to_buffers, render_to_file, ChannelLayout, Control, EncodeConfig, ExportConfig,
    FlacLevel, Format, IntDepth, RenderConfig, RenderGraph,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Osc;

let rate = SampleRate(48_000.0);
// A render consumes its graph, so build one per render.
let tone = || {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let osc = g.add(Osc::sine(Hz(440.0)));
    g.pipe_output(osc);
    let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
    RenderGraph::new(editor, executor).expect("built together")
};

let config = ExportConfig {
    render: RenderConfig {
        sample_rate: rate,
        duration_seconds: Seconds(0.5),
        ..Default::default()
    },
    encode: EncodeConfig {
        format: Format::Flac { depth: IntDepth::Int24, level: FlacLevel::Standard },
        ..Default::default()
    },
    ..Default::default()
};

// To buffers: planar, one `Vec` per channel, and `frames()` is FRAMES per
// plane rather than the total sample count.
let rendered = render_to_buffers(tone(), &config, &FrozenClock, &mut Control::none())
    .expect("a 0.5 s stereo render")
    .finished()
    .expect("nothing cancels it");
assert_eq!(rendered.channels(), 2);
assert_eq!(rendered.frames().get(), 24_000);

// Or straight to a file, which streams — no PCM is held whole.
let dir = tempfile::tempdir().expect("temp dir");
let written = render_to_file(
    tone(),
    &config,
    &FrozenClock,
    &dir.path().join("master.flac"),
    &mut Control::none(),
)
.expect("flac encodes")
.finished()
.expect("nothing cancels it");
assert!(written.bytes > 0, "a finalized export reports its size on disk");
```

## Normalization is a separate entry point, not a config field

Choosing a gain means measuring the whole signal first, which is two passes. So
normalization is not a field on `ExportConfig` that quietly changes what
`render_to_file` costs — it is `render_normalized_to_file`, whose name says
which path the caller is on. Hiding the two passes inside one export is what
forces the whole signal into memory.

For a gain that is logged, gated, or derived some other way, compose the steps
directly: measure with `tutti_measure::measure_loudness` (a streaming meter, so it can
run *while* rendering), take `Loudness::gain_to`, apply it with
`Rendered::apply_gain`, and write with `write_buffers`.

## Features

- `wav` — WAV, `tutti-codec`'s own writer.
- `flac` — FLAC, via flacenc.
- `aiff` — AIFF / AIFF-C, via aifc.
- `ogg` — Ogg Vorbis, via vorbis_rs.

Each turns on the `tutti-codec` feature of the same name (`ogg` is its
`vorbis`), and a `Format` variant exists only with its feature.

Resampling (`tutti_dsp::StreamingResampler`, an FFT resampler on
`tutti_dsp::RealFft`) and dither (`tutti_dsp::DitherNoise`) are algorithms
in `tutti-dsp`; this crate calls them. Resampling is unconditional: it is not
a codec, so it is not gated.

## License

MIT OR Apache-2.0
