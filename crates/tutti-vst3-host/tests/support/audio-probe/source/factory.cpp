//-----------------------------------------------------------------------------
// Factory for the audio-correctness probe.
//-----------------------------------------------------------------------------

#include "probecontroller.h"
#include "probeids.h"
#include "probeprocessor.h"
#include "version.h"

#include "public.sdk/source/main/pluginfactory.h"

#define stringPluginName "AudioProbe"

BEGIN_FACTORY_DEF (stringCompanyName, stringCompanyWeb, stringCompanyEmail)

DEF_CLASS2 (INLINE_UID_FROM_FUID (Steinberg::Vst::AudioProbeProcessorUID),
            PClassInfo::kManyInstances,
            kVstAudioEffectClass,
            stringPluginName,
            Steinberg::Vst::kDistributable,
            "Fx",
            FULL_VERSION_STR,
            kVstVersionString,
            Steinberg::Vst::AudioProbeProcessor::createInstance)

DEF_CLASS2 (INLINE_UID_FROM_FUID (Steinberg::Vst::AudioProbeControllerUID),
            PClassInfo::kManyInstances,
            kVstComponentControllerClass,
            stringPluginName "Controller",
            0,
            "",
            FULL_VERSION_STR,
            kVstVersionString,
            Steinberg::Vst::AudioProbeController::createInstance)

END_FACTORY
