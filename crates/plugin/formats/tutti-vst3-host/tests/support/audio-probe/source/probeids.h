//-----------------------------------------------------------------------------
// Audio-correctness probe — shared ids between processor, controller and host.
//
// This plugin is a *test oracle*, not an audio effect. Its output is a closed-
// form function of what the host handed it, so a host test can assert exact
// sample values instead of merely checking that the ProcessData struct was
// shaped correctly. Structural conformance (HostChecker) proves the host built
// a legal call; this proves the host wired the right samples to the right
// place.
//-----------------------------------------------------------------------------

#pragma once

#include "pluginterfaces/base/funknown.h"
#include "pluginterfaces/vst/vsttypes.h"

#include <cstdlib>

namespace Steinberg {
namespace Vst {

static const FUID AudioProbeProcessorUID (0x9A1B2C3D, 0x4E5F6071, 0x82939AAB, 0xBCCDDEEF);
static const FUID AudioProbeControllerUID (0x1122A3B4, 0xC5D6E7F8, 0x90A1B2C3, 0xD4E5F607);

/// What the plugin writes into its output buffers. Selected by
/// `kParamMode`, so one binary covers every assertion the host wants to make.
enum ProbeMode : int32
{
    /// `out[bus][ch][i] = in[bus][ch][i] + tag(bus, ch)`.
    ///
    /// The tag makes every (bus, channel) slot uniquely identifiable, so a host
    /// that crosses channels, swaps buses, or routes an aux bus onto the main
    /// one produces arithmetically wrong samples rather than merely
    /// suspicious-looking ones. This is the routing oracle.
    kModeTagPassthrough = 0,

    /// `out[..][i] = value of kParamRamp at sample i`.
    ///
    /// Materialises the host's automation curve as audio. Feeding two parameter
    /// points and comparing against the expected interpolation catches
    /// unsorted, dropped, or mistimed automation — the class of bug where the
    /// ProcessData is structurally legal but the ramp runs backwards.
    kModeParamRamp = 1,

    /// `out[..][i] = blockIndex + i / frames`.
    ///
    /// A strictly increasing global sample clock. Duplicated, dropped or
    /// reordered blocks show up as a discontinuity the host test can locate
    /// exactly.
    kModeBlockCounter = 2,

    /// `out[..][i] = in[..][i]` delayed by exactly `kReportedLatencySamples`.
    ///
    /// The plugin also reports that latency via `getLatencySamples`, so a host
    /// that applies delay compensation lines the impulse back up and one that
    /// ignores latency does not.
    kModeLatency = 3,

    /// `out[..][i] = 1.0` from the sample offset of the most recent note-on
    /// onward, 0 before it; reset each block.
    ///
    /// Turns MIDI event timing into an edge position the host can measure to
    /// the sample, rather than trusting that the event list merely looked right.
    kModeNoteGate = 4,

    /// Renders a *transcript* of the event list: at each event's sample offset
    /// the output carries a code identifying the event, and 0 elsewhere.
    ///
    /// Note-on is `+(pitch + 1)`, note-off is `-(pitch + 1)`, and any other
    /// event type is `kEventOtherCode`. That makes offset, kind, and pitch all
    /// recoverable from the audio, so a host that drops note-offs, reorders
    /// events, mangles a pitch, or collapses two events onto one offset is
    /// caught — none of which the note-gate mode can see.
    kModeEventTranscript = 5,
};

/// Parameter ids. Deliberately nonzero and non-contiguous: a host that
/// confuses parameter *index* with parameter *id* passes with 0,1,2 and fails
/// here (and HostChecker's own duplicate-id check misfires on id 0).
enum ProbeParams : ParamID
{
    kParamMode = 100,
    /// The value `kModeParamRamp` renders into the output.
    kParamRamp = 101,
    /// Read back by the host to confirm parameter writes land.
    kParamGain = 102,
};

/// Latency the plugin reports and applies in `kModeLatency`. A prime number so
/// an accidentally-correct result is unlikely.
static const int32 kReportedLatencySamples = 137;

/// Code written by `kModeEventTranscript` for an event that is neither a
/// note-on nor a note-off. Distinct from any `±(pitch + 1)`, which spans
/// `±1..=128`.
static const int32 kEventOtherCode = 1000;

/// Base for `kModeEventTranscript`'s note-expression encoding: the rendered
/// sample is `kNoteExpressionBaseCode + typeId + value`. Far from the
/// `±1..=128` note codes and from `kEventOtherCode`, so the three are never
/// confusable.
static const int32 kNoteExpressionBaseCode = 5000;

/// Per-slot DC offset in `kModeTagPassthrough`.
///
/// Chosen so every (bus, channel) pair maps to a distinct, exactly
/// representable float, and so the values are large enough that a
/// misrouted slot can never be mistaken for signal.
inline double probeTag (int32 busIndex, int32 channelIndex)
{
    return static_cast<double> (busIndex) * 1000.0 + static_cast<double> (channelIndex) + 1.0;
}

//-----------------------------------------------------------------------------
// Misbehaviour
//
// Everything above makes the probe a *well-behaved* oracle. These make it a
// badly-behaved one, so the host can be tested against plugins that violate the
// spec — the thing no Steinberg sample will ever do, and the reason a corpus of
// only well-behaved plugins proves so little about robustness.
//
// Selected by the `TUTTI_PROBE_MISBEHAVIOUR` environment variable, read once
// when the processor is constructed. It cannot be a parameter: most of these
// happen during `initialize`/`setActive`, before a host could set one. The
// variable is read in the *host test's* own process (the plugin is loaded
// in-process), so a test sets it, loads, asserts, and unsets.
//
// Each value names a real, observed class of plugin bug. A host that survives
// all of them will not be taken down by a plugin that does one of them.
//-----------------------------------------------------------------------------

/// Name of the environment variable selecting a misbehaviour.
#define kProbeMisbehaviourEnv "TUTTI_PROBE_MISBEHAVIOUR"

enum ProbeMisbehaviour : int32
{
    /// Behave. The default whenever the variable is unset or unrecognised.
    kMisbehaveNone = 0,

    /// `setActive(true)` returns `kResultFalse`.
    ///
    /// Plugins do this when they cannot claim a resource (a licence check, an
    /// audio device, a dongle). The host must surface it as an error rather
    /// than proceeding to `process` a plugin that never activated.
    kMisbehaveSetActiveFails = 1,

    /// `getLatencySamples` reports a latency the plugin does not apply.
    ///
    /// Common in the wild: the value is stale, or reported in milliseconds
    /// rather than samples. The host must not crash or mis-size buffers; PDC
    /// being wrong is the plugin's fault, but a *crash* would be the host's.
    kMisbehaveLatencyLies = 2,

    /// `getBusCount` reports more buses than were ever added.
    ///
    /// The classic out-of-bounds trigger: a host that trusts the count and
    /// indexes `getBusInfo`/`activateBus` up to it walks off the end of the
    /// plugin's own array. The host must clamp to what it can actually resolve.
    kMisbehaveExtraBuses = 3,

    /// `process` returns `kResultFalse` on every call.
    ///
    /// Legal per the spec and used by plugins that have nothing to render. The
    /// host must keep running and must not treat the output buffers as
    /// containing meaningful audio.
    kMisbehaveProcessFails = 4,

    /// `process` writes nothing at all, leaving output buffers untouched.
    ///
    /// A host that assumes its output scratch was filled will forward whatever
    /// was previously in that memory — the classic stale-buffer leak, which
    /// sounds like a burst of an earlier signal.
    kMisbehaveProcessWritesNothing = 5,

    /// `getState` fails and `setState` rejects everything.
    ///
    /// The host must treat a state round-trip as best-effort rather than
    /// failing the whole load.
    kMisbehaveStateFails = 6,

    /// `setupProcessing` returns `kResultFalse`.
    ///
    /// Plugins that only support certain sample rates or block sizes do this.
    /// The host currently *tolerates* `kResultFalse` here by design; this makes
    /// that tolerance a tested decision rather than an untested one.
    kMisbehaveSetupFails = 7,
};

/// Read the selected misbehaviour from the environment. Returns
/// [`kMisbehaveNone`] when unset, empty, or not a recognised number.
///
/// Defined here rather than in the .cpp so the host test and the plugin cannot
/// disagree about the variable's name or its encoding.
inline int32 probeMisbehaviour ()
{
    const char* raw = std::getenv (kProbeMisbehaviourEnv);
    if (!raw || !*raw)
        return kMisbehaveNone;
    const int32 v = static_cast<int32> (std::strtol (raw, nullptr, 10));
    return (v >= kMisbehaveNone && v <= kMisbehaveSetupFails) ? v : kMisbehaveNone;
}

/// Bus count reported under [`kMisbehaveExtraBuses`]. Larger than any real
/// count so the overreport is unambiguous, but small enough that a host which
/// (wrongly) allocates per reported bus does not exhaust memory before the
/// test can observe it.
static const int32 kLyingBusCount = 64;

/// Latency claimed under [`kMisbehaveLatencyLies`] but never applied. Distinct
/// from `kReportedLatencySamples` so the two are never confusable.
static const int32 kLiedLatencySamples = 9001;

//------------------------------------------------------------------------
} // namespace Vst
} // namespace Steinberg
