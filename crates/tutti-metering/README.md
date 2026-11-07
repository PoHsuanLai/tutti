# tutti-metering

Level metering for the Tutti audio engine.

## What this is

- `MasterMeter` / `MeterWriter` / `MeterReading`: peak and RMS of the output.
  The audio thread holds the writer; any thread reads through `MasterMeter`.
- `LoadRecorder` / `LoadMeter` / `LoadReading`: how much of each block's time budget the render
  used, and the blocks over budget. The audio thread holds the recorder; any
  thread reads through `LoadMeter`.

`tutti-engine`'s `Engine` feeds both meters on every block it renders, a device
callback's or an offline session's. Copying audio out of the graph is
`tutti-graph`'s taps (`Editor::open_tap`, `TapReader`), not this crate's.

## Dependencies

`tutti-types` alone (plus `thiserror`).
`#![forbid(unsafe_code)]`.

## License

MIT OR Apache-2.0
