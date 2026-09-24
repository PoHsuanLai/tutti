# tutti-io

Tutti's audio I/O edge: what comes in from a device or a file, and what goes
out to a file.

## What this is

Four live pieces, and recording is just a pump between two of them:

- `MicMonitorNode` — the read side, an `AudioUnit` over a device-filled ring, so
  a live input can sit anywhere in the graph.
- `TapIn` — the *other* read side: the analysis tap's consumer end adapted to
  `AudioIn`, so what the graph is playing records through the same pump that
  records a microphone. `AudioTap` itself is `tutti-core`'s (the audio callback
  pushes into it); this is only the adapter that lets it meet the I/O
  vocabulary.
- `WavOut` — the write side, an `AudioOut` sink.
- `Recorder` — `pump(src, sink)` on its own thread, plus the stop policy and the
  finalize-exactly-once guarantee that `pump` itself deliberately leaves out.

And the file side, which needs no device either:

- `Wave` — a resident, planar sample buffer: what a sampler voice plays.
- `Wave::load` / `Wave::probe_metadata` — decode a file into one, or read only
  its header. Behind the codec features.
- `FileIn` — the streaming read, an `AudioIn` over a file at the file's own
  width, with sample-accurate `seek`. The read-side twin of `WavOut`.
- `WaveAsset` — the Bevy asset wrapper (feature `bevy`).
- `can_decode` / `decodable_extensions` — which formats *this build* reads.

These came from the fundsp fork (design doc 013, Phase 0); decode output is
pinned bit-for-bit by `tests/decode_golden.rs`.

The traits this crate implements are re-exported from its root (`pump`,
`AudioIn`, `AudioOut`, `OnEmpty`, `BitDepth`), so a consumer reaches the
vocabulary and its live impls from one place.

## What this crate does not own

- **The device.** It is device-free, and that is what puts it *below*
  `tutti-cpal` rather than inside it. `Recorder::start` is generic over
  `AudioIn`, so a microphone is one option among several — a socket, a decoded
  file, or a generated signal record identically; the caller opens its own device
  and hands the source over. Folding this into `tutti-cpal` would make a headless
  render pull in CPAL just to write a WAV. Note the arrow: the mic ring's
  *producer* half lives one layer **up**, in CPAL's input callback.
- **Offline rendering.** That is `tutti-export`'s. The two are peers: one moves
  frames in real time between endpoints a host owns, the other renders a graph
  faster than real time to a file.
- **The I/O vocabulary itself.** `AudioIn`/`AudioOut`/`pump`, `ChannelLayout` and
  the PCM quantizers are all `tutti-types`', re-exported through `tutti-core`.
  This crate supplies the live *impls*, not the traits.

```text
tutti-types    AudioIn/AudioOut, ChannelLayout, the PCM quantizers
    ↑
tutti-core     AudioUnit, AudioTap; re-exports io
    ↑
tutti-io       MicMonitorNode, WavOut, TapIn, Recorder,  (device-free)
               Wave, FileIn, the decoder
    ↑
tutti-cpal     MicIn, the output stream, the driver      (owns CPAL)
```

`tutti-cpal` depends on this crate (behind its `capture` feature) for the ring
and the monitor node; `tutti-sampler` depends on it for `Wave` and the decoder,
and `bevy-tutti` for both.

## Example — recording what the graph is playing

The tap path end to end, and it needs no device: `tutti-core`'s `AudioTap` is
pushed by the audio callback, `TapIn` is its consumer end as an `AudioIn`, and
`pump` moves one block into a `WavOut`. Swap `TapIn` for `tutti_cpal::MicIn` and
the same three lines record a microphone — that interchangeability is the reason
the traits exist.

```rust
use tutti_core::{AudioTap, Samples};
use tutti_io::{pump, AudioIn, AudioOut, BitDepth, TapIn, WavOut};

let tap = AudioTap::new();
let mut src = TapIn::new(tap.open().expect("a fresh tap has no other reader"));

// The callback's push is denominated in FRAMES; the slice it reads from is
// interleaved, so it must hold `frames * 2` samples.
let block = [0.25f32, -0.25, 0.5, -0.5, 0.75, -0.75];
tap.push(&block, 3);

let dir = tempfile::tempdir().expect("temp dir");
let path = dir.path().join("take.wav");
let mut wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32)
    .expect("sink opens");
assert_eq!(src.layout(), AudioOut::layout(&wav), "pump requires equal widths");

// The scratch is sized in SAMPLES because a flat interleaved slice has no other
// unit — but `pump` returns FRAMES, as a `Samples`. Conflating the two is this
// boundary's most repeated defect: a stereo take compared against a sample
// count runs half as long as it should. So the one crossing is named, and a
// bare `usize` is not accepted where a frame count is meant.
let mut scratch = vec![0.0f32; Samples(1024).interleaved_len(src.layout())];
assert_eq!(pump(&mut src, &mut wav, &mut scratch), Samples(3));

// `finalize` takes `self`, so the header back-patch happens exactly once and
// writing after it is a compile error rather than a corrupt file.
wav.finalize().expect("header back-patches");
```

## Two things worth knowing

- **`AudioIn::ON_EMPTY` is not optional bookkeeping.** A zero-frame poll means
  "not yet" for a mic and "never again" for a decoded file, and nothing in the
  count distinguishes them. Getting it wrong ends a take milliseconds in with no
  error anywhere.
- **`Recorder::start` checks the layouts.** It is the one place both endpoints
  are in scope before a frame moves, so a width mismatch is an error there
  rather than a file whose channels rotate every frame.

## Features

All off by default (`default = []`):

- `wav` / `flac` / `mp3` / `ogg` — one symphonia container (and codec) each,
  for `Wave::load`, `Wave::probe_metadata` and `FileIn`. With none, those do
  not exist and `decodable_extensions()` is empty; `Wave` itself is always
  there.
- `bevy` — the `Asset` derive on `WaveAsset`. Needs a codec as well.

## License

MIT OR Apache-2.0
