# tutti-io

Tutti's **live** audio I/O edge: what comes in from a device, and what goes out
to a file.

## What this is

Four pieces, and recording is just a pump between two of them:

- `MicMonitorNode` — the read side, an `AudioUnit` over a device-filled ring, so
  a live input can sit anywhere in the graph.
- `TapIn` — the *other* read side: the analysis tap's consumer end adapted to
  `AudioIn`, so what the graph is playing records through the same pump that
  records a microphone.
- `WavOut` — the write side, an `AudioOut` sink.
- `Recorder` — `pump(src, sink)` on its own thread, plus the stop policy and the
  finalize-exactly-once guarantee that `pump` itself deliberately leaves out.

## Why it is its own crate

**It is device-free, and that is what puts it *below* `tutti-cpal` rather than
inside it.** `Recorder::start` is generic over `AudioIn`, so a microphone is one
option among several — a socket, a decoded file, or a generated signal record
identically; the caller opens its own device and hands the source over. Folding
this into `tutti-cpal` would make a headless render pull in CPAL just to write a
WAV.

It is also the **live** edge, as distinct from `tutti-export`'s **offline** one.
The two are peers: one moves frames in real time between endpoints a host owns,
the other renders a graph faster than real time to a file.

## Where it sits

```
tutti-types    AudioIn/AudioOut, ChannelLayout, the PCM quantizers
    ↑
tutti-core     Wave, AudioUnit; re-exports io
    ↑
tutti-io       MicMonitorNode, WavOut, TapIn, Recorder   (device-free)
    ↑
tutti-cpal     MicIn, the output stream, the driver      (owns CPAL)
```

`tutti-cpal` depends on this crate (behind its `capture` feature) for the ring
and the monitor node; `tutti-sampler` and `bevy-tutti` depend on it too. Note
the arrow: the mic ring's *producer* half lives one layer up, in CPAL's input
callback.

The traits this crate implements are re-exported from its root
(`pump`, `AudioIn`, `AudioOut`, `OnEmpty`, `BitDepth`), so a consumer reaches
the vocabulary and its live impls from one place.

## Features

None. The crate is the live I/O edge; there is nothing here to gate.

## Two things worth knowing

- **`AudioIn::ON_EMPTY` is not optional bookkeeping.** A zero-frame poll means
  "not yet" for a mic and "never again" for a decoded file, and nothing in the
  count distinguishes them. Getting it wrong ends a take milliseconds in with no
  error anywhere.
- **`Recorder::start` checks the layouts.** It is the one place both endpoints
  are in scope before a frame moves, so a width mismatch is an error there
  rather than a file whose channels rotate every frame.

## License

MIT OR Apache-2.0
