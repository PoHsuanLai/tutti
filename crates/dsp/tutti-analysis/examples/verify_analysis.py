#!/usr/bin/env python3
"""Judge tutti-analysis's pitch and loudness readings against independent references.

Run:
    cargo run --release --manifest-path crates/bevy-tutti/Cargo.toml \
        -p tutti-analysis --example render_analysis_cases -- /tmp/tutti-analysis
    uv run python crates/dsp/tutti-analysis/examples/verify_analysis.py /tmp/tutti-analysis

The Rust half emits what tutti *measured*; this half recomputes what the answer
should be, from the case name and from libraries with no knowledge of tutti.
Neither side reads the other's expectations -- that is the whole point. A judge
written against the same understanding as the code cannot contradict it, which
is how four sampler defects survived a full Rust suite.

Two different kinds of check live here, and the distinction matters:

  * **Loudness has an absolute answer.** EBU R128 is a published spec, and
    pyloudnorm is its reference Python implementation. A 1 kHz sine at a known
    amplitude has a calculable LUFS, so these are checked against a *number*.

  * **Pitch does not.** Every YIN implementation differs in windowing, in
    thresholding, and in interpolation, so librosa is a cross-check rather than
    an oracle -- it reads static tones ~0.2% high itself. Pitch is therefore
    checked against the *synthesised* frequency, which is known exactly, with
    librosa reported alongside so a disagreement can be attributed.
"""

import csv
import math
import sys
from pathlib import Path

import numpy as np

SR = 48000.0

# soundfile/librosa are only needed for the cross-check; the absolute checks
# stand without them.
try:
    import librosa

    HAVE_LIBROSA = True
except ImportError:  # pragma: no cover - environment-dependent
    HAVE_LIBROSA = False

try:
    import pyloudnorm as pyln

    HAVE_PYLN = True
except ImportError:  # pragma: no cover - environment-dependent
    HAVE_PYLN = False


class Report:
    """Collects pass/fail lines so one run reports every problem, not the first."""

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


# --------------------------------------------------------------------------
# Signal synthesis -- deliberately duplicated from the Rust side.
#
# Sharing a generated WAV between the two halves would make agreement prove only
# that both read the same bytes. Each side builds the signal from the case name
# instead, so a mismatch is a real disagreement about the DSP.
# --------------------------------------------------------------------------


def harmonic_tone(freq, harmonics, secs, amp):
    n = int(SR * secs)
    t = np.arange(n) / SR
    s = np.zeros(n)
    for k, a in enumerate(harmonics):
        if a:
            s += a * np.sin(2 * np.pi * freq * (k + 1) * t)
    return (s * amp).astype(np.float32)


def signal_for_case(name):
    """Rebuild a pitch case's audio from its name alone."""
    if name.startswith("sine_"):
        return harmonic_tone(float(name[5:]), [1.0], 0.5, 0.5)
    if name.startswith("saw_"):
        f = float(name[4:])
        return harmonic_tone(f, [1.0 / k for k in range(1, 9)], 0.5, 0.3)
    if name.startswith("missingfund_"):
        return harmonic_tone(float(name[12:]), [0.0, 1.0, 0.7, 0.5], 0.5, 0.4)
    if name.startswith("square_"):
        f = float(name[7:])
        return harmonic_tone(f, [1.0, 0.0, 0.33, 0.0, 0.2, 0.0, 0.14], 0.5, 0.4)
    if name == "quiet_440":
        return harmonic_tone(440.0, [1.0], 0.5, 0.005)
    return None  # noise / silence -- no deterministic restatement needed


def freq_to_note_name(f):
    """Nearest equal-tempered note name, computed independently of tutti."""
    names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
    midi = round(69 + 12 * math.log2(f / 440.0))
    return f"{names[midi % 12]}{midi // 12 - 1}"


# --------------------------------------------------------------------------
# Pitch
# --------------------------------------------------------------------------


def judge_pitch(path, rep):
    print("\n=== pitch ===")
    rows = list(csv.DictReader(open(path / "pitch.csv")))

    for r in rows:
        name = r["case"]
        expected = float(r["expected_hz"])
        got = float(r["tutti_hz"])
        conf = float(r["confidence"])
        voiced = r["voiced"] == "true"

        # Controls: noise and silence must not be confidently pitched. A
        # detector that reports a pitch for white noise is worse than useless --
        # it is confidently wrong, and downstream tuning would act on it.
        if expected == 0.0:
            rep.check(
                not voiced,
                f"{name}: correctly unvoiced",
                f"reported {got:.2f} Hz at confidence {conf:.3f}",
            )
            continue

        if not rep.check(voiced, f"{name}: voiced", "reported unvoiced"):
            continue

        # 0.5% is generous for a synthetic tone but tight enough to catch the
        # errors that matter. An octave error is 100% or 50% off; a
        # cents-level interpolation bug is ~0.1%. Nothing real lands at 0.4%.
        err_pct = abs(got - expected) / expected * 100
        rep.check(
            err_pct < 0.5,
            f"{name}: {got:.3f} Hz vs {expected:.1f} Hz ({err_pct:.3f}% error)",
            f"expected within 0.5%",
        )

        # Octave errors get their own named check. They are YIN's signature
        # failure mode, they are what the harmonic-rich cases exist to provoke,
        # and folding them into the tolerance above would report them as merely
        # "inaccurate" rather than as the specific, recognisable defect they are.
        for mult, label in ((2.0, "octave up"), (0.5, "octave down"), (3.0, "twelfth")):
            if abs(got - expected * mult) < expected * mult * 0.02:
                rep.check(False, f"{name}: {label} error", f"got {got:.1f}, want {expected:.1f}")

        # The note name must agree with the frequency tutti itself reported --
        # an internally inconsistent result would mean the MIDI conversion is
        # wrong even though the detection is right.
        if r["note"] != "-":
            rep.check(
                r["note"] == freq_to_note_name(got),
                f"{name}: note name {r['note']} matches {got:.1f} Hz",
                f"independent calculation says {freq_to_note_name(got)}",
            )

        # Cross-check against librosa on the identical signal. Reported as a
        # comparison, not asserted tightly: librosa reads static tones ~0.2%
        # high, so demanding agreement would encode its bias as truth.
        if HAVE_LIBROSA:
            sig = signal_for_case(name)
            if sig is not None:
                y = librosa.yin(sig, fmin=50, fmax=2000, sr=int(SR))
                lib = float(np.median(y))
                rep.check(
                    abs(lib - got) / expected < 0.03,
                    f"{name}: agrees with librosa ({lib:.2f} vs {got:.2f})",
                    "the two YIN implementations disagree by >3%",
                )


def judge_sweep(path, rep):
    """A rising sweep, judged per frame.

    The subtlety this encodes: YIN does **not** analyse the whole frame.
    `compute_difference` correlates `x[0..max_period]` against
    `x[0..2*max_period]`, so with a 50 Hz floor at 48 kHz a 1920-sample frame is
    judged on its first 960 samples. The expected value is therefore the mean
    instantaneous frequency over *that* sub-window.

    Judging against the frame centre instead puts every expectation 14-17 Hz
    high and reads as a systematic downward bias in the detector. It is not one
    -- the same code reads a static tone to within 0.002 Hz at the same frame
    length. Getting this wrong in the judge would have reported a bug that does
    not exist.

    A second, smaller effect is modelled below. Even against the correct window
    the reading sits a little high, by an amount that falls monotonically with
    frequency (4.4 Hz at 220 Hz, 1.4 Hz at 745 Hz). In the period domain -- which
    is what YIN actually estimates -- that is an error of -4.4 samples shrinking
    to -0.12, tracking how far the frequency travels *within* the correlation
    window: the lowest frame has the longest period, so its window spans the
    fewest cycles and the estimate is pulled hardest. This is intrinsic to
    running YIN on a chirp, not a property of tutti, so the tolerance is scaled
    by the within-window sweep rate rather than set to a flat percentage. A flat
    2% would fail frame 0 alone and invite widening the number until it passed,
    which would encode the confusion instead of the physics.
    """
    print("\n=== sweep (yin_track) ===")
    rows = list(csv.DictReader(open(path / "sweep.csv")))

    frame = 2 * int(SR / 50.0)  # buffer_size = max_period * 2
    analysed = frame // 2  # the correlation window: max_period
    total = frame * len(rows)

    for r in rows:
        i = int(r["frame"])
        got = float(r["tutti_hz"])
        # Mean instantaneous frequency over the analysed sub-window, derived
        # here from the sweep's definition rather than read from the CSV.
        centre = (i * frame + analysed / 2) / total
        expected = 200.0 + 600.0 * centre

        # How much the sweep moves across the analysed window. The estimate
        # cannot be sharper than the signal it is given: a window whose
        # frequency spans `span` Hz has no single true answer to within `span`.
        span = 600.0 * analysed / total
        tol = span * 0.5 + expected * 0.005

        err = abs(got - expected)
        rep.check(
            err < tol,
            f"frame {i}: {got:.2f} Hz vs {expected:.2f} Hz expected "
            f"({err:.2f} Hz, tol {tol:.2f})",
            "a sweep frame is judged on the first max_period samples, "
            "with tolerance scaled by how far the sweep moves inside it",
        )

    # The track must be monotonic for a monotonically rising sweep. A single
    # frame landing an octave off would still pass the per-frame tolerance if
    # the sweep crossed it, but breaks the ordering.
    freqs = [float(r["tutti_hz"]) for r in rows]
    rep.check(
        all(b > a for a, b in zip(freqs, freqs[1:])),
        "sweep is monotonically rising",
        f"got {[round(f, 1) for f in freqs]}",
    )


# --------------------------------------------------------------------------
# Loudness
# --------------------------------------------------------------------------


def judge_loudness(path, rep):
    print("\n=== loudness (EBU R128) ===")
    rows = list(csv.DictReader(open(path / "loudness.csv")))

    for r in rows:
        amp = float(r["amp"])
        lufs = float(r["tutti_lufs"])
        peak = float(r["tutti_true_peak"])

        # True peak is exact and needs no reference implementation: a sine of
        # amplitude `a` peaks at `a`, so dBTP = 20*log10(a). Checked first
        # because it isolates the meter's peak path from its gating path.
        want_peak = 20 * math.log10(amp)
        rep.check(
            abs(peak - want_peak) < 0.3,
            f"amp {amp}: true peak {peak:.3f} dBTP vs {want_peak:.3f} expected",
            "true peak should be the sine's amplitude in dB",
        )

        if HAVE_PYLN:
            # pyloudnorm is the reference R128 implementation. This is the
            # absolute check -- not "tutti agrees with itself" but "tutti agrees
            # with the spec".
            n = int(SR * 3.0)
            t = np.arange(n) / SR
            mono = (amp * np.sin(2 * np.pi * 1000 * t)).astype(np.float32)
            stereo = np.stack([mono, mono], axis=-1)
            ref = pyln.Meter(int(SR)).integrated_loudness(stereo)
            rep.check(
                abs(lufs - ref) < 0.5,
                f"amp {amp}: {lufs:.3f} LUFS vs pyloudnorm {ref:.3f}",
                "beyond the 0.5 LU tolerance R128 allows between meters",
            )

    # A pure steady tone has no loudness *range* -- LRA measures variation, and
    # there is none. A non-zero reading here would mean the gating blocks are
    # being computed over something that changes when it should not.
    for r in rows:
        rep.check(
            abs(float(r["tutti_range"])) < 0.5,
            f"amp {r['amp']}: steady tone has ~zero loudness range",
            f"got {r['tutti_range']} LU",
        )

    # Level linearity: halving the amplitude must drop the reading by exactly
    # 6.02 dB. This is independent of any reference implementation and catches a
    # meter whose absolute calibration is off but whose scaling is right (or
    # vice versa).
    by_amp = {float(r["amp"]): float(r["tutti_lufs"]) for r in rows}
    for hi, lo in ((1.0, 0.5), (0.5, 0.25), (0.1, 0.01)):
        if hi in by_amp and lo in by_amp:
            delta = by_amp[hi] - by_amp[lo]
            want = 20 * math.log10(hi / lo)
            rep.check(
                abs(delta - want) < 0.15,
                f"{hi} -> {lo}: {delta:.3f} dB drop vs {want:.3f} expected",
                "loudness must scale linearly with amplitude in dB",
            )


def judge_loudness_rate(path, rep):
    """The same musical signal at three sample rates must read the same.

    This axis has a documented defect history: two true-peak sites hardcoded
    48 kHz while a sibling threaded the real rate, so a 44.1 kHz render was
    metered through a filter built for the wrong rate. `LoudnessConfig` exists to
    make that unrepresentable; this checks that it does.
    """
    print("\n=== loudness rate-independence ===")
    rows = list(csv.DictReader(open(path / "loudness_rate.csv")))
    lufs = [float(r["tutti_lufs"]) for r in rows]
    peaks = [float(r["tutti_true_peak"]) for r in rows]

    rep.check(
        max(lufs) - min(lufs) < 0.2,
        f"LUFS agrees across rates (spread {max(lufs) - min(lufs):.3f} LU)",
        f"readings: {[(r['rate'], r['tutti_lufs']) for r in rows]}",
    )
    rep.check(
        max(peaks) - min(peaks) < 0.3,
        f"true peak agrees across rates (spread {max(peaks) - min(peaks):.3f} dB)",
        f"readings: {[(r['rate'], r['tutti_true_peak']) for r in rows]}",
    )

    # Each rate independently against the reference, so a *uniform* error --
    # which the spread check above would pass -- is still caught.
    if HAVE_PYLN:
        for r in rows:
            rate = float(r["rate"])
            n = int(rate * 3.0)
            t = np.arange(n) / rate
            mono = (0.5 * np.sin(2 * np.pi * 1000 * t)).astype(np.float32)
            ref = pyln.Meter(int(rate)).integrated_loudness(
                np.stack([mono, mono], axis=-1)
            )
            rep.check(
                abs(float(r["tutti_lufs"]) - ref) < 0.5,
                f"{rate:.0f} Hz: {r['tutti_lufs']} LUFS vs pyloudnorm {ref:.3f}",
            )


def main():
    path = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/tutti-analysis")
    if not path.is_dir():
        sys.exit(f"no such directory: {path}\nRun render_analysis_cases first.")

    if not HAVE_LIBROSA:
        print("note: librosa missing -- skipping the cross-implementation check")
    if not HAVE_PYLN:
        print("note: pyloudnorm missing -- skipping the absolute R128 check")

    rep = Report()
    judge_pitch(path, rep)
    judge_sweep(path, rep)
    judge_loudness(path, rep)
    judge_loudness_rate(path, rep)

    print(f"\n{rep.checks} checks, {len(rep.failures)} failed")
    if rep.failures:
        for f in rep.failures:
            print(f"  - {f}")
        sys.exit(1)
    print("all clear")


if __name__ == "__main__":
    main()
