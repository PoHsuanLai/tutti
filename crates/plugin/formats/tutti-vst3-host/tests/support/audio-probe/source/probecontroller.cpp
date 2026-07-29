//-----------------------------------------------------------------------------
// Audio-correctness probe controller.
//-----------------------------------------------------------------------------

#include "probecontroller.h"

#include "base/source/fstreamer.h"
#include "pluginterfaces/base/ibstream.h"

namespace Steinberg {
namespace Vst {

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeController::initialize (FUnknown* context)
{
	tresult result = EditController::initialize (context);
	if (result != kResultOk)
		return result;

	// Ids are deliberately nonzero and non-contiguous — see `ProbeParams`.
	// `kParamMode` is stepped so a host that rounds normalized values
	// differently still lands on an exact mode.
	parameters.addParameter (STR16 ("Mode"), nullptr, 5 /*stepCount: modes 0..5*/, 0.0,
	                         ParameterInfo::kCanAutomate, kParamMode);
	parameters.addParameter (STR16 ("Ramp"), nullptr, 0, 0.0, ParameterInfo::kCanAutomate,
	                         kParamRamp);
	parameters.addParameter (STR16 ("Gain"), nullptr, 0, 1.0, ParameterInfo::kCanAutomate,
	                         kParamGain);

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

	setParamNormalized (kParamMode, static_cast<double> (mode) / 5.0);
	setParamNormalized (kParamRamp, ramp);
	setParamNormalized (kParamGain, gain);
	return kResultOk;
}

//------------------------------------------------------------------------
} // namespace Vst
} // namespace Steinberg
