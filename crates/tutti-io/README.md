# tutti-io

Tutti's live audio I/O edge: what comes in from a device or the graph's tap,
and what goes out to a file. Device-free: the sound-card side lives in `tutti-cpal`, and the
`tutti` facade re-exports this crate as `tutti::io` (feature `io`).

## What is here

Four live pieces, and recording is just a pump between two of them:

- `MicMonitorNode` — the read side, a graph node over a device-filled
  ring, so a live input can sit anywhere in the graph. Inserted unforkable: an
  export of a graph holding the live mic is refused, naming it.
- `TapIn` — the *other* read side: the analysis tap's consumer end adapted to
  `AudioIn`, so what the graph is playing records through the same pump that
  records a microphone. `AudioTap` itself is `tutti-metering`'s (the audio callback
  pushes into it); this is only the adapter that lets it meet the I/O
  vocabulary.
- `WavOut` — the write side, an `AudioOut` sink.
- `Recorder` — `pump(src, sink)` on its own thread, plus the stop policy and the
  finalize-exactly-once guarantee that `pump` itself deliberately leaves out.
  Any `AudioIn` into any `AudioOut` of the same element, not only a WAV. It is
  the workspace's one pump loop: `bevy-tutti`'s `AudioPump` component holds a
  `Recorder` and ends it through the non-blocking `request_stop`,
  `is_finished` and `finish_in_place`.

Reading a file is `tutti-codec`'s (`Wave`, `Wave::read`, `FileIn`); this crate
holds the live edge only, and writes WAV through `tutti-codec`'s encoder.

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
- **The I/O vocabulary itself.** `AudioIn`/`AudioOut`/`pump` and `ChannelLayout`
  are `tutti-types`'; import them from there. This crate supplies the live
  *impls*, not the traits.
- **File decode.** `Wave`, `FileIn` and the decoders are `tutti-codec`'s.

```text
tutti-types    AudioIn/AudioOut, ChannelLayout
    ↑
tutti-codec     the encoder WavOut writes through
tutti-metering  AudioTap
    ↑
tutti-io       MicMonitorNode, WavOut, TapIn, Recorder   (device-free)
    ↑
tutti-cpal     MicIn, the output stream, the driver      (owns CPAL)
```

`tutti-cpal` depends on this crate (behind its `capture` feature) for the ring
and the monitor node, and `bevy-tutti` for the recorder.

## Example — recording what the graph is playing

The tap path end to end, and it needs no device: `tutti-metering`'s `AudioTap` is
pushed by the audio callback, `TapIn` is its consumer end as an `AudioIn`, and
`pump` moves one block into a `WavOut`. Swap `TapIn` for `tutti_cpal::MicIn` and
the same three lines record a microphone — that interchangeability is the reason
the traits exist.

```rust
use tutti_codec::Pcm;
use tutti_io::{TapIn, WavOut};
use tutti_metering::AudioTap;
use tutti_types::{pump, AudioIn, AudioOut, Samples};

let tap = AudioTap::new();
let mut src = TapIn::new(tap.open().expect("a fresh tap has no other reader"));

// The callback pushes an interleaved stereo block.
let block = [0.25f32, -0.25, 0.5, -0.5, 0.75, -0.75];
tap.push(&block);

let dir = tempfile::tempdir().expect("temp dir");
let path = dir.path().join("take.wav");
let mut wav = WavOut::create(&path, 48_000.0, 2u16, Pcm::Float32)
    .expect("sink opens");
assert_eq!(src.layout(), AudioOut::layout(&wav), "pump requires equal widths");

// The scratch is sized in SAMPLES because a flat interleaved slice has no other
// unit, but `pump` returns FRAMES, as a `Samples`. `interleaved_len` is the one
// named crossing between the two.
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

None. A decoder is `tutti-codec`'s `decode-*` features, and the Bevy asset
wrapper of a decoded file is `bevy-tutti`'s.

## License

MIT OR Apache-2.0
