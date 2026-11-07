# tutti-tempo

A tempo map for the Tutti audio engine.

## What this is

- `TempoMap`: an immutable value built on the control side from `TempoPoint`s.
  Each point holds its tempo until the next (`Shape::Step`) or moves to the next
  one's, linear in time (`RampTime`) or linear in beats (`RampBeats`). Queries
  convert between beats, `f64` seconds and frames; none allocates. A `Cursor`
  hint makes the monotone case a few nanoseconds.
- `chord_cell`, `CellGrid`, `Cell`: a map as constant-tempo cells on one fixed
  frame grid, cut also at every point (`TempoMap::next_change`). A cell is a
  pure function of the absolute frame, so it is the same at any buffer size.

The constructors refuse a tempo that is not finite and positive themselves.

## Dependencies

`tutti-types` and `libm` (pure-Rust `log1p`, `expm1` and `exp`, so a ramp's
figures do not depend on the platform's math library). `serde` is optional and
only on `CellGrid`. `#![forbid(unsafe_code)]`.

`cargo run --release -p tutti-tempo --example bench` times the lookups.
