//-----------------------------------------------------------------------------
// Audio-correctness probe processor.
//-----------------------------------------------------------------------------

#pragma once

#include "probeids.h"
#include "public.sdk/source/vst/vstaudioeffect.h"

#include <vector>

namespace Steinberg {
namespace Vst {

//-----------------------------------------------------------------------------
class AudioProbeProcessor : public AudioEffect
{
public:
	AudioProbeProcessor ();

	tresult PLUGIN_API initialize (FUnknown* context) SMTG_OVERRIDE;
	tresult PLUGIN_API setBusArrangements (SpeakerArrangement* inputs, int32 numIns,
	                                       SpeakerArrangement* outputs,
	                                       int32 numOuts) SMTG_OVERRIDE;
	tresult PLUGIN_API canProcessSampleSize (int32 symbolicSampleSize) SMTG_OVERRIDE;
	uint32 PLUGIN_API getLatencySamples () SMTG_OVERRIDE;
	tresult PLUGIN_API setActive (TBool state) SMTG_OVERRIDE;
	tresult PLUGIN_API setupProcessing (ProcessSetup& setup) SMTG_OVERRIDE;
	tresult PLUGIN_API process (ProcessData& data) SMTG_OVERRIDE;

	/// Overridden only to support `kMisbehaveExtraBuses`; otherwise defers to
	/// the base. A host that trusts an inflated count and indexes `getBusInfo`
	/// up to it reads past the plugin's own bus array.
	int32 PLUGIN_API getBusCount (MediaType type, BusDirection dir) SMTG_OVERRIDE;

	/// Counted so `kModeConnectBalance` can report whether this half was left
	/// holding a peer the other half never accepted. The base class keeps only
	/// the current pointer, which cannot distinguish "never connected" from
	/// "connected then correctly unwound" — and that distinction is the whole
	/// question a half-refused connect raises.
	tresult PLUGIN_API connect (IConnectionPoint* other) SMTG_OVERRIDE;
	tresult PLUGIN_API disconnect (IConnectionPoint* other) SMTG_OVERRIDE;

	tresult PLUGIN_API setState (IBStream* state) SMTG_OVERRIDE;
	tresult PLUGIN_API getState (IBStream* state) SMTG_OVERRIDE;

	static FUnknown* createInstance (void*) { return (IAudioProcessor*)new AudioProbeProcessor (); }

private:
	/// One render pass, generic over sample type so f32 and f64 share exactly
	/// one implementation — a divergence between the two would otherwise be
	/// invisible to a test that only exercises one.
	template <typename T>
	void renderBlock (ProcessData& data);

	/// Apply queued parameter changes. Returns the ramp value at each sample
	/// via `mRampAt`, sized to the block.
	void consumeParameterChanges (ProcessData& data);

	/// Sample offset of the last note-on seen this block, or -1.
	int32 firstNoteOnOffset (ProcessData& data) const;

	/// Whether the host activated our event input bus.
	///
	/// Read from the base class's own bus list — the one `activateBus` writes —
	/// so it reports what the host actually did rather than inferring it from
	/// whether events arrived. VST3 offers a host no way to read this back
	/// (`BusInfo` has no active field), which is why the plugin must report it.
	bool eventInputActive () const;

	/// `connect` calls minus `disconnect` calls on this half. 1 means the host
	/// left us joined to a peer; 0 means never joined, or joined and unwound.
	int32 mConnectBalance {0};

	/// `setActive(true)` calls seen. Deliberately **not** reset in `setActive`,
	/// unlike the block counter and delay line beside it — the whole point is
	/// to survive a deactivate/reactivate cycle so a host can be asked whether
	/// it ran one.
	int32 mActivationCount {0};

	int32 mMode {kModeTagPassthrough};
	double mRamp {0.0};
	double mGain {1.0};

	/// Which spec violation to commit, from `TUTTI_PROBE_MISBEHAVIOUR`.
	///
	/// Latched once at construction rather than read per call: a host test sets
	/// the variable, loads, asserts, then unsets it, and re-reading would make
	/// the plugin's behaviour depend on when each call happened relative to the
	/// test's cleanup.
	int32 mMisbehaviour {kMisbehaveNone};

	/// Ramp value per sample for the current block (see `kModeParamRamp`).
	std::vector<double> mRampAt;

	/// Blocks processed since `setActive(true)` — the `kModeBlockCounter`
	/// clock. Reset on activation so a test gets a deterministic origin.
	int64 mBlockIndex {0};

	/// Delay line for `kModeLatency`, one ring per channel.
	std::vector<std::vector<double>> mDelay;
	int32 mDelayPos {0};
};

//------------------------------------------------------------------------
} // namespace Vst
} // namespace Steinberg
