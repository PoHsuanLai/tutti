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

    /// `out[..][i] = kEventBusActiveCode` when this plugin's event input bus
    /// was activated by the host, `kEventBusInactiveCode` when it was not.
    ///
    /// The spec starts every bus inactive (`ivstcomponent.h:52`, unqualified —
    /// `kEvent` is a `MediaTypes` value beside `kAudio`), so a host must call
    /// `activateBus` for event buses just as it does for audio ones. Every
    /// other mode here is deliberately *lenient*: it reads `data.inputEvents`
    /// whatever the bus state, which is what most real plugins do and is
    /// exactly why a host can omit the call and still appear to work.
    ///
    /// This mode is the strict counterpart, and it exists because the omission
    /// is otherwise unobservable: `BusInfo` carries no "is active" field, so a
    /// host cannot read its own activation back, and `HostChecker` validates
    /// the `ProcessData` handed over rather than the lifecycle that preceded
    /// it. Only the plugin knows, so only the plugin can report it.
    kModeEventBusActive = 6,

    /// `out[..][i] = kConnectBalanceBase + (connects - disconnects)` on the
    /// processor half.
    ///
    /// Answers the one question a half-refused `connect` raises: was this half
    /// left holding a peer the other half never accepted? The base class keeps
    /// only the current pointer, which cannot tell "never connected" from
    /// "connected then correctly unwound" — so the count, not the pointer, is
    /// the observable.
    kModeConnectBalance = 7,

    /// `out[..][i] = kActivationCountBase + (number of setActive(true) calls)`.
    ///
    /// `kIoChanged` and `kLatencyChanged` both oblige the host to deactivate,
    /// re-ask, and reactivate (`ivsteditcontroller.h:125-127`, `:137-138`).
    /// Whether that happened is invisible from the host side — the bus counts
    /// read back the same either way, because this plugin's layout does not
    /// actually change. Counting activations is what separates "re-read the
    /// buses in place" from "ran the cycle".
    kModeActivationCount = 8,

    /// `out[..][i] = kAudioBusActiveBase + bitmask of active audio buses`.
    ///
    /// Bit 0 = input bus 0 (main), bit 1 = input bus 1 (aux), bit 2 = output
    /// bus 0 (main), bit 3 = output bus 1 (aux).
    ///
    /// A host's activation *policy* is otherwise invisible. `BusInfo` carries
    /// no active field, so the host cannot read its own decision back, and the
    /// audio a bus carries does not depend on it — this plugin writes its aux
    /// output whether or not the bus was activated, exactly as a lenient real
    /// plugin does. Only the plugin knows, so only the plugin can report it.
    kModeAudioBusActive = 9,

    /// `out[..][i] = kSetupWhileActiveBase + (setupProcessing calls received
    /// while this plugin was active)`.
    ///
    /// `setupProcessing` is documented *"Called in disable state (setActive not
    /// called with true) before setProcessing is called and processing will
    /// begin"* (`ivstaudioprocessor.h:328-330`). A host that re-runs it on a
    /// live instance — to change the sample rate, say — has the plugin
    /// re-deriving coefficients and re-sizing buffers underneath a `process`
    /// that may be in flight.
    ///
    /// Invisible from the host side, which is why it needs a mode of its own:
    /// `setupProcessing` returns the same `kResultOk` either way, and every
    /// value the host can read back afterwards (rate, block size, bus counts)
    /// is what it just wrote. Only the plugin sees the *order*, so only the
    /// plugin can report it. Every real plugin is lenient here for the same
    /// reason `kModeAudioBusActive` exists: it simply stores the setup whenever
    /// it arrives, which is exactly why a host can violate this and appear to
    /// work.
    kModeSetupWhileActive = 10,
};

/// Step count for `kParamMode`. A stepped VST3 parameter normalizes as
/// `index / stepCount`, so this is `highest mode index`, not the mode count.
///
/// It lives here because three places must agree — the controller's
/// `addParameter`, the controller's `setComponentState`, and the processor's
/// decode — and the host's `MODE_STEPS` mirrors it. Disagreement does not fail
/// to compile: it selects a *different mode* than the caller asked for, and
/// every assertion then reads the wrong renderer's output.
static const int32 kModeStepCount = kModeSetupWhileActive;

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
    /// Writing any non-zero value makes the *controller* ask the host for
    /// `restartComponent(kIoChanged)`. Lets a test trigger the restart path on
    /// demand instead of waiting for a plugin that happens to reconfigure.
    kParamRequestIoChanged = 103,

    /// Controller-only UI state, standing in for a scroll position or a
    /// selected tab. The **processor never sees it**: it is written and read
    /// solely by `IEditController::setState`/`getState`, and is absent from the
    /// component's stream and from `setComponentState`.
    ///
    /// That is what makes it an observable. A host that persists only the
    /// component stream restores this to its default no matter what was saved,
    /// so the round trip below is the one test that can tell the two streams
    /// apart.
    kParamUiState = 104,
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

/// `kModeEventBusActive` writes one of these. Distinct from every other code
/// above and from any plausible audio sample, so a host reading the wrong
/// buffer cannot land on either by accident.
static const int32 kEventBusActiveCode = 7000;
static const int32 kEventBusInactiveCode = -7000;

/// `kModeConnectBalance` writes `kConnectBalanceBase + balance`. Offset rather
/// than reported raw so a balance of 0 is distinguishable from a mode that
/// never ran and left the buffer zeroed.
static const int32 kConnectBalanceBase = 8000;

/// `kModeActivationCount` writes `kActivationCountBase + activations`. Offset
/// for the same reason as the balance above, and far enough from it that the
/// two modes' outputs are never confusable.
static const int32 kActivationCountBase = 9000;

/// `kModeAudioBusActive` writes `kAudioBusActiveBase + mask`, where the mask
/// spans `0..=15`. Offset so an all-inactive mask of 0 is distinguishable from
/// a mode that never ran and left the buffer zeroed, and spaced clear of the
/// bases above.
static const int32 kAudioBusActiveBase = 10000;

/// `kModeSetupWhileActive` writes `kSetupWhileActiveBase + violations`. Offset
/// for the same reason as the bases above — a count of 0 is the *passing*
/// answer, so it must be distinguishable from a mode that never ran and left
/// the buffer zeroed — and spaced clear of them.
static const int32 kSetupWhileActiveBase = 11000;

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

    /// `getState`/`setState` return `kNotImplemented`.
    ///
    /// **Not a violation at all.** This is what the SDK's own `Component` base
    /// returns (`vstcomponent.cpp:159,165`), so every plugin that does not
    /// override state does exactly this. It lives here because the host was
    /// *rejecting* it: the tolerance list was `kResultOk || kResultFalse`,
    /// which is neither what the SDK returns nor what the SDK's own preset
    /// writer accepts — `vstpresetfile.cpp`'s `verify` takes
    /// `kResultOk || kNotImplemented`.
    kMisbehaveStateNotImplemented = 8,

    /// `IComponent::initialize` returns `kResultFalse`.
    ///
    /// A refusal, like `setActive`: the plugin is declining to initialise, and
    /// the host must not go on to use a component that never came up. The SDK's
    /// own host requires `== kResultOk` here (`plugprovider.cpp:140`).
    kMisbehaveInitializeFails = 9,

    /// The *controller* half refuses `IConnectionPoint::connect`, after the
    /// component half has already accepted.
    ///
    /// Wiring the two halves takes two calls, and a plugin may take the first
    /// and refuse the second. Both returns were discarded, so the pair was
    /// left asymmetric — the component holding a peer that never reciprocated
    /// — and initialisation carried on. The host must unwind the half that
    /// took rather than leave the plugin in a state it cannot reach from any
    /// legal call sequence.
    ///
    /// The controller is chosen as the refusing end deliberately: refusing at
    /// the *component* end fails the first call, which needs no unwind and so
    /// exercises nothing.
    kMisbehaveControllerConnectFails = 10,

    /// `setBusArrangements` returns `kResultFalse` and the plugin *keeps a
    /// layout that differs from the one proposed* — the main input narrows to
    /// mono.
    ///
    /// Not a violation: `ivstaudioprocessor.h` documents `kResultFalse` as
    /// "the plugin did not accept your arrangement, it kept its own", and the
    /// host is then required to read the kept layout back with
    /// `getBusArrangement`. Plugins with fixed I/O (a mono-only analyser, a
    /// hardwired upmixer) do exactly this.
    ///
    /// The *narrowing* is the whole point. A plugin that refuses but keeps the
    /// layout the host happened to propose is indistinguishable from one that
    /// accepted, so it witnesses nothing: the host reads back the same numbers
    /// it already had. Only a divergence between "what the host proposed" and
    /// "what the plugin kept" can catch a host that reports the former while
    /// rendering the latter.
    ///
    /// Mono is chosen because the probe's main input is stereo, so the change
    /// is a *narrowing* — a host that keeps the proposed width would over-read
    /// a channel the plugin is not running.
    kMisbehaveArrangementRefused = 11,
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
    // The upper bound must name the *last* variant: an appended misbehaviour
    // whose value falls outside this range is silently read as `kMisbehaveNone`,
    // so the plugin behaves and the test passes against a probe that never
    // misbehaved.
    return (v >= kMisbehaveNone && v <= kMisbehaveArrangementRefused) ? v : kMisbehaveNone;
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
