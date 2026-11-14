# tutti-export examples

`export.rs` is the API's documentation-by-use.

## The demo

```bash
cargo run --example export -- /tmp/export-demo
```

Writes a tone to a file, then to the other formats in one pass, folds a mono graph to quad, and normalizes a
render to −14 LUFS.

### Why this exists

An example that calls the public API the way a caller would — every format, a
file wider than the graph — is the cheapest check that the API *works*, as
opposed to type-checking, and it exercises paths a WAV-stereo test suite never
reaches. `tutti-verify`'s render cases exist for the same reason.

It doubles as the API's documentation. If a case here reads badly, the API is
wrong, not the example.

### What the output should look like

```
wrote .../tone.flac (151196 bytes)
wrote .../tone.wav  (576068 bytes)
wrote .../tone.ogg  (11996 bytes)
wrote .../tone.aiff (576054 bytes)
mono -> quad: 96000 frames, per-channel peaks [0.5, ~1e-7, ~1e-7, ~1e-7]
normalize: -12.72 LUFS + -1.28 dB -> -14.00 LUFS (peak -12.32 dBTP)
```

Three things worth reading closely:

- **FLAC is ~1/4 the size of WAV and Ogg ~1/48.** Equal sizes would mean a codec
  silently fell back to PCM.
- **The quad peaks are `[0.5, ~0, ~0, ~0]`**, not `[0.5, 0.5, 0.5, 0.5]`. A mono
  graph folded to surround puts its signal in channel 0 and leaves the rest
  silent; the non-zero `1e-7` is the dither floor. Copying mono into every
  channel would put program material in the LFE and both surrounds, and add
  6 dB on any downmix.
- **Normalization lands on the target exactly**, because the gain is measured
  and applied as two separate steps the caller composes — which is what lets the
  export itself stream.
