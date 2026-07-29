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

//------------------------------------------------------------------------
} // namespace Vst
} // namespace Steinberg
