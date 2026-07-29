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
