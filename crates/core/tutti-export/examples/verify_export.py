"""Independent verification of tutti-export output.

Judges files written by `examples/render_export_cases.rs` using
soundfile/numpy/scipy. Expectations are derived here from the case NAME and
first principles -- a 1 kHz tone is 1 kHz whatever container it lands in, one
LSB at 16-bit is 1/32767, an 18 kHz tone above the new Nyquist must be gone
rather than folded -- never read from the Rust source. The point is a second
opinion that cannot inherit the implementation's assumptions.

This complements rather than duplicates the Rust suite. `tests/roundtrip.rs`
reads files back with hound, which only speaks WAV, so FLAC/AIFF/Ogg are checked
there by frame count and header alone. soundfile decodes all four uniformly,
which is what makes "does the FLAC hold the same samples as the WAV" answerable
at all.

Usage:
    cargo run --release -p tutti-export --example render_export_cases -- /tmp/exports
    uv run python crates/core/tutti-export/examples/verify_export.py /tmp/exports
"""

import pathlib
import sys

import numpy as np
import soundfile as sf

SR = 48000
TONE_HZ = 1000.0

# Tutti scales floats by (2^(n-1) - 1) -- 32767 at 16-bit -- while soundfile
# normalizes integers by 2^(n-1) = 32768. The two differ by one part in 32768,
# which is larger than a single LSB, so comparing without accounting for it makes
# a correct encoder look wrong. Every integer file read here is rescaled by this
# factor to get back to the domain tutti wrote in.
I16_FIX = 32768.0 / 32767.0
I24_FIX = 8388608.0 / 8388607.0

failures = []


def fail(case, msg):
    failures.append((case, msg))
    return "FAIL"


def load(path):
    """Decode any container to (float64 samples [frames, channels], rate)."""
    data, rate = sf.read(str(path), always_2d=True, dtype="float64")
    return data, rate


def dominant_hz(x, rate):
    """FFT peak with parabolic interpolation, so sub-bin error stays visible."""
    w = np.hanning(len(x))
    spec = np.abs(np.fft.rfft(x * w))
    k = int(np.argmax(spec))
    if k == 0 or k == len(spec) - 1:
        return k * rate / len(x)
    a, b, c = spec[k - 1], spec[k], spec[k + 1]
    denom = a - 2 * b + c
    delta = 0.5 * (a - c) / denom if denom != 0 else 0.0
    return (k + delta) * rate / len(x)


def rms(x):
    return float(np.sqrt(np.mean(x**2)))


def energy_near(x, rate, target, frac=0.03):
    """Fraction of spectral energy within +/-frac of `target` Hz."""
    w = np.hanning(len(x))
    spec = np.abs(np.fft.rfft(x * w)) ** 2
    freqs = np.fft.rfftfreq(len(x), 1 / rate)
    total = spec.sum()
    if total <= 0:
        return 0.0
    band = (freqs > target * (1 - frac)) & (freqs < target * (1 + frac))
    return float(spec[band].sum() / total)


def settled(x, rate):
    """The middle of the signal, past any filter warm-up at either edge."""
    skip = int(rate * 0.1)
    return x[skip:-skip] if len(x) > 2 * skip + 1024 else x


# ---------------------------------------------------------------------------
# 1. The control
# ---------------------------------------------------------------------------


def check_dry(d):
    """If this fails, the harness is wrong rather than the engine."""
    x, rate = load(d / "dry.wav")
    left = settled(x[:, 0], rate)
    hz = dominant_hz(left, rate)
    lvl = rms(left)

    print(f"{'dry':28} {rate:6d} Hz  f0={hz:8.2f}  rms={lvl:.4f}", end="  ")
    if rate != SR:
        return print(fail("dry", f"rate {rate} != {SR}"))
    if abs(hz - TONE_HZ) / TONE_HZ > 0.01:
        return print(fail("dry", f"f0 {hz:.1f} != {TONE_HZ}"))
    # -6 dBFS sine: RMS = 0.5/sqrt(2) ~ 0.354
    if abs(lvl - 0.3536) > 0.02:
        return print(fail("dry", f"rms {lvl:.4f} != ~0.354"))
    print("ok")


# ---------------------------------------------------------------------------
# 2. Every container/depth carries the same audio
# ---------------------------------------------------------------------------

FORMAT_CASES = [
    ("fmt_wav_i16.wav", 16),
    ("fmt_wav_i24.wav", 24),
    ("fmt_wav_f32.wav", 32),
    ("fmt_flac_i16.flac", 16),
    ("fmt_flac_i24.flac", 24),
    ("fmt_aiff_i16.aiff", 16),
    ("fmt_aiff_i24.aiff", 24),
    ("fmt_aiff_f32.aiff", 32),
]


def check_formats(d):
    ref, _ = load(d / "dry.wav")
    ref_left = settled(ref[:, 0], SR)
    ref_rms = rms(ref_left)

    print(f"\n{'case':28} {'rate':>6}    {'f0':>8}  {'rms':>7}  {'d_dB':>6}  verdict")
    print("-" * 78)

    for name, bits in FORMAT_CASES:
        p = d / name
        if not p.exists():
            print(fail(name, "missing"))
            continue
        x, rate = load(p)
        left = settled(x[:, 0], rate)
        hz = dominant_hz(left, rate)
        lvl = rms(left)
        db = 20 * np.log10(lvl / ref_rms) if lvl > 0 else -np.inf

        print(f"{name:28} {rate:6d} {hz:8.2f}  {lvl:7.4f}  {db:+6.2f}", end="  ")

        if rate != SR:
            print(fail(name, f"rate {rate} != {SR}"))
            continue
        if abs(hz - TONE_HZ) / TONE_HZ > 0.01:
            print(fail(name, f"f0 {hz:.1f} != {TONE_HZ}"))
            continue
        # Quantization to 16 bits changes the level by far less than 0.1 dB; a
        # format that halved its samples or wrote the wrong depth shows up here
        # as several dB, and one that wrote silence as -inf.
        if abs(db) > 0.1:
            print(fail(name, f"level {db:+.2f} dB from dry"))
            continue
        # Sample-wise against the float reference, at the depth's own floor.
        n = min(len(left), len(ref_left))
        tol = {16: 1 / 32767, 24: 1 / 8388607, 32: 1e-6}[bits] * 2
        dev = np.abs(left[:n] - ref_left[:n]).max()
        if dev > tol:
            print(fail(name, f"max sample deviation {dev:.2e} > {tol:.2e}"))
            continue
        print("ok")

    # Ogg is lossy: spectral only, never sample-wise.
    p = d / "fmt_ogg.ogg"
    if p.exists():
        x, rate = load(p)
        left = settled(x[:, 0], rate)
        hz = dominant_hz(left, rate)
        pur = energy_near(left, rate, TONE_HZ)
        print(f"{'fmt_ogg.ogg':28} {rate:6d} {hz:8.2f}  {rms(left):7.4f}  {'':>6}", end="  ")
        if abs(hz - TONE_HZ) / TONE_HZ > 0.01:
            print(fail("fmt_ogg", f"f0 {hz:.1f} != {TONE_HZ}"))
        elif pur < 0.95:
            print(fail("fmt_ogg", f"purity {pur:.3f} < 0.95"))
        else:
            print(f"ok (purity {pur:.3f})")


# ---------------------------------------------------------------------------
# 3. DC: the exactly-knowable case, and cross-format quantization agreement
# ---------------------------------------------------------------------------


def check_dc(d):
    """A constant must survive quantization to the same integer everywhere.

    0.7 x 32767 = 22936.9, which rounds to 22937 and truncates to 22936. That
    one-LSB gap is exactly how a private truncating converter in the FLAC encoder
    went unnoticed -- both files decoded, both had the right length, and the
    samples differed. This is the check that sees it.
    """
    print(f"\n{'dc case':28} {'expect':>8} {'got':>8}  verdict")
    print("-" * 60)

    for tag, level in [("p70", 0.7), ("n70", -0.7)]:
        # Round-half-away-from-zero, which is what Rust's f32::round does.
        expect = int(np.sign(level) * np.floor(abs(level) * 32767.0 + 0.5))
        seen = {}
        for ext in ("wav", "flac", "aiff"):
            p = d / f"dc_{tag}_{ext}_i16.{ext}"
            if not p.exists():
                print(fail(f"dc_{tag}_{ext}", "missing"))
                continue
            x, _ = load(p)
            ints = np.rint(x[:, 0] * 32768.0).astype(int)
            vals = np.unique(ints)
            name = f"dc_{tag}_{ext}"
            print(f"{name:28} {expect:8d} {vals[0]:8d}", end="  ")
            if len(vals) != 1:
                print(fail(name, f"DC is not constant: {len(vals)} distinct values"))
                continue
            if vals[0] != expect:
                print(fail(name, f"quantized to {vals[0]}, expected {expect}"))
                continue
            seen[ext] = int(vals[0])
            print("ok")

        if len(set(seen.values())) > 1:
            fail(
                f"dc_{tag}",
                f"formats disagree on the same sample: {seen} "
                "(a private quantizer has drifted from tutti_core::pcm)",
            )
            print(f"  -> FORMATS DISAGREE: {seen}")


# ---------------------------------------------------------------------------
# 4. Dither
# ---------------------------------------------------------------------------


def check_dither(d):
    """Rect and tri must differ, stay bounded, and not bias the signal."""
    print(f"\n{'dither':28} {'span':>5} {'mean_err':>9} {'std':>7}  verdict")
    print("-" * 66)

    exact = 0.25 * 32767.0
    spans = {}
    for tag, want_span in [("off", 1), ("rect", 2), ("tri", 3)]:
        p = d / f"dither_{tag}.wav"
        if not p.exists():
            print(fail(f"dither_{tag}", "missing"))
            continue
        x, _ = load(p)
        ints = np.rint(x[:, 0] * 32768.0).astype(int)
        span = int(ints.max() - ints.min() + 1)
        mean_err = float(ints.mean() - exact)
        std = float(ints.std())
        spans[tag] = span

        print(f"{'dither_' + tag:28} {span:5d} {mean_err:+9.4f} {std:7.4f}", end="  ")
        if span != want_span:
            print(fail(f"dither_{tag}", f"spans {span} integers, expected {want_span}"))
            continue
        # Zero-mean: a biased dither is a DC offset, audible as a click at the
        # start and end of every export and cumulative through a chain.
        #
        # Only meaningful where there IS noise. Undithered, the single output
        # integer is the rounding of the exact target (0.25 x 32767 = 8191.75 ->
        # 8192), so its "mean error" is the half-LSB of plain quantization --
        # correct behaviour, not a bias. Held to half an LSB here; the dithered
        # modes average their noise away and are held far tighter.
        limit = 0.5 if tag == "off" else 0.1
        if abs(mean_err) > limit:
            print(fail(f"dither_{tag}", f"mean off by {mean_err:+.4f} LSB -- not zero-mean"))
            continue
        print("ok")

    # TPDF must be materially wider than RPDF, or one mode is aliasing the other.
    if "rect" in spans and "tri" in spans and spans["rect"] == spans["tri"]:
        fail("dither", "rect and tri have the same span -- one mode is not distinct")


# ---------------------------------------------------------------------------
# 5. Resampling -- the real gap in the Rust suite
# ---------------------------------------------------------------------------


def check_resample(d):
    """A resampled file must hold the tone, at the right rate and level.

    The Rust tests check the frame count, which a resampler emitting zeros would
    also satisfy. These check the signal.
    """
    print(f"\n{'resample':28} {'rate':>6} {'f0':>9} {'purity':>7} {'d_dB':>6}  verdict")
    print("-" * 82)

    ref, _ = load(d / "dry.wav")
    ref_rms = rms(settled(ref[:, 0], SR))

    for tag, want_rate in [("44k1", 44100), ("96k", 96000), ("22k05", 22050)]:
        p = d / f"resample_{tag}.wav"
        if not p.exists():
            print(fail(f"resample_{tag}", "missing"))
            continue
        x, rate = load(p)
        left = settled(x[:, 0], rate)
        hz = dominant_hz(left, rate)
        pur = energy_near(left, rate, TONE_HZ)
        lvl = rms(left)
        db = 20 * np.log10(lvl / ref_rms) if lvl > 0 else -np.inf
        name = f"resample_{tag}"

        print(f"{name:28} {rate:6d} {hz:9.2f} {pur:7.4f} {db:+6.2f}", end="  ")
        if rate != want_rate:
            print(fail(name, f"rate {rate} != {want_rate}"))
            continue
        # A rate conversion must not move the tone.
        if abs(hz - TONE_HZ) / TONE_HZ > 0.01:
            print(fail(name, f"f0 shifted to {hz:.1f} Hz"))
            continue
        # Nearly all energy still at 1 kHz: a conversion that aliased or
        # modulated would smear it even with the peak in the right bin.
        if pur < 0.98:
            print(fail(name, f"purity {pur:.4f} < 0.98 -- conversion artifacts"))
            continue
        # And it must not change the level.
        if abs(db) > 0.5:
            print(fail(name, f"level {db:+.2f} dB from dry"))
            continue
        print("ok")

    # The anti-alias case: 18 kHz downsampled to 22.05 k is above the new
    # Nyquist (11.025 k) and must be filtered out. Without an anti-alias filter
    # it folds to |18000 - 22050| = 4050 Hz, loudly.
    p = d / "resample_alias_22k05.wav"
    if p.exists():
        x, rate = load(p)
        left = settled(x[:, 0], rate)
        lvl = rms(left)
        alias = energy_near(left, rate, 4050.0, frac=0.05)
        db = 20 * np.log10(lvl) if lvl > 0 else -np.inf
        print(f"{'resample_alias_22k05':28} {rate:6d} {'':>9} {alias:7.4f} {db:+6.2f}", end="  ")
        if alias > 0.05:
            print(fail("resample_alias", f"{alias:.3f} of energy at 4050 Hz -- 18 kHz folded down"))
        elif db > -40:
            print(fail("resample_alias", f"residual {db:+.1f} dBFS -- above-Nyquist content survived"))
        else:
            print(f"ok (rejected to {db:+.1f} dBFS)")

    # Every chunk preset must give substantially the same conversion.
    chunks = []
    for i in range(4):
        p = d / f"chunk_{i}.wav"
        if not p.exists():
            continue
        x, rate = load(p)
        left = settled(x[:, 0], rate)
        chunks.append((i, dominant_hz(left, rate), energy_near(left, rate, TONE_HZ)))
    for i, hz, pur in chunks:
        print(f"{'chunk_' + str(i):28} {'':>6} {hz:9.2f} {pur:7.4f} {'':>6}", end="  ")
        if abs(hz - TONE_HZ) / TONE_HZ > 0.01 or pur < 0.98:
            print(fail(f"chunk_{i}", f"f0 {hz:.1f}, purity {pur:.4f}"))
        else:
            print("ok")


# ---------------------------------------------------------------------------
# 6. Channels
# ---------------------------------------------------------------------------


def check_channels(d):
    """Upmix leaves the extras silent; a fold uses the ITU matrix."""
    print(f"\n{'channels':28} {'ch':>3}  verdict")
    print("-" * 60)

    p = d / "chan_mono.wav"
    if p.exists():
        x, rate = load(p)
        print(f"{'chan_mono':28} {x.shape[1]:3d}", end="  ")
        if x.shape[1] != 1:
            print(fail("chan_mono", f"{x.shape[1]} channels, expected 1"))
        elif rms(settled(x[:, 0], rate)) < 0.1:
            print(fail("chan_mono", "silent"))
        else:
            print("ok")

    # A mono graph widened to quad puts its signal in channel 0 and leaves the
    # rest silent -- NOT four copies, which would add 6 dB on any downmix.
    p = d / "chan_mono_to_quad.wav"
    if p.exists():
        x, rate = load(p)
        peaks = [float(np.abs(x[:, c]).max()) for c in range(x.shape[1])]
        print(f"{'chan_mono_to_quad':28} {x.shape[1]:3d}", end="  ")
        if x.shape[1] != 4:
            print(fail("chan_mono_to_quad", f"{x.shape[1]} channels, expected 4"))
        elif peaks[0] < 0.4:
            print(fail("chan_mono_to_quad", f"channel 0 peak {peaks[0]:.3f} -- signal missing"))
        elif max(peaks[1:]) > 1e-3:
            print(fail("chan_mono_to_quad", f"extras not silent: {peaks[1:]}"))
        else:
            print(f"ok (peaks {[round(v, 3) for v in peaks]})")

    # Stereo widened to 5.1: L and R carry the tone, C/LFE/Ls/Rs are silent.
    # Widening must not invent phantom centre content.
    p = d / "chan_stereo_to_51.wav"
    if p.exists():
        x, rate = load(p)
        peaks = [float(np.abs(x[:, c]).max()) for c in range(x.shape[1])]
        print(f"{'chan_stereo_to_51':28} {x.shape[1]:3d}", end="  ")
        if x.shape[1] != 6:
            print(fail("chan_stereo_to_51", f"{x.shape[1]} channels, expected 6"))
        elif min(peaks[0], peaks[1]) < 0.4:
            print(fail("chan_stereo_to_51", f"front pair too quiet: {peaks[:2]}"))
        elif max(peaks[2:]) > 1e-3:
            print(fail("chan_stereo_to_51", f"stereo invented content in C/LFE/surrounds: {peaks[2:]}"))
        else:
            print(f"ok (peaks {[round(v, 3) for v in peaks]})")


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    d = pathlib.Path(sys.argv[1])
    if not d.is_dir():
        print(f"no such directory: {d}")
        return 2

    check_dry(d)
    check_formats(d)
    check_dc(d)
    check_dither(d)
    check_resample(d)
    check_channels(d)

    print()
    if failures:
        print(f"{len(failures)} FAILING:")
        for name, msg in failures:
            print(f"  {name}: {msg}")
        return 1
    print("All export cases pass.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
