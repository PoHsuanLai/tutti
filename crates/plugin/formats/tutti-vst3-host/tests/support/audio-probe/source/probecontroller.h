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

	/// The controller's **own** state stream, distinct from the component's.
	/// Carries `kParamUiState` and nothing else — the processor neither writes
	/// nor reads it, so a host that persists only the component stream restores
	/// this parameter to its default however it was saved.
	tresult PLUGIN_API setState (IBStream* state) SMTG_OVERRIDE;
	tresult PLUGIN_API getState (IBStream* state) SMTG_OVERRIDE;

	/// Refuses under `kMisbehaveControllerConnectFails`, otherwise defers to
	/// the base. The host wires the two halves with two `connect` calls, and
	/// this is the second one — refusing here leaves the component half
	/// already connected, which is the asymmetry the host must unwind.
	tresult PLUGIN_API connect (IConnectionPoint* other) SMTG_OVERRIDE;

	/// Writing `kParamRequestIoChanged` asks the host for
	/// `restartComponent(kIoChanged)`, so a test can drive the restart path
	/// without needing a plugin that reconfigures itself spontaneously.
	tresult PLUGIN_API setParamNormalized (ParamID id, ParamValue value) SMTG_OVERRIDE;

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
