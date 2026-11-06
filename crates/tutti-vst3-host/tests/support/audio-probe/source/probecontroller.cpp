//-----------------------------------------------------------------------------
// Audio-correctness probe controller.
//-----------------------------------------------------------------------------

#include "probecontroller.h"

#include "base/source/fstreamer.h"
#include "pluginterfaces/base/ibstream.h"

namespace Steinberg {
namespace Vst {

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeController::setParamNormalized (ParamID id, ParamValue value)
{
	const tresult r = EditController::setParamNormalized (id, value);
	if (id == kParamRequestIoChanged && value != 0.0 && componentHandler)
		componentHandler->restartComponent (kIoChanged);
	return r;
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeController::connect (IConnectionPoint* other)
{
	// Latched per-call rather than at construction: the controller is built
	// before the host reads the environment in some load orders, and this
	// misbehaviour only has to be true at the moment `connect` is called.
	if (probeMisbehaviour () == kMisbehaveControllerConnectFails)
		return kResultFalse;
	return EditController::connect (other);
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeController::initialize (FUnknown* context)
{
	tresult result = EditController::initialize (context);
	if (result != kResultOk)
		return result;

	// Ids are deliberately nonzero and non-contiguous — see `ProbeParams`.
	// `kParamMode` is stepped so a host that rounds normalized values
	// differently still lands on an exact mode.
	parameters.addParameter (STR16 ("Mode"), nullptr, kModeStepCount, 0.0,
	                         ParameterInfo::kCanAutomate, kParamMode);
	parameters.addParameter (STR16 ("Ramp"), nullptr, 0, 0.0, ParameterInfo::kCanAutomate,
	                         kParamRamp);
	parameters.addParameter (STR16 ("Gain"), nullptr, 0, 1.0, ParameterInfo::kCanAutomate,
	                         kParamGain);
	parameters.addParameter (STR16 ("RequestIoChanged"), nullptr, 1, 0.0,
	                         ParameterInfo::kCanAutomate, kParamRequestIoChanged);
	parameters.addParameter (STR16 ("UiState"), nullptr, 0, 0.0, ParameterInfo::kCanAutomate,
	                         kParamUiState);

	// The one parameter with a plain range that is not 0..1. Everything above
	// uses the base `Parameter`, whose `toPlain` is the identity, so a host that
	// never calls `normalizedParamToPlain` looks correct against all of them.
	// See `kParamDelayMs`.
	parameters.addParameter (new RangeParameter (
	    STR16 ("DelayMs"), kParamDelayMs, STR16 ("ms"), kDelayMsMin, kDelayMsMax,
	    kDelayMsMin, 0, ParameterInfo::kCanAutomate));

	return kResultOk;
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeController::setComponentState (IBStream* state)
{
	if (!state)
		return kResultFalse;

	IBStreamer s (state, kLittleEndian);
	int32 mode = 0;
	double ramp = 0.0, gain = 1.0;
	if (!s.readInt32 (mode) || !s.readDouble (ramp) || !s.readDouble (gain))
		return kResultFalse;

	setParamNormalized (kParamMode,
	                    static_cast<double> (mode) / static_cast<double> (kModeStepCount));
	setParamNormalized (kParamRamp, ramp);
	setParamNormalized (kParamGain, gain);
	// Deliberately does NOT touch kParamUiState: the component stream does not
	// carry it, which is the whole point of the separate controller stream.
	return kResultOk;
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeController::setState (IBStream* state)
{
	// The state misbehaviours describe the *plugin*, not one of its halves: a
	// plugin with no state has none on either side. Answering here while the
	// processor declines would make the probe a plugin no host ever meets.
	const int32 misbehaviour = probeMisbehaviour ();
	if (misbehaviour == kMisbehaveStateFails)
		return kResultFalse;
	if (misbehaviour == kMisbehaveStateNotImplemented)
		return kNotImplemented;

	if (!state)
		return kResultFalse;

	IBStreamer s (state, kLittleEndian);
	double ui = 0.0;
	if (!s.readDouble (ui))
		return kResultFalse;

	setParamNormalized (kParamUiState, ui);
	return kResultOk;
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeController::getState (IBStream* state)
{
	const int32 misbehaviour = probeMisbehaviour ();
	if (misbehaviour == kMisbehaveStateFails)
		return kResultFalse;
	if (misbehaviour == kMisbehaveStateNotImplemented)
		return kNotImplemented;

	if (!state)
		return kResultFalse;

	IBStreamer s (state, kLittleEndian);
	s.writeDouble (getParamNormalized (kParamUiState));
	return kResultOk;
}

//------------------------------------------------------------------------
} // namespace Vst
} // namespace Steinberg
