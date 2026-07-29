# tutti-export examples

Two programs. `export.rs` is the API's documentation-by-use; `render_export_cases.rs`
plus `verify_export.py` are an independent correctness harness.

## The demo

```bash
cargo run --example export -- /tmp/export-demo
```

Writes a tone to all four formats, folds a mono graph to quad, and normalizes a
render to −14 LUFS.

### Why this exists

The crate had 45 passing tests while three of its four output formats were
broken and an upmix export panicked. The tests all went through the same WAV
stereo path; nothing rendered a FLAC, and nothing asked for a file wider than the
graph. An example that actually calls the public API the way a caller would is
the cheapest check that the API *works*, as opposed to type-checking — and it is
the same reason `tutti-sampler/examples/` exists.

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

## The correctness harness

```bash
cargo run --release -p tutti-export --example render_export_cases -- /tmp/exports

uv sync                              # from crates/tutti/
uv run python crates/core/tutti-export/examples/verify_export.py /tmp/exports
```

30 cases: every format × bit depth, DC at levels where rounding and truncation
disagree, all three dither modes, four resample ratios, every `ChunkSize` preset,
and the channel fold/upmix paths.

### Why a Python judge, when `tests/` exists

The Rust tests read files back with `hound`, which only speaks WAV. FLAC, AIFF
and Ogg are checked there by frame count and header — never by content. So
"does the FLAC hold the same samples as the WAV" is not a question the Rust suite
can ask, and that is exactly the question that mattered: the FLAC encoder carried
a private quantizer that **truncated** where `tutti_core::pcm` rounds, so 0.7
encoded as 22936 in a FLAC and 22937 in a WAV from the same render. Its own unit
tests covered only 0.0 and ±1.0, where truncation and rounding agree.

`soundfile` decodes all four containers uniformly, which makes the comparison
possible at all. And the resampler's actual quality — passband flatness, alias
rejection — needs a spectral analysis that a hand-rolled DFT in a test would be
reimplementing badly.

The judge derives every expectation from the case **name** and first principles
(a 1 kHz tone is 1 kHz whatever container it lands in; one LSB at 16-bit is
1/32767; an 18 kHz tone above the new Nyquist must be *gone*, not folded down to
4050 Hz), never from the Rust. It is a second opinion, not a restatement.

### Two traps, both paid for

**`soundfile` and tutti disagree about full scale.** tutti scales floats by
`2^(n-1) − 1` (32767 at 16-bit); `soundfile` normalizes integers by `2^(n-1)`
(32768). The gap is one part in 32768 — larger than a single LSB, so comparing
without accounting for it makes a correct encoder look broken.

**Undithered output is not "unbiased" in the same sense as dithered output.** A
constant quantizes to one integer, so its mean sits up to half an LSB off the
exact target. That is plain quantization, not a biased dither; the zero-mean
check has to be loosened for `Dither::Off` alone.

### Reading the output

Every case prints its measurement and a verdict. `dry` — an unprocessed stereo
tone, 32-bit float, no rate conversion — is the control: if it fails, the harness
is broken rather than the engine, which is the same role `dry` plays in the
sampler's matrix.

Worth confirming the harness still bites, after changing it: corrupt a rendered
file (halve its samples, zero it, copy mono into every channel) and check the
judge fails on exactly that case. A green run only means something if failure is
reachable.
