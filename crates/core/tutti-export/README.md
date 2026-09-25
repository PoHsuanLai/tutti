# tutti-export

**The OFFLINE edge**: render a Tutti graph to a file, or to buffers, faster (or
slower) than real time.

## What this is

The graph is *pulled* to a known frame count that `RenderConfig` fixes up front,
then resampled, dithered and encoded on the way out. Three entry points, and the
names say which path a caller is on:

- `render_to_file` — render straight to disk. Streams: the encoder pulls the
  graph one block at a time and no PCM is held whole.
- `render_to_buffers` — render into memory, as planar `Vec<f32>` planes.
- `write_buffers` — encode planes a caller already holds.

`render_normalized_to_file` is the fourth, and it is separate on purpose — see
below. Encoding is WAV, FLAC, AIFF or OGG Vorbis, each behind its own feature.

## What this crate does not own

- **Live capture.** Its peer is `tutti-io`, the **LIVE** edge — a microphone, a
  tap, a `Recorder` pushing blocks into a WAV as they arrive. The distinction is
  structural rather than stylistic: a live capture has no total frame count (it
  ends when someone stops it), so it cannot be expressed as an export, and an
  export needs a total (to size a plan, trim latency, cap output), so it cannot
  be expressed as a live pump. Reach here to bounce a mix; reach for `tutti-io`
  to record one.
- **Loudness measurement.** EBU R128 lives in `tutti-analysis`, which is where
  the engine's `Config`/`State`/step analysis vocabulary is;
  `render_normalized_to_file` measures with it. The edge is acyclic —
  `tutti-analysis` does not depend on this crate.
- **Threads.** Both entry points are synchronous and `Send`. A host that wants a
  render off the main thread already owns a task pool that is better at it than
  a raw `std::thread` would be.
- **A buffering strategy.** Every format streams, because every codec library
  used here supports incremental encoding. There is no buffered-versus-streaming
  mode to pick.

## Example — bounce a graph

A config is a struct literal, so a caller states what it means and lets
`..Default::default()` cover the rest. The clock is not optional:
`FrozenClock` is how a caller *says* "this graph has no time-dependent nodes",
so forgetting a transport is a compile error rather than a silently silent
render.

The graph is the native one (`tutti_graph`), built with its `GraphBuilder` and
prepared at the render's rate (`RenderGraph::prepare`): a graph prepared at
another rate is refused rather than re-rated. A host exporting its live graph
forks it instead, with `RenderGraph::fork`. The native graph is the only one
an export renders: fundsp's `Net` is not accepted (doc 013 Phase 3 PR 14).

```rust
use tutti_core::{FrozenClock, Hz, SampleRate};
use tutti_export::{
    render_to_buffers, render_to_file, AudioFormat, ChannelLayout, EncodeConfig, ExportConfig,
    Flac, RenderConfig, RenderGraph,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Osc;

let rate = SampleRate(48_000.0);
// A render consumes its graph, so build one per render.
let tone = || {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let osc = g.add_unit(Box::new(Osc::sine(Hz(440.0))));
    g.pipe_output(osc);
    let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
    RenderGraph { editor, executor }
};

let config = ExportConfig {
    render: RenderConfig {
        sample_rate: rate,
        // `f64`, not `Seconds`: an f32 cannot carry an hour-long render.
        duration_seconds: 0.5,
        ..Default::default()
    },
    encode: EncodeConfig {
        format: AudioFormat::Flac(Flac::default()),
        ..Default::default()
    },
    ..Default::default()
};

// To buffers: planar, one `Vec` per channel, and `frames()` is FRAMES per
// plane rather than the total sample count.
let rendered = render_to_buffers(tone(), &config, &FrozenClock)
    .expect("a 0.5 s stereo render");
assert_eq!(rendered.channels(), 2);
assert_eq!(rendered.frames().get(), 24_000);

// Or straight to a file, which streams — no PCM is held whole.
let dir = tempfile::tempdir().expect("temp dir");
let written = render_to_file(tone(), &config, &FrozenClock, &dir.path().join("master.flac"))
    .expect("flac encodes");
assert!(written.bytes > 0, "a finalized export reports its size on disk");
```

## Normalization is a separate entry point, not a config field

Choosing a gain means measuring the whole signal first, which is two passes. So
normalization is not a field on `ExportConfig` that quietly changes what
`render_to_file` costs — it is `render_normalized_to_file`, whose name says
which path the caller is on. Hiding the two passes inside one export is what
forces the whole signal into memory.

For a gain that is logged, gated, or derived some other way, compose the steps
directly: measure with `tutti_analysis::loudness` (a streaming meter, so it can
run *while* rendering), take `Loudness::gain_to`, apply it with
`Rendered::apply_gain`, and write with `write_buffers`.

## Features

- `wav` (default) — WAV encoding, via hound.
- `flac` (default) — FLAC encoding, via flacenc.
- `aiff` (default) — AIFF / AIFF-C encoding, via aifc.
- `ogg` (default) — OGG Vorbis encoding, via vorbis_rs.

Resampling (rubato) is unconditional — it is not a codec, so it is not gated.

## License

MIT OR Apache-2.0
