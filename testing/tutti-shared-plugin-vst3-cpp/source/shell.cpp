// The VST3 shell on the Steinberg SDK base classes: a separate component and
// controller, buses, parameter queues, event lists, IBStream state.
#include "core.hpp"

#include "base/source/fstreamer.h"
#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/base/ustring.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstmidilearn.h"
#include "pluginterfaces/vst/ivstnoteexpression.h"
#include "pluginterfaces/vst/ivstparameterfunctionname.h"
#include "pluginterfaces/vst/ivstphysicalui.h"
#include "pluginterfaces/vst/ivstprefetchablesupport.h"
#include "pluginterfaces/vst/ivstrepresentation.h"
#include "pluginterfaces/vst/ivstunits.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
#include "public.sdk/source/main/pluginfactory.h"
#include "public.sdk/source/vst/vstaudioeffect.h"
#include "public.sdk/source/vst/vsteditcontroller.h"

#include <cstring>
#include <thread>

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

class SharedProcessor : public AudioEffect, public IPrefetchableSupport
{
public:
	OBJ_METHODS (SharedProcessor, AudioEffect)
	DEFINE_INTERFACES
		DEF_INTERFACE (IPrefetchableSupport)
	END_DEFINE_INTERFACES (AudioEffect)
	REFCOUNT_METHODS (AudioEffect)

	// The plugin can be prefetched in its current state.
	tresult PLUGIN_API getPrefetchableSupport (PrefetchableSupport& support) SMTG_OVERRIDE
	{
		support = kIsYetPrefetchable;
		return kResultOk;
	}

	shared::Core core;
	SharedProcessor ()
	{
		setControllerClass (kControllerUID);
		// What the transcript records of a block's transport (core.rs, "State"), and the chord, which
		// the host never sends and no transcript reads.
		processContextRequirements.needTempo ().needProjectTimeMusic ().needTransportState ().needChord ();
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
	uint32 PLUGIN_API getTailSamples () SMTG_OVERRIDE { return core.tailSamples (); }
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
	// A note on of `key` on channel 15 writes the point `aux = key / 127` into the
	// block's outputParameterChanges (core.rs, "VST3 controllers and flushes").
	void reportKey (ProcessData& d, int32 offset, int16 key)
	{
		double v = double (key) / 127.0;
		if (!d.outputParameterChanges) return;
		int32 index = 0, at = 0;
		if (auto* queue = d.outputParameterChanges->addParameterData (shared::kAux, index))
			queue->addPoint (offset, v, at);
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
		// `inputParameterChanges` is not annotated optional (ivstaudioprocessor.h): a null one is recorded.
		if (!d.inputParameterChanges) core.push ({6, 0, 0, 0});
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
				if (e.type == Event::kNoteOnEvent)
				{
					core.push ({2, off, uint32_t (e.noteOn.pitch), 0});
					if (e.noteOn.channel == 15) reportKey (d, int32 (off), e.noteOn.pitch);
				}
				else if (e.type == Event::kDataEvent)
					core.push ({5, off, e.data.size && e.data.bytes ? uint32_t (e.data.bytes[0]) : 0u, double (e.data.size)});
				else if (e.type == Event::kNoteOffEvent) core.push ({3, off, uint32_t (e.noteOff.pitch), 0});
			}
		// A parameter flush (numSamples == 0): the points are applied now.
		if (d.numSamples == 0) { core.flush (); return kResultOk; }
		if (d.numInputs < 1 || d.numOutputs < 1) return kResultOk;
		if (d.symbolicSampleSize == kSample64)
			run<double> (d, [] (AudioBusBuffers& b) { return b.channelBuffers64; });
		else
			run<float> (d, [] (AudioBusBuffers& b) { return b.channelBuffers32; });
		return kResultOk;
	}
};

// The controller's optional interfaces, which only this twin implements: the figures the host's reads are held to.
namespace spec {
// Note expression: three types on bus 0, channel 0.
constexpr NoteExpressionTypeID kCustomNoteExpression = 100;
// Keyswitches: two on bus 0, channel 0, as [min, max] key ranges.
constexpr int32 kKeyswitchRanges[2][2] = {{0, 1}, {2, 3}};
// "Generic 8 Cells", the one representation the plugin answers.
constexpr const char* kXmlName = "Generic 8 Cells";
constexpr const char* kXml =
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n"
    "<vstXML><version>1.0</version><representation name=\"Generic 8 Cells\">"
    "<layout name=\"Cells\" type=\"generic\"><cell><parameter>level</parameter></cell></layout>"
    "</representation></vstXML>";
}

class SharedController : public EditControllerEx1,
                         public IMidiMapping,
                         public INoteExpressionController,
                         public IKeyswitchController,
                         public INoteExpressionPhysicalUIMapping,
                         public IXmlRepresentationController,
                         public IParameterFunctionName,
                         public IMidiLearn
{
public:
	OBJ_METHODS (SharedController, EditControllerEx1)
	DEFINE_INTERFACES
		DEF_INTERFACE (IMidiMapping)
		DEF_INTERFACE (INoteExpressionController)
		DEF_INTERFACE (IKeyswitchController)
		DEF_INTERFACE (INoteExpressionPhysicalUIMapping)
		DEF_INTERFACE (IXmlRepresentationController)
		DEF_INTERFACE (IParameterFunctionName)
		DEF_INTERFACE (IMidiLearn)
	END_DEFINE_INTERFACES (EditControllerEx1)
	REFCOUNT_METHODS (EditControllerEx1)

	// Event bus 0, channel 0: controller 74 drives `level`; nothing else is mapped (core.rs).
	tresult PLUGIN_API getMidiControllerAssignment (int32 bus, int16 channel, CtrlNumber controller, ParamID& id) SMTG_OVERRIDE
	{
		if (bus != 0 || channel != 0 || controller != 74) return kResultFalse;
		id = shared::kLevel;
		return kResultTrue;
	}

	// INoteExpressionController: volume, tuning and a custom type.
	int32 PLUGIN_API getNoteExpressionCount (int32 bus, int16 channel) SMTG_OVERRIDE { return bus == 0 && channel == 0 ? 3 : 0; }
	tresult PLUGIN_API getNoteExpressionInfo (int32 bus, int16 channel, int32 index, NoteExpressionTypeInfo& info) SMTG_OVERRIDE
	{
		if (bus != 0 || channel != 0 || index < 0 || index > 2) return kResultFalse;
		static const NoteExpressionTypeID ids[3] = {kVolumeTypeID, kTuningTypeID, spec::kCustomNoteExpression};
		static const char* titles[3] = {"Volume", "Tuning", "Custom"};
		info = {};
		info.typeId = ids[index];
		UString (info.title, 128).fromAscii (titles[index]);
		UString (info.shortTitle, 128).fromAscii (titles[index]);
		info.unitId = -1;
		info.valueDesc.defaultValue = 0.5;
		info.valueDesc.minimum = 0.0;
		info.valueDesc.maximum = 1.0;
		return kResultTrue;
	}
	tresult PLUGIN_API getNoteExpressionStringByValue (int32, int16, NoteExpressionTypeID, NoteExpressionValue, String128) SMTG_OVERRIDE { return kResultFalse; }
	tresult PLUGIN_API getNoteExpressionValueByString (int32, int16, NoteExpressionTypeID, const TChar*, NoteExpressionValue&) SMTG_OVERRIDE { return kResultFalse; }

	// IKeyswitchController: two note-on keyswitches.
	int32 PLUGIN_API getKeyswitchCount (int32 bus, int16 channel) SMTG_OVERRIDE { return bus == 0 && channel == 0 ? 2 : 0; }
	tresult PLUGIN_API getKeyswitchInfo (int32 bus, int16 channel, int32 index, KeyswitchInfo& info) SMTG_OVERRIDE
	{
		if (bus != 0 || channel != 0 || index < 0 || index > 1) return kResultFalse;
		static const char* titles[2] = {"Legato", "Staccato"};
		info = {};
		info.typeId = kNoteOnKeyswitchTypeID;
		UString (info.title, 128).fromAscii (titles[index]);
		UString (info.shortTitle, 128).fromAscii (titles[index]);
		info.keyswitchMin = spec::kKeyswitchRanges[index][0];
		info.keyswitchMax = spec::kKeyswitchRanges[index][1];
		info.keyRemapped = -1;
		info.unitId = -1;
		return kResultTrue;
	}

	// INoteExpressionPhysicalUIMapping: x movement drives tuning, pressure drives volume, y is unmapped.
	tresult PLUGIN_API getPhysicalUIMapping (int32 bus, int16 channel, PhysicalUIMapList& list) SMTG_OVERRIDE
	{
		if (bus != 0 || channel != 0) return kResultFalse;
		for (uint32 i = 0; i < list.count; ++i)
		{
			PhysicalUIMap& m = list.map[i];
			if (m.physicalUITypeID == kPUIXMovement) m.noteExpressionTypeID = kTuningTypeID;
			else if (m.physicalUITypeID == kPUIPressure) m.noteExpressionTypeID = kVolumeTypeID;
			else m.noteExpressionTypeID = kInvalidTypeID;
		}
		return kResultTrue;
	}

	// IXmlRepresentationController: one document, for one name.
	tresult PLUGIN_API getXmlRepresentationStream (RepresentationInfo& info, IBStream* stream) SMTG_OVERRIDE
	{
		if (!stream || std::strcmp (info.name, spec::kXmlName) != 0) return kResultFalse;
		int32 n = 0;
		const int32 len = int32 (std::strlen (spec::kXml));
		return stream->write (const_cast<char*> (spec::kXml), len, &n) == kResultOk && n == len ? kResultOk : kResultFalse;
	}

	// IParameterFunctionName: the dry/wet mix is `level`, the randomize target is `tone`.
	tresult PLUGIN_API getParameterIDFromFunctionName (UnitID unit, FIDString name, ParamID& id) SMTG_OVERRIDE
	{
		if (unit != kRootUnitId || !name) return kResultFalse;
		if (std::strcmp (name, FunctionNameType::kDryWetMix) == 0) id = shared::kLevel;
		else if (std::strcmp (name, FunctionNameType::kRandomize) == 0) id = shared::kTone;
		else return kResultFalse;
		return kResultTrue;
	}

	// IMidiLearn: a live controller moves `aux` to controller / 127. The call is a main-thread call
	// (ivstmidilearn.h), so one from any other thread aborts, as the editor view's does.
	tresult PLUGIN_API onLiveMIDIControllerInput (int32 bus, int16, CtrlNumber controller) SMTG_OVERRIDE
	{
		if (std::this_thread::get_id () != mainThread) std::abort ();
		if (bus != 0) return kResultFalse;
		setParamNormalized (shared::kAux, double (controller) / 127.0);
		return kResultTrue;
	}
	static FUnknown* createInstance (void*) { return (IEditController*)new SharedController (); }
	tresult PLUGIN_API initialize (FUnknown* ctx) SMTG_OVERRIDE
	{
		tresult r = EditControllerEx1::initialize (ctx);
		if (r != kResultOk) return r;
		mainThread = std::this_thread::get_id ();
		// A unit tree three levels deep under the implicit root; the parameters stay in the root.
		addUnit (new Unit (STR16 ("Voices"), 1, kRootUnitId));
		addUnit (new Unit (STR16 ("Oscillator"), 2, 1));
		addUnit (new Unit (STR16 ("Filter"), 3, 2));
		parameters.addParameter (STR16 ("level"), nullptr, 0, 0.5, ParameterInfo::kCanAutomate, shared::kLevel);
		parameters.addParameter (STR16 ("aux"), nullptr, 0, 0.25, ParameterInfo::kCanAutomate, shared::kAux);
		parameters.addParameter (STR16 ("mode"), nullptr, shared::kModes - 1, 0.0, ParameterInfo::kCanAutomate, shared::kMode);
		parameters.addParameter (STR16 ("tone"), nullptr, 0, 0.5, ParameterInfo::kCanAutomate, shared::kTone);
		return kResultOk;
	}
	// `tone` is the one parameter with a plain domain of its own (core.rs, "Spec").
	ParamValue PLUGIN_API normalizedParamToPlain (ParamID id, ParamValue v) SMTG_OVERRIDE
	{
		return id == shared::kTone ? shared::tonePlain (v) : v;
	}
	ParamValue PLUGIN_API plainParamToNormalized (ParamID id, ParamValue v) SMTG_OVERRIDE
	{
		return id == shared::kTone ? shared::toneNormalized (v) : v;
	}
	tresult PLUGIN_API setComponentState (IBStream* s) SMTG_OVERRIDE
	{
		auto b = readAll (s);
		if (b.size () < shared::kStateHead) return kResultFalse;
		shared::Core tmp;
		if (!tmp.load (b.data (), b.size ())) return kResultFalse;
		for (uint32_t id = 0; id < shared::kTone; ++id) { double v; tmp.param (id, v); setParamNormalized (id, v); }
		// The notice (core.rs): an edit of `aux` to 0.75, or the gesture of the spec (a change to each of
		// kGestureValues), through the host's component handler on every state load.
		if (tmp.notice == shared::kNoticeOnLoad || tmp.notice == shared::kNoticeGestureOnLoad)
		{
			beginEdit (shared::kAux);
			if (tmp.notice == shared::kNoticeOnLoad)
				performEdit (shared::kAux, 0.75);
			else
				for (double v : shared::kGestureValues) performEdit (shared::kAux, v);
			endEdit (shared::kAux);
		}
		return kResultOk;
	}

private:
	std::thread::id mainThread;
};

BEGIN_FACTORY_DEF ("Tutti", "", "")
DEF_CLASS2 (INLINE_UID_FROM_FUID (kProcessorUID), PClassInfo::kManyInstances, kVstAudioEffectClass,
            "Tutti Shared Test Plugin (C++)", Steinberg::Vst::kDistributable, "Fx", "1.0.0", kVstVersionString,
            SharedProcessor::createInstance)
DEF_CLASS2 (INLINE_UID_FROM_FUID (kControllerUID), PClassInfo::kManyInstances, kVstComponentControllerClass,
            "Tutti Shared Test Plugin Controller", 0, "", "1.0.0", kVstVersionString, SharedController::createInstance)
END_FACTORY
