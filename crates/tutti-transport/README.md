# tutti-transport

Playback transport for the Tutti audio engine: where the playhead is, how fast
it moves, and what moves it.

## What this is

Split by who decides. `TransportSettings` holds the plain values (tempo, loop
region, recording) anyone may store into; `MotionFsm` is the state machine that
may defer or reject a play, stop or locate, and queues commands timed to a frame
or a beat (`MotionFsm::schedule`; `try_send` is `schedule` at the next block, on
the same queue). `Transport` is the two halves as one handle, and
`TransportAudio` (`Transport::audio`) is the audio-thread side `tutti-engine`'s
`Engine` takes once: it drives the playhead and hands it to the graph in each
block's `Env`.

Also here: the read side (`Timeline`), the clocks a renderer
drives (`RenderClock`, `FrozenClock`, `OfflineTimeline`) and `ProjectMeter`, the
project's time signature and tempo map.

## Dependencies

`tutti-types`, `tutti-rt` and `tutti-graph` (a `RenderClock` hands the executor
its block's transport). It does not depend on `tutti-engine` or
`tutti-metering`. `#![forbid(unsafe_code)]`.

## License

MIT OR Apache-2.0
