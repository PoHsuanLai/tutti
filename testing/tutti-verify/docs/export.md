# Export correctness harness

Judges `tutti-export`'s files against independent decoders.

```bash
cargo run --release -p tutti-verify --bin render_export_cases -- /tmp/exports

uv sync                              # from the repo root
uv run python testing/tutti-verify/py/verify_export.py /tmp/exports
```

30 cases: every format × bit depth, DC at levels where rounding and truncation
disagree, all three dither modes, four resample ratios, every `ChunkSize` preset,
and the channel fold/upmix paths.

### Why a Python judge, when `tests/` exists

`soundfile` decodes all four containers the same way, so one comparison spans
them. The resampler's quality (passband flatness, alias rejection) needs a
spectral analysis, which the judge does with numpy's FFT.

The judge derives every expectation from the case **name** and first principles
(a 1 kHz tone is 1 kHz whatever container it lands in; one LSB at 16-bit is
1/32767; an 18 kHz tone above the new Nyquist must be *gone*, not folded down to
4050 Hz), never from the Rust. It is a second opinion, not a restatement.

### Two traps

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
