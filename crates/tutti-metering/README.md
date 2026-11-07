# tutti-metering

Level metering for the Tutti audio engine.

## What this is

- `MasterMeter` / `MeterWriter` / `MeterReading`: peak and RMS of the output.
  The audio thread holds the writer; any thread reads through `MasterMeter`.
- `AudioTap`: hands the callback's output to an analysis consumer through a
  lock-free ring (`AudioTap::open`, `TapCons`).
- `LoadRecorder` / `LoadMeter` / `LoadReading`: how much of each block's time budget the render
  used, and the blocks over budget. The audio thread holds the recorder; any
  thread reads through `LoadMeter`.
- `meter_output`: what the audio callback runs on one block to feed the meter
  and the tap.

## Dependencies

`tutti-types` alone (plus `ringbuf`, `parking_lot` and `thiserror`).
`#![forbid(unsafe_code)]`.

## License

MIT OR Apache-2.0
