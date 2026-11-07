# tutti-metering

Level metering for the Tutti audio engine.

## What this is

- `MasterMeter` / `MeterReading`: peak and RMS of the output, read by any
  thread.
- `AudioTap`: hands the callback's output to an analysis consumer through a
  lock-free ring (`AudioTap::open`, `TapCons`).
- `LoadMeter` / `LoadReading`: how much of each block's time budget the render
  used, and the blocks over budget.
- `meter_output` and `MeteringContext`: what the audio callback runs on one
  block to feed the three.

## Dependencies

`tutti-types` alone (plus `ringbuf`, `parking_lot` and `thiserror`).
`#![forbid(unsafe_code)]`.

## License

MIT OR Apache-2.0
