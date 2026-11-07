# tutti-metering

Load metering for the Tutti audio engine.

## What this is

- `LoadRecorder` / `LoadMeter` / `LoadReading`: how much of each block's time budget the render
  used, and the blocks over budget. The audio thread holds the recorder; any
  thread reads through `LoadMeter`.

`tutti-engine`'s `Engine` feeds it on every block it renders, a device
callback's or an offline session's. It is in the engine because it times the
block; it is not audio.

Level metering is not here. A level meter is an op in the compiled plan, on the
graph's outputs or on any node's output port: `tutti-graph`'s
`Editor::open_meter` and `MeterReader`. Copying audio out of the graph is
`tutti-graph`'s taps (`Editor::open_tap`, `TapReader`).

## Dependencies

`tutti-types` alone (plus `thiserror`).
`#![forbid(unsafe_code)]`.

## License

MIT OR Apache-2.0
