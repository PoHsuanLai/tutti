// The VST3 shell on the Steinberg SDK base classes: a separate component and
// controller, buses, parameter queues, event lists, IBStream state.
#include "core.hpp"

#include "base/source/fstreamer.h"
#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
#include "public.sdk/source/main/pluginfactory.h"
#include "public.sdk/source/vst/vstaudioeffect.h"
#include "public.sdk/source/vst/vsteditcontroller.h"

using namespace Steinberg;
using namespace Steinberg::Vst;

static const FUID kProcessorUID (0x54534850, 0x43505001, 0x53484152, 0x45440001);
static const FUID kControllerUID (0x54534850, 0x43505002, 0x53484152, 0x45440001);

static std::vector<uint8_t> readAll (IBStream* s)
{
	std::vector<uint8_t> b; uint8_t chunk[256]; int32 n = 0;
	while (s && s->read (chunk, sizeof chunk, &n) == kResultOk && n > 0) b.insert (b.end (), chunk, chunk + n);
	return b;
}

class SharedProcessor : public AudioEffect
{
public:
	shared::Core core;
	SharedProcessor ()
	{
		setControllerClass (kControllerUID);
		// What the transcript records of a block's transport (core.rs, "State").
		processContextRequirements.needTempo ().needProjectTimeMusic ().needTransportState ();
	}
	static FUnknown* createInstance (void*) { return (IAudioProcessor*)new SharedProcessor (); }

	tresult PLUGIN_API initialize (FUnknown* ctx) SMTG_OVERRIDE
	{
		tresult r = AudioEffect::initialize (ctx);
		if (r != kResultOk) return r;
		addAudioInput (STR16 ("In"), SpeakerArr::kStereo);
		addAudioOutput (STR16 ("Out"), SpeakerArr::kStereo);
		addEventInput (STR16 ("Notes"), 1);
		return kResultOk;
	}
	tresult PLUGIN_API canProcessSampleSize (int32 s) SMTG_OVERRIDE
	{
		return (s == kSample32 || s == kSample64) ? kResultTrue : kResultFalse;
	}
	uint32 PLUGIN_API getLatencySamples () SMTG_OVERRIDE { return core.latency (); }
	uint32 PLUGIN_API getTailSamples () SMTG_OVERRIDE { return shared::kTail; }
	tresult PLUGIN_API setupProcessing (ProcessSetup& s) SMTG_OVERRIDE
	{
		core.prepare (s.sampleRate, size_t (s.maxSamplesPerBlock));
		return AudioEffect::setupProcessing (s);
	}
	tresult PLUGIN_API setProcessing (TBool on) SMTG_OVERRIDE
	{
		if (!on) core.reset ();
		return AudioEffect::setProcessing (on);
	}
	tresult PLUGIN_API setState (IBStream* s) SMTG_OVERRIDE
	{
		auto b = readAll (s);
		return core.load (b.data (), b.size ()) ? kResultOk : kResultFalse;
	}
	tresult PLUGIN_API getState (IBStream* s) SMTG_OVERRIDE
	{
		auto b = core.save (); int32 w = 0;
		return (s && s->write (b.data (), int32 (b.size ()), &w) == kResultOk && w == int32 (b.size ())) ? kResultOk : kResultFalse;
	}

	template <typename S> void run (ProcessData& d, S** (*chan) (AudioBusBuffers&))
	{
		const size_t frames = size_t (d.numSamples);
		auto* ib = &d.inputs[0]; auto* ob = &d.outputs[0];
		S** in = chan (*ib); S** out = chan (*ob);
		std::vector<std::vector<S>> copy (size_t (ib->numChannels));
		for (size_t c = 0; c < copy.size (); ++c) copy[c].assign (in[c], in[c] + frames);
		core.process<S> (frames, 0, copy, out, size_t (ob->numChannels));
	}
	tresult PLUGIN_API process (ProcessData& d) SMTG_OVERRIDE
	{
		if (auto* c = d.processContext)
		{
			uint32_t f = 0;
			if (c->state & ProcessContext::kPlaying) f |= shared::kPlaying;
			if (c->state & ProcessContext::kRecording) f |= shared::kRecording;
			if (c->state & ProcessContext::kCycleActive) f |= shared::kCycle;
			if (c->state & ProcessContext::kTempoValid) f |= shared::kTempoValid;
			if (c->state & ProcessContext::kProjectTimeMusicValid) f |= shared::kBeatValid;
			core.logTransport (0, f, c->tempo, c->projectTimeMusic);
		}
		if (d.inputParameterChanges)
			for (int32 q = 0; q < d.inputParameterChanges->getParameterCount (); ++q)
				if (auto* queue = d.inputParameterChanges->getParameterData (q))
					for (int32 i = 0; i < queue->getPointCount (); ++i)
					{
						int32 off; ParamValue v;
						if (queue->getPoint (i, off, v) == kResultTrue)
							core.push ({1, uint32_t (off < 0 ? 0 : off), queue->getParameterId (), v});
					}
		if (d.inputEvents)
			for (int32 i = 0; i < d.inputEvents->getEventCount (); ++i)
			{
				Event e;
				if (d.inputEvents->getEvent (i, e) != kResultOk) continue;
				uint32_t off = uint32_t (e.sampleOffset < 0 ? 0 : e.sampleOffset);
				if (e.type == Event::kNoteOnEvent) core.push ({2, off, uint32_t (e.noteOn.pitch), 0});
				else if (e.type == Event::kNoteOffEvent) core.push ({3, off, uint32_t (e.noteOff.pitch), 0});
			}
		if (d.numInputs < 1 || d.numOutputs < 1) return kResultOk;
		if (d.symbolicSampleSize == kSample64)
			run<double> (d, [] (AudioBusBuffers& b) { return b.channelBuffers64; });
		else
			run<float> (d, [] (AudioBusBuffers& b) { return b.channelBuffers32; });
		return kResultOk;
	}
};

class SharedController : public EditController
{
public:
	static FUnknown* createInstance (void*) { return (IEditController*)new SharedController (); }
	tresult PLUGIN_API initialize (FUnknown* ctx) SMTG_OVERRIDE
	{
		tresult r = EditController::initialize (ctx);
		if (r != kResultOk) return r;
		parameters.addParameter (STR16 ("level"), nullptr, 0, 0.5, ParameterInfo::kCanAutomate, shared::kLevel);
		parameters.addParameter (STR16 ("aux"), nullptr, 0, 0.25, ParameterInfo::kCanAutomate, shared::kAux);
		parameters.addParameter (STR16 ("mode"), nullptr, shared::kModes - 1, 0.0, ParameterInfo::kCanAutomate, shared::kMode);
		return kResultOk;
	}
	tresult PLUGIN_API setComponentState (IBStream* s) SMTG_OVERRIDE
	{
		auto b = readAll (s);
		if (b.size () < shared::kStateHead) return kResultFalse;
		shared::Core tmp;
		if (!tmp.load (b.data (), b.size ())) return kResultFalse;
		for (uint32_t id = 0; id < shared::kParams; ++id) { double v; tmp.param (id, v); setParamNormalized (id, v); }
		return kResultOk;
	}
};

BEGIN_FACTORY_DEF ("Tutti", "", "")
DEF_CLASS2 (INLINE_UID_FROM_FUID (kProcessorUID), PClassInfo::kManyInstances, kVstAudioEffectClass,
            "Tutti Shared Test Plugin (C++)", Steinberg::Vst::kDistributable, "Fx", "1.0.0", kVstVersionString,
            SharedProcessor::createInstance)
DEF_CLASS2 (INLINE_UID_FROM_FUID (kControllerUID), PClassInfo::kManyInstances, kVstComponentControllerClass,
            "Tutti Shared Test Plugin Controller", 0, "", "1.0.0", kVstVersionString, SharedController::createInstance)
END_FACTORY
