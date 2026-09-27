# tutti-sampler

Sample playback for the Tutti audio engine: voices that play audio from memory
or stream it from disk, a per-track voice pool, and a phase-vocoder time
stretch.

Use it to play clips on a timeline or free-running: a one-shot, a looped
region, a whole take streamed from disk, varispeed or time-stretched. It is a
clip player, not an instrument: there is no `note_on`, no keymap and no voice
stealing (see `tutti-polysynth` and `tutti-soundfont` for note-driven
playback). Most applications reach it through the `tutti` crate
(`tutti::sampler`, feature `sampler`) or through `bevy-tutti`, which adds the
Bevy plugin and asset loader.

## Two playback tiers

A voice plays either from memory or streamed from disk, and **the tier is the
caller's choice**; the sampler never picks one on its own.

- `MemorySource` plays a decoded `Wave` held in memory. It needs no thread and
  no file, and is a graph node on its own.
- `DiskVoice` plays a file streamed by the butler, a background thread owned by
  `DiskStreamer`. The butler decodes ahead of the playhead into a ring per
  stream; the audio thread only reads the ring, so the streaming tier is
  real-time safe. `probe` reads a file's header and says whether this build
  can stream it.

Both are wrapped by `Voice` and played by one of two graph nodes:

- `VoicePool`: every voice on a track in one node, controlled from the control
  thread through a `VoicePoolHandle` (add, remove, re-place, respeed, stretch).
  Adding or removing a voice is a queued command, not a graph edit.
- `VoiceNode`: a single voice as its own node, for previews and single clips,
  controlled through a `VoiceNodeHandle`.

A voice placed on the transport (a start beat and an optional duration)
derives its read position from the playhead in its block's `Env`, so it enters,
exits, loops and follows a seek on the exact frame, wherever that falls in a
block. An unplaced voice runs free once started.

## Rates

Three rate types keep the two tiers consistent: `PlaybackRate` is varispeed
(reading faster raises the pitch), `SrcRatio` is sample-rate conversion
(derived from the file and engine rates, never set by the user), and
`StretchFactor` drives the phase vocoder (duration changes, pitch does not).
All three are `tutti-core` types.

## Quick start

An in-memory voice needs no butler and no file, so it is a plain graph node:
build the `Wave`, wrap it, put it in a graph and render.

```rust
use std::sync::Arc;
use tutti_core::{SampleRate, Samples};
use tutti_graph::{Prepare, Solo};
use tutti_sampler::{MemorySource, Wave};

// 100 stereo frames; `push_frame` takes one frame, not one sample.
let mut wave = Wave::new(2, 44_100.0);
for _ in 0..100 {
    wave.push_frame(&[0.5, 0.5]);
}

// Free-running (not placed on the transport), so it plays once started.
let source = MemorySource::new(Arc::new(wave));
source.play();

// No audio input: the voice is the source. Stereo out.
let mut graph = Solo::new(source, Prepare::new(SampleRate(44_100.0), Samples(64)));
let out = graph.render(64);
assert_eq!(out.len(), 2);
assert!(out[0].iter().all(|&s| s != 0.0));
```

Streaming needs a real file. Build a `DiskStreamer` once with
`DiskStreamer::new`, start a stream through its `commands()` port, and take
the `DiskVoice` for it from its `status()` port; `DiskStreamer`'s own
documentation has that example.

## Channel ceiling

`MAX_SAMPLER_CHANNELS` is the widest frame a voice reads or emits. It equals
`tutti_core::MAX_ROOT_CHANNELS`, the graph root's width: a voice wider than the
root could render would be truncated downstream, so the sampler refuses it
here instead (`VoicePool::with_channels` returns `PoolTooWide`).

## Features

`default = ["wav"]`.

- `wav`, `flac`, `mp3`, `ogg`: decoder support for that format, forwarded to
  `tutti-io`, which owns the decoder. `files` turns on all four. With none of
  them, `probe` and `SampleFacts` are absent: no header can be read.
- `bevy`: derives `Component` on the voice-pool markers `VoicePoolRef` and
  `VoicePoolNode`. The plugin and asset loader are `bevy-tutti`'s; everything
  else in this crate works without Bevy.
- `test-support`: `DiskStreamer::manual` and `step_once`, which run the
  butler's cycle by hand instead of on its thread, and the `testing` module (a
  mock transport and block driver). For tests; a host has no use for it.
