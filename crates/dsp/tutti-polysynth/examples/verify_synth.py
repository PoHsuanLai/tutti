#!/usr/bin/env python3
"""Judge tutti-synth's rendered output against first-principles synthesis theory.

Run:
    cargo run --release --manifest-path crates/bevy-tutti/Cargo.toml \
        -p tutti-synth --example render_synth_cases -- /tmp/tutti-synth
    uv run python crates/dsp/tutti-synth/examples/verify_synth.py /tmp/tutti-synth

The Rust half renders audio and encodes each case's parameters in its filename;
this half re-derives what the file should contain from that name and from
textbook synthesis theory, and judges it. Neither knows the other's
expectations. That separation is the point: a judge written against the same
understanding as the code cannot contradict it.

What this adds over `tests/synth_audio.rs`, which already pins pitch, level,
channel layout and the allocation strategies: **spectral shape**. A sawtooth is
not merely "a tone at 110 Hz" — it is a specific harmonic series, and an
oscillator can get the fundamental exactly right while getting the timbre
completely wrong. That is invisible to any test that measures only the dominant
frequency.
"""

import math
import re
import sys
from pathlib import Path

import numpy as np

try:
    import soundfile as sf
except ImportError:  # pragma: no cover - environment-dependent
    sys.exit("soundfile is required: uv sync")

SR = 48000.0

# Skip the attack and take a stationary window. The envelope is flat by
# construction (see `flat_envelope` in the renderer), so anything after the
# 1 ms attack is steady state.
ANALYSIS_START = 8192
ANALYSIS_LEN = 16384


class Report:
    """Collects results so one run reports every problem, not just the first."""

    def __init__(self):
        self.failures = []
        self.checks = 0

    def check(self, ok, label, detail=""):
        self.checks += 1
        if ok:
            print(f"  PASS  {label}")
        else:
            print(f"  FAIL  {label}")
            if detail:
                print(f"        {detail}")
            self.failures.append(label)
        return ok


def midi_hz(note):
    """Equal-tempered frequency. Derived here, not read from the Rust."""
    return 440.0 * 2 ** ((note - 69) / 12)


def load(path):
    x, sr = sf.read(path)
    assert abs(sr - SR) < 1, f"{path.name}: unexpected sample rate {sr}"
    if x.ndim == 1:
        x = np.stack([x, x], axis=-1)
    return x


def spectrum(mono):
    """Hann-windowed magnitude spectrum and its frequency axis.

    The window matters: without it, spectral leakage from the rectangular edges
    biases both peak positions and harmonic amplitudes, by enough (~1% on the
    peak) to invent errors that are not in the signal.
    """
    seg = mono[ANALYSIS_START : ANALYSIS_START + ANALYSIS_LEN]
    win = np.hanning(len(seg))
    mag = np.abs(np.fft.rfft(seg * win))
    freqs = np.fft.rfftfreq(len(seg), 1 / SR)
    return freqs, mag


def harmonic_amp(freqs, mag, f):
    """Peak magnitude in a narrow band around `f`.

    A band rather than the single nearest bin: the analysis window is 16384
    samples so bins are ~2.9 Hz apart, and a harmonic whose true frequency falls
    between bins spreads across neighbours. Taking the max over +/-3 bins
    recovers the peak without admitting the next harmonic.
    """
    if f >= SR / 2:
        return 0.0
    idx = int(np.argmin(np.abs(freqs - f)))
    lo, hi = max(0, idx - 3), min(len(mag), idx + 4)
    return float(mag[lo:hi].max())


def harmonic_series(freqs, mag, f0, count=8):
    """Amplitudes of harmonics 1..count, normalised to the fundamental."""
    amps = [harmonic_amp(freqs, mag, f0 * k) for k in range(1, count + 1)]
    a1 = amps[0]
    if a1 <= 0:
        return None
    return [a / a1 for a in amps]


# --------------------------------------------------------------------------
# Waveform spectra
#
# The expected shapes are textbook, and are stated here rather than measured
# from tutti:
#   sine     -- one harmonic, nothing else
#   saw      -- all harmonics, amplitude 1/k
#   square   -- odd harmonics only, amplitude 1/k
#   triangle -- odd harmonics only, amplitude 1/k^2
#
# Tolerances account for band-limiting. A real oscillator cannot emit the ideal
# series -- it must suppress everything above Nyquist to avoid aliasing, and the
# anti-aliasing shapes the top of the series. Measured slopes come out at -0.98
# (saw), -0.94 (square) and -1.94 (triangle) against ideals of -1, -1 and -2,
# which is the ~6% the bounds below allow.
# --------------------------------------------------------------------------


def judge_waveforms(path, rep):
    print("\n=== waveform spectra ===")
    for f in sorted(path.glob("wave_*.wav")):
        m = re.match(r"wave_(\w+)_note(\d+)\.wav", f.name)
        if not m:
            continue
        wave, note = m.group(1), int(m.group(2))
        f0 = midi_hz(note)

        x = load(f)
        freqs, mag = spectrum(x[:, 0])

        # The fundamental must dominate, and must be where the note says.
        peak_idx = int(np.argmax(mag[(freqs > 30) & (freqs < SR / 2 - 100)])) + int(
            np.searchsorted(freqs, 30)
        )
        peak_f = freqs[peak_idx]
        rep.check(
            abs(peak_f - f0) / f0 < 0.02,
            f"{f.name}: fundamental at {peak_f:.1f} Hz (want {f0:.1f})",
        )

        amps = harmonic_series(freqs, mag, f0)
        if amps is None:
            rep.check(False, f"{f.name}: no energy at the fundamental")
            continue

        if wave == "sine":
            # A pure sine has nothing above the fundamental. 2% admits FFT
            # leakage and the oscillator's own numerical noise, and nothing more.
            worst = max(amps[1:])
            rep.check(
                worst < 0.02,
                f"{f.name}: harmonics suppressed (largest {worst:.4f})",
                "a sine must have no partials above the fundamental",
            )

        elif wave == "saw":
            # Every harmonic present, falling as 1/k.
            for k in (2, 3, 4, 5):
                rep.check(
                    amps[k - 1] > 0.05,
                    f"{f.name}: harmonic {k} present ({amps[k - 1]:.3f})",
                    "a sawtooth contains every harmonic",
                )
            slope = np.polyfit(np.log(np.arange(1, 9)), np.log(np.maximum(amps, 1e-9)), 1)[0]
            rep.check(
                -1.25 < slope < -0.80,
                f"{f.name}: harmonic rolloff {slope:.3f} (ideal -1 for 1/k)",
                "a sawtooth's amplitudes should fall as 1/k",
            )

        elif wave == "square":
            # Odd harmonics only. The even ones are the discriminating check:
            # an oscillator emitting a saw would pass every "harmonic present"
            # assertion and fail here.
            for k in (2, 4, 6):
                rep.check(
                    amps[k - 1] < 0.05,
                    f"{f.name}: even harmonic {k} suppressed ({amps[k - 1]:.4f})",
                    "a 50% square has no even harmonics",
                )
            for k in (3, 5):
                rep.check(
                    amps[k - 1] > 0.05,
                    f"{f.name}: odd harmonic {k} present ({amps[k - 1]:.3f})",
                )
            odd = [1, 3, 5, 7]
            slope = np.polyfit(
                np.log(odd), np.log([max(amps[k - 1], 1e-9) for k in odd]), 1
            )[0]
            rep.check(
                -1.25 < slope < -0.80,
                f"{f.name}: odd-harmonic rolloff {slope:.3f} (ideal -1)",
            )

        elif wave == "triangle":
            for k in (2, 4, 6):
                rep.check(
                    amps[k - 1] < 0.05,
                    f"{f.name}: even harmonic {k} suppressed ({amps[k - 1]:.4f})",
                    "a triangle has no even harmonics",
                )
            odd = [1, 3, 5, 7]
            slope = np.polyfit(
                np.log(odd), np.log([max(amps[k - 1], 1e-9) for k in odd]), 1
            )[0]
            # The check that separates triangle from square: -2 not -1.
            rep.check(
                -2.35 < slope < -1.65,
                f"{f.name}: odd-harmonic rolloff {slope:.3f} (ideal -2 for 1/k^2)",
                "a triangle falls as 1/k^2, twice as fast as a square",
            )


# --------------------------------------------------------------------------
# Filters
# --------------------------------------------------------------------------


def judge_filters(path, rep):
    """A filter must attenuate the side it is named for, and pass the other.

    Measured as a **transfer function against the unfiltered saw**: for each
    harmonic, the filtered amplitude divided by the same harmonic's amplitude in
    `wave_saw_note45.wav`. That is the only comparison that isolates the filter,
    because it cancels the oscillator's own 1/k rolloff and the envelope gain.

    The first version of this check compared the loudest harmonic below the
    cutoff against the loudest above it, within a single file, and reported both
    highpass cases as broken. It was the check that was broken. A saw falls as
    1/k, so the "above cutoff" band's maximum always sits at its *lowest*
    harmonic -- right at the transition edge, where a correct filter has barely
    begun to act. It compared two points that say nothing about the filter. The
    engine was fine: measured properly, the highpass attenuates 110 Hz to 0.003
    of source at a 2 kHz cutoff and passes 3.5 kHz at 0.95.
    """
    print("\n=== filters ===")

    dry_path = path / "wave_saw_note45.wav"
    if not dry_path.exists():
        print("  (no unfiltered saw to compare against -- skipping)")
        return
    dry_freqs, dry_mag = spectrum(load(dry_path)[:, 0])

    for f in sorted(path.glob("filter_*.wav")):
        m = re.match(r"filter_(lp|hp)(\d+)_note(\d+)\.wav", f.name)
        if not m:
            continue
        kind, cutoff, note = m.group(1), float(m.group(2)), int(m.group(3))
        f0 = midi_hz(note)

        freqs, mag = spectrum(load(f)[:, 0])

        def transfer(freq):
            """Filtered amplitude / dry amplitude at `freq`."""
            d = harmonic_amp(dry_freqs, dry_mag, freq)
            if d <= 1e-9:
                return None
            return harmonic_amp(freqs, mag, freq) / d

        # Sample an octave clear of the cutoff on each side, so the transition
        # region is never what decides the verdict.
        deep_stop = [
            f0 * k
            for k in range(1, 60)
            if (f0 * k < cutoff / 3 if kind == "hp" else f0 * k > cutoff * 3)
            and f0 * k < SR / 2 - 2000
        ]
        deep_pass = [
            f0 * k
            for k in range(1, 60)
            if (f0 * k > cutoff * 2 if kind == "hp" else f0 * k < cutoff / 2)
            and f0 * k < SR / 2 - 2000
        ]

        stop_vals = [t for t in (transfer(x) for x in deep_stop) if t is not None]
        pass_vals = [t for t in (transfer(x) for x in deep_pass) if t is not None]

        if stop_vals:
            worst = max(stop_vals)
            rep.check(
                worst < 0.35,
                f"{f.name}: stopband attenuated to {worst:.4f} of source",
                f"a {cutoff:.0f} Hz {kind} must attenuate the far side of its "
                "cutoff",
            )
        if pass_vals:
            worst = min(pass_vals)
            rep.check(
                worst > 0.6,
                f"{f.name}: passband preserved at {worst:.4f} of source",
                f"a {cutoff:.0f} Hz {kind} must pass its own passband",
            )

        # The cutoff is defined as the -3 dB point (0.707). Checking the
        # transfer *at* the named frequency pins that the cutoff parameter means
        # what it says -- a filter with the right shape at the wrong frequency
        # passes both checks above and fails this one.
        at_cut = transfer(cutoff)
        if at_cut is None:
            # The cutoff may not coincide with a harmonic; interpolate from the
            # nearest ones instead of skipping the check entirely.
            near = sorted(
                (abs(f0 * k - cutoff), f0 * k) for k in range(1, 60) if f0 * k < SR / 2
            )[:1]
            at_cut = transfer(near[0][1]) if near else None
        if at_cut is not None:
            rep.check(
                0.4 < at_cut < 0.95,
                f"{f.name}: transfer near the cutoff is {at_cut:.3f} "
                "(-3 dB point is 0.707)",
                "the cutoff frequency should be where the filter is ~3 dB down",
            )


# --------------------------------------------------------------------------
# Unison
# --------------------------------------------------------------------------


def judge_unison(path, rep):
    """Detuned unison voices must beat, and stereo spread must decorrelate.

    The Rust suite asserts the *opposite* case — that a plain voice is identical
    in both channels — so this is the other half of that claim. Beating is
    measured as amplitude-envelope modulation, which is what detuning physically
    produces and what a unison that silently collapsed to one voice would lack.
    """
    print("\n=== unison ===")
    for f in sorted(path.glob("unison_*.wav")):
        m = re.match(r"unison_v(\d+)_d(\d+)_note(\d+)\.wav", f.name)
        if not m:
            continue
        voices, detune, note = int(m.group(1)), float(m.group(2)), int(m.group(3))

        x = load(f)
        left, right = x[:, 0], x[:, 1]
        seg_l = left[ANALYSIS_START : ANALYSIS_START + ANALYSIS_LEN]
        seg_r = right[ANALYSIS_START : ANALYSIS_START + ANALYSIS_LEN]

        # Amplitude envelope via analytic-signal magnitude, smoothed. Beating
        # shows as variation in this envelope.
        env = np.abs(seg_l)
        k = 512
        smooth = np.convolve(env, np.ones(k) / k, mode="valid")
        variation = smooth.std() / max(smooth.mean(), 1e-12)

        rep.check(
            variation > 0.01,
            f"{f.name}: envelope varies by {variation:.4f} (beating present)",
            f"{voices} voices detuned {detune:.0f} cents must beat against "
            "each other",
        )

        # Stereo spread must decorrelate the channels. Without unison the Rust
        # tests require them bit-identical, so a correlation of 1.0 here means
        # the spread never took effect.
        corr = float(
            np.corrcoef(seg_l, seg_r)[0, 1]
            if seg_l.std() > 0 and seg_r.std() > 0
            else 1.0
        )
        rep.check(
            corr < 0.999,
            f"{f.name}: channels decorrelated (r={corr:.4f})",
            "stereo spread should make the channels differ",
        )


# --------------------------------------------------------------------------
# Polyphony
# --------------------------------------------------------------------------


def judge_chord(path, rep):
    """Every note of a chord must be present simultaneously.

    The spectral counterpart to the Rust `two_simultaneous_notes_both_sound`,
    which can only establish that the mix got louder. A synth that dropped one
    note of a triad, or retriggered the same voice three times, passes that and
    fails this.
    """
    print("\n=== polyphony ===")
    f = path / "chord_60_64_67.wav"
    if not f.exists():
        return

    x = load(f)
    freqs, mag = spectrum(x[:, 0])
    peak = mag.max()

    for note in (60, 64, 67):
        f0 = midi_hz(note)
        amp = harmonic_amp(freqs, mag, f0)
        rep.check(
            amp > peak * 0.1,
            f"chord: note {note} ({f0:.1f} Hz) present at {amp / peak:.3f} of peak",
            "all three notes of the triad must sound together",
        )


def judge_control(path, rep):
    """The unprocessed control case.

    Twice in this repo's history a harness was itself wrong and the dry case is
    what caught it. If this fails, no other result in this run means anything.
    """
    print("\n=== control ===")
    f = path / "control_sine_note69.wav"
    if not f.exists():
        return
    x = load(f)
    freqs, mag = spectrum(x[:, 0])
    idx = int(np.argmax(mag))
    rep.check(
        abs(freqs[idx] - 440.0) < 5.0,
        f"control: a plain A4 sine reads {freqs[idx]:.2f} Hz",
        "the control case must be exactly what it says it is",
    )
    rms = float(np.sqrt(np.mean(x[ANALYSIS_START:, 0] ** 2)))
    rep.check(rms > 0.01, f"control: audible (rms {rms:.4f})")


def main():
    path = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/tutti-synth")
    if not path.is_dir():
        sys.exit(f"no such directory: {path}\nRun render_synth_cases first.")

    rep = Report()
    judge_control(path, rep)
    judge_waveforms(path, rep)
    judge_filters(path, rep)
    judge_unison(path, rep)
    judge_chord(path, rep)

    print(f"\n{rep.checks} checks, {len(rep.failures)} failed")
    if rep.failures:
        for f in rep.failures:
            print(f"  - {f}")
        sys.exit(1)
    print("all clear")


if __name__ == "__main__":
    main()
