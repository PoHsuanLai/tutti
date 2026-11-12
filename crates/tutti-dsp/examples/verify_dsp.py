#!/usr/bin/env python3
"""Judge tutti-dsp's filters and export resampler against independent references.

Run:
    cargo run --locked --release -p tutti-dsp --example render_dsp_cases -- /tmp/tutti-dsp
    uv run python crates/tutti-dsp/examples/verify_dsp.py /tmp/tutti-dsp

The Rust half renders impulse responses (and test tones for the resampler) and
writes the parameters of every case to cases.json. This half re-derives the
expected response from the published definition, with scipy and numpy only, and
compares. Neither side reads the other's expectation.

References, all built here from the definitions:

  * SVF: the biquads of the "RBJ Audio EQ Cookbook" (Robert Bristow-Johnson),
    coded below from its formulas. The SVF is a different topology (Simper's
    trapezoidal integrators) with the same transfer function: both are the
    bilinear transform of the same analog prototype, prewarped at the cutoff.
  * Ladder: the small-signal Moog ladder, four identical one-pole stages
    1/(1 + s/wc) with global feedback k = 4 * resonance:
        H(s) = 1 / ((1 + s/wc)^4 + k)
    bilinear-transformed with prewarp at the cutoff, i.e. evaluated at
    s/wc = j tan(pi f / fs) / tan(pi fc / fs). That is a continuous-time model
    of the feedback loop; the discrete ladder closes it without a delay
    (zero-delay feedback), so the two agree to rounding at every resonance.
  * Linkwitz-Riley crossover: LR4 = a 2nd-order Butterworth squared
    (scipy.signal.butter); its high-pass is the squared Butterworth high-pass.
    LP4 + HP4 is an all-pass (flat magnitude). A tree's band j is the high-pass
    of every earlier split, the low-pass of its own and the all-pass of every
    later split, so the bands stay in phase.
  * First-order all-pass (the phaser stage): magnitude 1 and phase
    -2 atan(tan(pi f / fs) / tan(pi fc / fs)) per stage, from the analog
    (1 - s/wc)/(1 + s/wc) under the same prewarped bilinear map.
  * Resampler: the ideal band-limited converter has unit gain below the
    smaller Nyquist and none above; scipy.signal.resample_poly is run alongside
    as a cross-check of the method (printed, not judged).

An impulse response is compared by evaluating its DTFT directly on a log grid
(20 Hz to 0.45 * rate), which is the zero-padded FFT without the bin grid. Only
bins where the reference is above -60 dB are compared in magnitude (a deep
null is dominated by rounding, not by the filter) and above -40 dB in phase.

Exits non-zero if any case fails.
"""
import json
import sys
from fractions import Fraction
from pathlib import Path

import numpy as np
import soundfile as sf
from scipy import signal

GRID_POINTS = 400
MAG_FLOOR_DB = -60.0
PHASE_FLOOR_DB = -40.0


def log_grid(rate):
    return np.geomspace(20.0, 0.45 * rate, GRID_POINTS)


def dtft(h, freqs, rate):
    """H(f) = sum h[n] exp(-j 2 pi f n / rate), exactly, in chunks of frequencies."""
    n = np.arange(len(h))
    out = np.empty(len(freqs), dtype=complex)
    for i in range(0, len(freqs), 40):
        w = 2 * np.pi * freqs[i : i + 40] / rate
        out[i : i + 40] = np.exp(-1j * np.outer(w, n)) @ h
    return out


def db(x):
    return 20 * np.log10(np.maximum(np.abs(x), 1e-30))


# ---- the RBJ Audio EQ Cookbook ------------------------------------------------

def rbj(kind, f0, q, gain_db, rate):
    """(b, a) of the cookbook biquad `kind`, normalised by a0."""
    w0 = 2 * np.pi * f0 / rate
    c, s = np.cos(w0), np.sin(w0)
    alpha = s / (2 * q)
    A = 10 ** (gain_db / 40)
    sa = 2 * np.sqrt(A) * alpha
    if kind == "lowpass":
        b, a = [(1 - c) / 2, 1 - c, (1 - c) / 2], [1 + alpha, -2 * c, 1 - alpha]
    elif kind == "highpass":
        b, a = [(1 + c) / 2, -(1 + c), (1 + c) / 2], [1 + alpha, -2 * c, 1 - alpha]
    elif kind == "bandpass":
        # The cookbook's "constant skirt gain, peak gain = Q" form (sin(w0)/2 = Q alpha).
        # Its other form, constant 0 dB peak gain, is this divided by Q; SvfType::BandPass
        # is the first (the SVF's band-pass state is s/(s^2 + s/Q + 1) * 1/(1/Q), see
        # findings in the README).
        b, a = [alpha * q, 0, -alpha * q], [1 + alpha, -2 * c, 1 - alpha]
    elif kind == "notch":
        b, a = [1, -2 * c, 1], [1 + alpha, -2 * c, 1 - alpha]
    elif kind == "allpass":
        b, a = [1 - alpha, -2 * c, 1 + alpha], [1 + alpha, -2 * c, 1 - alpha]
    elif kind == "bell":
        b = [1 + alpha * A, -2 * c, 1 - alpha * A]
        a = [1 + alpha / A, -2 * c, 1 - alpha / A]
    elif kind == "lowshelf":
        b = [A * ((A + 1) - (A - 1) * c + sa), 2 * A * ((A - 1) - (A + 1) * c),
             A * ((A + 1) - (A - 1) * c - sa)]
        a = [(A + 1) + (A - 1) * c + sa, -2 * ((A - 1) + (A + 1) * c),
             (A + 1) + (A - 1) * c - sa]
    elif kind == "highshelf":
        b = [A * ((A + 1) + (A - 1) * c + sa), -2 * A * ((A - 1) + (A + 1) * c),
             A * ((A + 1) + (A - 1) * c - sa)]
        a = [(A + 1) - (A - 1) * c + sa, 2 * ((A - 1) - (A + 1) * c),
             (A + 1) - (A - 1) * c - sa]
    else:
        raise ValueError(kind)
    b, a = np.array(b, float), np.array(a, float)
    return b / a[0], a / a[0]


def ref_svf(case, freqs):
    b, a = rbj(case["type"], case["freq"], case["q"], case["gain_db"], case["rate"])
    return signal.freqz(b, a, worN=freqs, fs=case["rate"])[1]


# ---- the ladder ---------------------------------------------------------------

def ref_ladder(case, freqs):
    warp = np.tan(np.pi * freqs / case["rate"]) / np.tan(np.pi * case["freq"] / case["rate"])
    s = 1j * warp
    return 1.0 / ((1 + s) ** 4 + 4 * case["res"])


# ---- Linkwitz-Riley -----------------------------------------------------------

def lr_parts(fc, freqs, rate):
    """(LP4, HP4, all-pass) of one split, from squared Butterworth sections."""
    out = {}
    for kind in ("lowpass", "highpass"):
        sos = signal.butter(2, fc, btype="low" if kind == "lowpass" else "high",
                            fs=rate, output="sos")
        h = signal.sosfreqz(sos, worN=freqs, fs=rate)[1]
        out[kind] = h * h
    return out["lowpass"], out["highpass"], out["lowpass"] + out["highpass"]


def ref_bands(splits, freqs, rate):
    parts = [lr_parts(fc, freqs, rate) for fc in splits]
    bands = []
    for j in range(len(splits) + 1):
        h = np.ones(len(freqs), dtype=complex)
        for i, (lp, hp, ap) in enumerate(parts):
            if i < j:
                h = h * hp
            elif i == j:
                h = h * lp
            else:
                h = h * ap
        bands.append(h)
    return bands


# ---- the all-pass chain -------------------------------------------------------

def ref_allpass(case, freqs):
    t = np.tan(np.pi * freqs / case["rate"]) / np.tan(np.pi * case["freq"] / case["rate"])
    return np.exp(-1j * 2 * case["stages"] * np.arctan(t))


# ---- comparison ---------------------------------------------------------------

def compare(got, ref, freqs):
    """(max |dB error|, its frequency, max |phase error| in degrees, its frequency)."""
    mag_ok = db(ref) > MAG_FLOOR_DB
    err = np.abs(db(got) - db(ref))
    err[~mag_ok] = 0
    i = int(np.argmax(err))
    ph_ok = db(ref) > PHASE_FLOOR_DB
    dphi = np.degrees(np.angle(got / ref))  # wraps to (-180, 180]
    dphi = np.where(ph_ok, np.abs(dphi), 0)
    j = int(np.argmax(dphi))
    return err[i], freqs[i], dphi[j], freqs[j]


rows = []

# Cases whose disagreement with the reference is understood and is a property of tutti's
# design, not of the reference. They are printed as KNOWN and do not fail the run; every
# other FAIL does. A KNOWN case that passes fails the run (a stale excuse, like a
# contract `Gap` that no longer fails): delete its entry when the cause is removed.
KNOWN = {}


def record(name, mag_err, mag_f, mag_tol, ph_err=None, ph_f=None, ph_tol=None, note=""):
    ok = mag_err <= mag_tol and (ph_err is None or ph_err <= ph_tol)
    if name in KNOWN:
        if ok:
            ok, note = False, "KNOWN but passes: remove it from KNOWN"
        else:
            ok, note = None, "known finding: " + KNOWN[name]
    rows.append((name, mag_err, mag_f, mag_tol, ph_err, ph_f, ph_tol, ok, note))


def judge_response(name, got, ref, freqs, mag_tol, ph_tol, note=""):
    e, ef, p, pf = compare(got, ref, freqs)
    record(name, e, ef, mag_tol, p, pf, ph_tol, note)


# ---- resampler ----------------------------------------------------------------

def tone_amp(x, rate, f):
    """Amplitude of the sine at f in the middle 80% of x, Hann-windowed projection."""
    n = len(x)
    seg = x[int(0.1 * n) : int(0.9 * n)]
    t = np.arange(len(seg)) / rate
    w = np.hanning(len(seg))
    return 2 * abs(np.sum(seg * w * np.exp(-2j * np.pi * f * t))) / np.sum(w)


def peak_freq(x, rate):
    seg = x[int(0.1 * len(x)) : int(0.9 * len(x))] * np.hanning(int(0.9 * len(x)) - int(0.1 * len(x)))
    spec = np.abs(np.fft.rfft(seg, 16 * len(seg)))
    return np.argmax(spec) * rate / (16 * len(seg))


def judge_resample(case, d):
    x, rin = sf.read(d / case["file_in"])
    y, rout = sf.read(d / case["file_out"])
    from_, to = case["from"], case["to"]
    freqs = case["freqs"]
    nyq = 0.5 * min(from_, to)
    f_ref = Fraction(int(to), int(from_))
    ref = signal.resample_poly(x, f_ref.numerator, f_ref.denominator)
    name = case["name"]
    length_err = len(y) - round(len(x) * to / from_)
    if case["label"] == "passband":
        # Ripple: every tone in 40 Hz .. 0.9 * the lower Nyquist must keep its
        # amplitude (the input's tones are equal, so the ratio is the gain).
        # Tolerance 0.1 dB: an export converter's ripple should be inaudible
        # (about 0.1 dB is the usual audibility limit for a broadband gain).
        gains = np.array([db(tone_amp(y, rout, f) / tone_amp(x, rin, f)) for f in freqs])
        gref = np.array([db(tone_amp(ref, rout, f) / tone_amp(x, rin, f)) for f in freqs])
        fr = np.array(freqs)
        band = fr <= 0.9 * nyq
        e = np.abs(gains) * band
        i = int(np.argmax(e))
        record(name + ":ripple", e[i], fr[i], 0.1,
               note=f"resample_poly max {np.max(np.abs(gref) * band):.4f} dB; "
                    f"edge {gains[-1]:+.2f} dB at {fr[-1]:.0f} Hz")
        # Tone frequency preserved: peak of the output spectrum of each tone alone is
        # not separable in a multisine, so check the strongest bin's tone is one of the
        # input tones, to 0.5 Hz (1 s of signal resolves 1 Hz).
        pk = peak_freq(y, rout)
        d_f = min(abs(pk - f) for f in freqs)
        record(name + ":freqs", d_f, pk, 0.5, note="strongest output tone is an input tone (Hz error)")
        record(name + ":length", abs(length_err), 0.0, 1.0, note="output frames vs round(in * to/from)")
    elif case["label"] == "edge":
        # Where the converter's band edge is: the first tone more than 3 dB down
        # (linear interpolation between tones). The audio band must pass with margin:
        # the -3 dB point is required at or above 0.92 * Nyquist (20.3 kHz at 44.1 kHz).
        fr = np.array(freqs)
        g = np.array([db(tone_amp(y, rout, f) / tone_amp(x, rin, f)) for f in fr])
        k = int(np.argmax(g < -3.0)) if np.any(g < -3.0) else len(g) - 1
        f3 = np.interp(-3.0, [g[k], g[k - 1]], [fr[k], fr[k - 1]]) if k > 0 else fr[0]
        rows.append((name + ":edge", g[0], fr[0], 0.1, None, None, None, f3 >= 0.92 * nyq,
                     f"-3 dB point {f3:.0f} Hz, needs >= {0.92 * nyq:.0f} Hz; "
                     "dB column is the gain at the lowest tone"))
    else:
        f = freqs[0]
        alias = abs(to - f) if f > 0.5 * to else f  # folded frequency in the output
        amp_in = tone_amp(x, rin, f)
        # An ideal converter removes it entirely. Required: at least 20 dB down
        # 450 Hz above the target Nyquist (inside any real transition band),
        # 60 dB down from 1.4 kHz above.
        att = db(tone_amp(y, rout, alias) / amp_in)
        tol = -20.0 if f - 0.5 * to < 1_000 else -60.0
        attr = db(tone_amp(ref, rout, alias) / amp_in)
        ok = att <= tol
        rows.append((name + ":stopband", att, alias, tol, None, None, None, ok,
                     f"{f:.0f} Hz folds to {alias:.0f} Hz; resample_poly {attr:.1f} dB; "
                     "value is attenuation, pass if <= bound"))


def main():
    d = Path(sys.argv[1])
    cases = json.loads((d / "cases.json").read_text())
    for case in cases:
        kind = case["kind"]
        if kind == "resample":
            judge_resample(case, d)
            continue
        data, rate = sf.read(d / case["file"], always_2d=True)
        assert rate == case["rate"]
        freqs = log_grid(rate)
        got = [dtft(data[:, c], freqs, rate) for c in range(data.shape[1])]
        name = case["name"]
        if kind == "svf":
            # Same transfer function, so the only differences are rounding: 0.01 dB
            # and 0.1 degrees for f64 state. f32 state carries f32 rounding in the
            # recursion: 0.05 dB, 0.5 degrees.
            f64 = case["precision"] == "f64"
            judge_response(name, got[0], ref_svf(case, freqs), freqs,
                           0.01 if f64 else 0.05, 0.1 if f64 else 0.5)
        elif kind == "ladder":
            h = got[0] / case["amplitude"]
            ref = ref_ladder(case, freqs)
            # The loop is closed without a delay (zero-delay feedback), so the render is
            # the bilinear image of the continuous ladder, with or without resonance:
            # exact to rounding, 0.01 dB and 0.1 degrees over the whole band.
            judge_response(name, h, ref, freqs, 0.01, 0.1)
        elif kind == "crossover":
            refs = ref_bands(case["splits"], freqs, rate)
            for b, (g, r) in enumerate(zip(got, refs)):
                judge_response(f"{name}:band{b}", g, r, freqs, 0.01, 0.1)
            total = sum(got)
            # The sum is an all-pass: flat magnitude to rounding.
            e = np.abs(db(total))
            record(f"{name}:sum_flat", e.max(), freqs[int(np.argmax(e))], 0.01,
                   note="|sum of bands| = 0 dB")
        elif kind == "allpass_chain":
            # f32 recursion with a coefficient near -1 for the low centres.
            judge_response(name, got[0], ref_allpass(case, freqs), freqs, 0.01, 0.2)
        else:
            raise SystemExit(f"unknown case kind {kind}")

    w = max(len(r[0]) for r in rows)
    print(f"{'case':<{w}}  max|dB|  at Hz      tol dB  phase deg  at Hz      tol deg  result")
    failed = known = 0
    for name, e, ef, tol, pe, pf, ptol, ok, note in rows:
        ph = f"{pe:9.4f}  {pf:9.1f}  {ptol:7.2f}" if pe is not None else " " * 9 + "  " + " " * 9 + "  " + " " * 7
        print(f"{name:<{w}}  {e:7.4f}  {ef:9.1f}  {tol:6.2f}  {ph}  {'KNOWN' if ok is None else 'PASS' if ok else 'FAIL'}"
              + (f"  [{note}]" if note else ""))
        known += ok is None
        failed += ok is False
    print(f"\n{len(rows) - failed - known}/{len(rows)} passed, {known} known findings, {failed} failed")
    sys.exit(1 if failed else 0)


main()
