# tutti-transport

Playback transport for the Tutti audio engine: where the playhead is, how fast
it moves, and what moves it.

## What this is

Split by who decides. `TransportSettings` holds the plain values (tempo, loop
region, recording) anyone may store into; `MotionFsm` is the state machine that
may defer or reject a play, stop or locate, and queues commands timed to a frame
or a beat (`MotionFsm::schedule`). `Transport` is the two halves as one handle,
and `TransportClock` is the playhead `tutti-engine`'s `Engine` drives and hands
to the graph in each block's `Env`.

Also here: the read side (`Timeline`), the clocks a renderer
drives (`RenderClock`, `FrozenClock`, `OfflineTimeline`) and `ProjectMeter`, the
project's time signature and tempo map.

`Control`, `MotionAudio`, `Schedule` and `Scheduled` are the audio side of the
state machine, public for `tutti-engine` to run; a host does not name them.

## Dependencies

`tutti-types`, `tutti-rt` and `tutti-graph` (a `RenderClock` hands the executor
its block's transport). It does not depend on `tutti-engine` or
`tutti-metering`. `#![forbid(unsafe_code)]`.

## License

MIT OR Apache-2.0
