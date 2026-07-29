//-----------------------------------------------------------------------------
// Audio-correctness probe controller. Exists to expose the parameters the
// processor reads; it has no editor (this fixture must run headless).
//-----------------------------------------------------------------------------

#pragma once

#include "probeids.h"
#include "public.sdk/source/vst/vsteditcontroller.h"

namespace Steinberg {
namespace Vst {

//-----------------------------------------------------------------------------
class AudioProbeController : public EditController
{
public:
	tresult PLUGIN_API initialize (FUnknown* context) SMTG_OVERRIDE;
	tresult PLUGIN_API setComponentState (IBStream* state) SMTG_OVERRIDE;

	/// No editor on purpose: the probe is driven entirely from a test.
	IPlugView* PLUGIN_API createView (FIDString /*name*/) SMTG_OVERRIDE { return nullptr; }

	static FUnknown* createInstance (void*)
	{
		return (IEditController*)new AudioProbeController ();
	}
};

//------------------------------------------------------------------------
} // namespace Vst
} // namespace Steinberg
