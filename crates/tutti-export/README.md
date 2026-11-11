# tutti-export

**The OFFLINE edge**: render a Tutti graph to a file, or to buffers, faster (or
slower) than real time. The `tutti` facade re-exports this crate as
`tutti::export` (feature `export`).

## What is here

The graph is *pulled* to a known frame count that a `Job` fixes up front, once,
and every `Output` of the render is fed from that one pass. `render` takes the
`Job`, a list of `Output`s and a `Control`:

- An `Output` is a `Feed` (the graph's outputs, or a plan tap, `Feed::Tap`, read
  after every block and placed by a `Placement`) and a `Target`: a file
  (`FileTarget`: path, format, width, speaker order, resample, dither,
  normalize) or planes in memory (`BufferTarget`, returned as a `Rendered`).
  `render` returns one `Produced` per output, in order; `render_one` is the
  helper for a render of one.
- A file output that does not normalize **streams**: each block goes through
  its resample, dither and encoder as it arrives and no PCM is held.
- A file output that normalizes keeps what the graph rendered for it (in memory
  up to 512 MiB across the render, then in a temporary `f32` file beside its
  target), measures it at the file's rate, and writes it again with the gain.
  The graph is never rendered twice. An output can take another's gain
  (`NormalizeSpec::from_output`), so a set of stems keeps the master's balance.
- Every file is written to a temporary beside its path and put in place at the
  end, after every output has been finalized and the fork and the event loss
  have been checked. A render that fails or is cancelled leaves every path as it
  was. The renames are the one step that is not undone: if one fails after others
  landed the error is `Error::NotCommitted`, which names the files that did.
- `write_buffers` encodes planes a caller already holds into the `FileTarget`
  that describes the file.

`Control` is how a host watches and stops the render: a `CancelToken` that is
checked (before every block, between stages, during the measurement and the
write, before each output is finalized and before the renames) and a callback
that is handed a `Progress`, a `Stage` and one fraction of the whole render.
`Control::none()` is a render nobody watches or stops. A cancelled render is
`Outcome::Cancelled`, with every temporary removed and every target as it was.
Encoding is WAV, FLAC, AIFF or OGG Vorbis, each
behind its own feature (`wav`, `flac`, `aiff`, `ogg`). **None is on by default**:
`ogg` links the C Vorbis library, so a consumer names the formats it writes. The
umbrellas (`tutti`, `bevy-tutti`, `tutti-session`) forward them as `export-wav`,
`export-flac`, `export-aiff` and `export-ogg`; their `export` feature implies
`export-wav` only, so `export-ogg` is the one path to C.

This crate re-exports no other crate's type: a `Format` or `Pcm` is
`tutti_codec`'s, a `ChannelLayout` is `tutti_types`', and the clocks (`RenderClock`,
`FrozenClock`) are `tutti_transport`'s. Each figure it needs of one of them it asks
that crate for: the frames a file can hold of the encoder, the length of a resampled
stream of the resampler.

What a render is and where it goes are values of this crate (`Job`, the
targets); a project's bounce, whose figures are derived from a session, is
`tutti-session`'s `Bounce`, and this crate is the pipeline it drives.

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
  a normalizing output measures with it. The edge is acyclic —
  `tutti-measure` does not depend on this crate.
- **Threads.** `render` is synchronous and runs on the calling
  thread. A host that wants a render off the main thread runs it on its own
  task pool.
- **A buffering strategy.** Every format streams, because every codec library
  used here supports incremental encoding. There is no buffered-versus-streaming
  mode to pick.

## Example — bounce a graph

A `Job` is a rate and three frame counts, and a target names a file or planes.
The clock is not optional:
`FrozenClock` is how a caller *says* "this graph has no time-dependent nodes",
so forgetting a transport is a compile error rather than a silently silent
render.

The graph is `tutti_graph`'s, built with its `GraphBuilder` and
prepared at the render's rate (`RenderGraph::prepare`): a graph prepared at
another rate is refused rather than re-rated. A graph is made one way, `RenderGraph::assemble(editor, executor, Setup)`,
which checks that the pair was built together; a host exporting its live graph
forks it and assembles the fork (`tutti-session` does both).

```rust
use tutti_transport::FrozenClock;
use tutti_types::{Hz, SampleRate, Samples, Seconds};
use tutti_export::{render, render_one, BufferTarget, Control, FileTarget, Job, Output, Produced, RenderGraph, Setup};
use tutti_types::ChannelLayout;
use tutti_codec::{FlacLevel, Format, IntDepth, Pcm};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Osc;

let rate = SampleRate(48_000.0);
// A render borrows its graph; build one per render.
let tone = || {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let osc = g.add(Osc::sine(Hz(440.0)));
    g.pipe_output(osc);
    let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("built together")
};

let job = Job::new(rate, Seconds(0.5).to_samples(rate), Samples(0), Samples(0))
    .expect("a half second");

// To buffers: planar, one `Vec` per channel, and `frames()` is FRAMES per
// plane rather than the total sample count.
let rendered = render_one(
    &mut tone(),
    &job,
    &FrozenClock,
    Output::master(BufferTarget::new(ChannelLayout::STEREO)),
    &mut Control::none(),
)
.expect("a 0.5 s stereo render")
.finished()
.and_then(Produced::buffers)
.expect("nothing cancels it");
assert_eq!(rendered.channels(), 2);
assert_eq!(rendered.frames().get(), 24_000);

// Or straight to files, which streams — no PCM is held whole. One pass can
// write several, each in its own format.
let dir = tempfile::tempdir().expect("temp dir");
let produced = render(
    &mut tone(),
    &job,
    &FrozenClock,
    vec![
        Output::master(FileTarget::new(
            dir.path().join("master.flac"),
            Format::Flac { depth: IntDepth::Int24, level: FlacLevel::Standard },
            ChannelLayout::STEREO,
        )),
        Output::master(FileTarget::new(
            dir.path().join("master.wav"),
            Format::Wav(Pcm::Int16),
            ChannelLayout::STEREO,
        )),
    ],
    &mut Control::none(),
)
.expect("flac and wav encode")
.finished()
.expect("nothing cancels it");
for output in produced {
    let written = output.file().expect("a file output");
    assert!(written.bytes > 0, "a finalized export reports its size on disk");
}
```

## Normalization is part of an output, not of the render

Choosing a gain means measuring the whole signal first, which is two passes. So
an output that asks for it (`FileTarget::with_normalize`) does not stream: its
audio waits in a spill while the graph renders, is measured at the file's rate
(`Normalize::Peak` or `Normalize::Lufs`, which an integer format's headroom
lowers) and is written with the gain, which `Written::gain` reports beside the
reading it came from, `Written::measured`.

For a gain that is logged, gated, or derived some other way, compose the steps
directly: render into a `BufferTarget`, take `Normalize::gain_for_rendered`,
apply it with `Rendered::apply_gain`, and write with `write_buffers`.

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
