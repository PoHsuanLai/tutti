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

The Rust tests read files back with `hound`, which only speaks WAV. FLAC, AIFF
and Ogg are checked there by frame count and header — never by content. So
"does the FLAC hold the same samples as the WAV" is not a question the Rust suite
can ask. A FLAC quantizer that **truncated** where `tutti_codec::Pcm` rounds
would encode 0.7 as 22936 in a FLAC and 22937 in a WAV from the same render,
and a unit test at 0.0 and ±1.0, where truncation and rounding agree, would not
notice.

`soundfile` decodes all four containers uniformly, which makes the comparison
possible at all. And the resampler's actual quality — passband flatness, alias
rejection — needs a spectral analysis that a hand-rolled DFT in a test would be
reimplementing badly.

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
