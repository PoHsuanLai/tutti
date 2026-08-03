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
	return kResultOk;
}

//------------------------------------------------------------------------
} // namespace Vst
} // namespace Steinberg
