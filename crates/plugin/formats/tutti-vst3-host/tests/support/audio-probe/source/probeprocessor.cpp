//-----------------------------------------------------------------------------
// Audio-correctness probe processor.
//
// Every mode is a closed-form function of the host's input, so the host test
// can compute the expected output exactly and compare without tolerance. The
// host does no arithmetic on samples it forwards — it copies pointers — so any
// difference at all is a real routing or timing bug, not accumulated float
// error. (The one exception is `kModeParamRamp`, where the *host* chooses the
// interpolation; see the note there.)
//-----------------------------------------------------------------------------

#include "probeprocessor.h"

#include "base/source/fstreamer.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"

#include <algorithm>
#include <cstring>

namespace Steinberg {
namespace Vst {

//-----------------------------------------------------------------------------
AudioProbeProcessor::AudioProbeProcessor ()
{
	setControllerClass (AudioProbeControllerUID);
	// Latched once, not re-read per call: a test sets the variable, loads,
	// asserts, then unsets it, and re-reading would make behaviour depend on
	// when each call landed relative to that cleanup.
	mMisbehaviour = probeMisbehaviour ();
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeProcessor::initialize (FUnknown* context)
{
	tresult result = AudioEffect::initialize (context);
	if (result != kResultOk)
		return result;

	// Decline initialisation the way a plugin whose resources are unavailable
	// would. Done *after* the base call so teardown still has a coherent object.
	if (mMisbehaviour == kMisbehaveInitializeFails)
		return kResultFalse;

	// Two input buses of differing width plus two outputs. The asymmetry is
	// deliberate: a host that assumes "one stereo bus in, one stereo bus out"
	// — the shape of every other sample plugin — is exercised here instead of
	// being accidentally correct.
	addAudioInput (STR16 ("Main In"), SpeakerArr::kStereo);
	addAudioInput (STR16 ("Aux In"), SpeakerArr::kMono, kAux, 0);
	addAudioOutput (STR16 ("Main Out"), SpeakerArr::kStereo);
	addAudioOutput (STR16 ("Aux Out"), SpeakerArr::kMono, kAux, 0);

	addEventInput (STR16 ("Event In"), 1);

	return kResultOk;
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeProcessor::setBusArrangements (SpeakerArrangement* inputs,
                                                            int32 numIns,
                                                            SpeakerArrangement* outputs,
                                                            int32 numOuts)
{
	// Accept whatever the host proposes: the probe's assertions are written
	// against the layout the host actually negotiated, read back through
	// PluginInfo, so refusing here would only limit coverage.
	return AudioEffect::setBusArrangements (inputs, numIns, outputs, numOuts);
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeProcessor::canProcessSampleSize (int32 symbolicSampleSize)
{
	// Both precisions, so the f64 path is testable with the same assertions.
	if (symbolicSampleSize == kSample32 || symbolicSampleSize == kSample64)
		return kResultTrue;
	return kResultFalse;
}

//-----------------------------------------------------------------------------
uint32 PLUGIN_API AudioProbeProcessor::getLatencySamples ()
{
	// Claim a latency that is never applied — the stale-or-wrong-units bug.
	if (mMisbehaviour == kMisbehaveLatencyLies)
		return static_cast<uint32> (kLiedLatencySamples);
	return mMode == kModeLatency ? static_cast<uint32> (kReportedLatencySamples) : 0;
}

//-----------------------------------------------------------------------------
int32 PLUGIN_API AudioProbeProcessor::getBusCount (MediaType type, BusDirection dir)
{
	// Overreport *audio* buses only. Inflating the event count too would change
	// which failure the host hits first and muddle what the test observes.
	if (mMisbehaviour == kMisbehaveExtraBuses && type == kAudio)
		return kLyingBusCount;
	return AudioEffect::getBusCount (type, dir);
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeProcessor::setupProcessing (ProcessSetup& setup)
{
	if (mMisbehaviour == kMisbehaveSetupFails)
		return kResultFalse;
	return AudioEffect::setupProcessing (setup);
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeProcessor::setActive (TBool state)
{
	// Refuse activation the way a licence or device check would. Only the
	// *activating* direction fails: a plugin that cannot be deactivated would
	// wedge teardown for reasons unrelated to what this tests.
	if (mMisbehaviour == kMisbehaveSetActiveFails && state)
		return kResultFalse;

	if (state)
	{
		// Deterministic origin for the block counter, and a cleared delay line
		// so a latency test never sees residue from a previous activation.
		mBlockIndex = 0;
		mDelayPos = 0;
		const int32 channels = std::max<int32> (1, processSetup.maxSamplesPerBlock > 0 ? 8 : 8);
		mDelay.assign (static_cast<size_t> (channels),
		               std::vector<double> (static_cast<size_t> (kReportedLatencySamples), 0.0));
	}
	return AudioEffect::setActive (state);
}

//-----------------------------------------------------------------------------
void AudioProbeProcessor::consumeParameterChanges (ProcessData& data)
{
	const int32 frames = data.numSamples > 0 ? data.numSamples : 0;
	mRampAt.assign (static_cast<size_t> (frames), mRamp);

	if (!data.inputParameterChanges)
		return;

	const int32 numQueues = data.inputParameterChanges->getParameterCount ();
	for (int32 q = 0; q < numQueues; ++q)
	{
		IParamValueQueue* queue = data.inputParameterChanges->getParameterData (q);
		if (!queue)
			continue;

		const ParamID id = queue->getParameterId ();
		const int32 numPoints = queue->getPointCount ();
		if (numPoints <= 0)
			continue;

		if (id == kParamRamp)
		{
			// Materialise the automation curve per sample, using the exact
			// interpolation the VST3 docs prescribe: hold the previous value
			// until the first point, then linearly interpolate between
			// consecutive points, then hold the last.
			//
			// Points are consumed in queue order *without sorting* — that is
			// the point. If the host delivers them out of order, the rendered
			// ramp is wrong, and the host test sees it as wrong audio.
			int32 prevOffset = 0;
			double prevValue = mRamp;
			for (int32 p = 0; p < numPoints; ++p)
			{
				int32 offset = 0;
				ParamValue value = 0.0;
				if (queue->getPoint (p, offset, value) != kResultOk)
					continue;
				offset = std::clamp (offset, 0, frames > 0 ? frames - 1 : 0);

				for (int32 i = prevOffset; i < offset && i < frames; ++i)
				{
					const double t = offset > prevOffset
					                     ? static_cast<double> (i - prevOffset) /
					                           static_cast<double> (offset - prevOffset)
					                     : 1.0;
					mRampAt[static_cast<size_t> (i)] = prevValue + (value - prevValue) * t;
				}
				prevOffset = offset;
				prevValue = value;
			}
			for (int32 i = prevOffset; i < frames; ++i)
				mRampAt[static_cast<size_t> (i)] = prevValue;
			mRamp = prevValue;
		}
		else
		{
			// Non-ramp parameters: last point wins, as usual.
			int32 offset = 0;
			ParamValue value = 0.0;
			if (queue->getPoint (numPoints - 1, offset, value) != kResultOk)
				continue;
			if (id == kParamMode)
				mMode =
				    static_cast<int32> (value * static_cast<double> (kModeStepCount) + 0.5);
			else if (id == kParamGain)
				mGain = value;
		}
	}
}

//-----------------------------------------------------------------------------
int32 AudioProbeProcessor::firstNoteOnOffset (ProcessData& data) const
{
	if (!data.inputEvents)
		return -1;
	const int32 count = data.inputEvents->getEventCount ();
	for (int32 i = 0; i < count; ++i)
	{
		Event e {};
		if (data.inputEvents->getEvent (i, e) != kResultOk)
			continue;
		if (e.type == Event::kNoteOnEvent)
			return e.sampleOffset;
	}
	return -1;
}

//-----------------------------------------------------------------------------
bool AudioProbeProcessor::eventInputActive () const
{
	// `eventInputs` is the base class's own list, and `activateBus` is what
	// writes `Bus::active` in it. A plugin declaring no event bus reports
	// false, which keeps the "was it activated" question well-formed rather
	// than vacuously true.
	if (eventInputs.empty ())
		return false;
	for (const auto& bus : eventInputs)
	{
		if (!bus || !bus->isActive ())
			return false;
	}
	return true;
}

//-----------------------------------------------------------------------------
template <typename T>
void AudioProbeProcessor::renderBlock (ProcessData& data)
{
	const int32 frames = data.numSamples;
	const int32 noteOn = firstNoteOnOffset (data);

	// Flat channel index across all output buses — the delay line and any
	// per-channel state are indexed by it.
	int32 flatChannel = 0;

	for (int32 bus = 0; bus < data.numOutputs; ++bus)
	{
		AudioBusBuffers& out = data.outputs[bus];
		for (int32 ch = 0; ch < out.numChannels; ++ch, ++flatChannel)
		{
			T* dst = reinterpret_cast<T**> (
			    data.symbolicSampleSize == kSample32
			        ? reinterpret_cast<void**> (out.channelBuffers32)
			        : reinterpret_cast<void**> (out.channelBuffers64))[ch];
			if (!dst)
				continue;

			// The matching input slot, when the host supplied one. Buses and
			// channels are paired by index: a host that crosses them makes the
			// tag arithmetic below come out wrong.
			const T* src = nullptr;
			if (bus < data.numInputs)
			{
				AudioBusBuffers& in = data.inputs[bus];
				if (ch < in.numChannels)
				{
					T** table = reinterpret_cast<T**> (
					    data.symbolicSampleSize == kSample32
					        ? reinterpret_cast<void**> (in.channelBuffers32)
					        : reinterpret_cast<void**> (in.channelBuffers64));
					if (table)
						src = table[ch];
				}
			}

			switch (mMode)
			{
				case kModeParamRamp:
					for (int32 i = 0; i < frames; ++i)
						dst[i] = static_cast<T> (mRampAt.empty () ? mRamp
						                                         : mRampAt[static_cast<size_t> (i)]);
					break;

				case kModeBlockCounter:
					for (int32 i = 0; i < frames; ++i)
						dst[i] = static_cast<T> (static_cast<double> (mBlockIndex) +
						                         static_cast<double> (i) /
						                             static_cast<double> (frames));
					break;

				case kModeLatency:
				{
					// The delay line is allocated in `setActive(true)`, which
					// `kMisbehaveSetActiveFails` returns from early — and a host
					// that ignores that failure calls `process` anyway. Guard
					// rather than divide by `mDelay.size()` and take SIGFPE:
					// the probe's job is to misbehave *as specified*, and a
					// crash inside the plugin would be read as a host bug.
					if (mDelay.empty ())
					{
						std::fill (dst, dst + frames, static_cast<T> (0));
						break;
					}
					// Each channel walks its own ring from the same starting
					// cursor; the shared cursor is advanced once for the whole
					// block, after every channel is done (see below). Advancing
					// it per channel would desynchronise them.
					auto& ring = mDelay[static_cast<size_t> (
					    flatChannel % static_cast<int32> (mDelay.size ()))];
					int32 pos = mDelayPos;
					for (int32 i = 0; i < frames; ++i)
					{
						const double in = src ? static_cast<double> (src[i]) : 0.0;
						dst[i] = static_cast<T> (ring[static_cast<size_t> (pos)]);
						ring[static_cast<size_t> (pos)] = in;
						pos = (pos + 1) % kReportedLatencySamples;
					}
					break;
				}

				case kModeEventTranscript:
				{
					std::fill (dst, dst + frames, static_cast<T> (0));
					if (!data.inputEvents)
						break;
					const int32 count = data.inputEvents->getEventCount ();
					for (int32 e = 0; e < count; ++e)
					{
						Event ev {};
						if (data.inputEvents->getEvent (e, ev) != kResultOk)
							continue;
						if (ev.sampleOffset < 0 || ev.sampleOffset >= frames)
							continue;
						double code = static_cast<double> (kEventOtherCode);
						if (ev.type == Event::kNoteOnEvent)
							code = static_cast<double> (ev.noteOn.pitch) + 1.0;
						else if (ev.type == Event::kNoteOffEvent)
							code = -(static_cast<double> (ev.noteOff.pitch) + 1.0);
						else if (ev.type == Event::kNoteExpressionValueEvent)
						{
							// Encode the expression's type id and value so the
							// host can check both survived: a host that forwards
							// the event but loses the value would otherwise pass.
							code = kNoteExpressionBaseCode +
							       static_cast<double> (ev.noteExpressionValue.typeId) +
							       ev.noteExpressionValue.value;
						}
						// Accumulate rather than assign: two events sharing an
						// offset must not silently hide one another.
						dst[ev.sampleOffset] =
						    static_cast<T> (static_cast<double> (dst[ev.sampleOffset]) + code);
					}
					break;
				}

				case kModeNoteGate:
					for (int32 i = 0; i < frames; ++i)
						dst[i] = static_cast<T> ((noteOn >= 0 && i >= noteOn) ? 1.0 : 0.0);
					break;

				case kModeEventBusActive:
				{
					// Report our own event-input bus state. `AudioEffect` keeps
					// it in the bus list that `activateBus` writes, so this is
					// the plugin's own view of what the host did to it — not a
					// guess derived from whether events happened to arrive.
					const double code = eventInputActive () ?
					                        static_cast<double> (kEventBusActiveCode) :
					                        static_cast<double> (kEventBusInactiveCode);
					std::fill (dst, dst + frames, static_cast<T> (code));
					break;
				}

				case kModeTagPassthrough:
				default:
					for (int32 i = 0; i < frames; ++i)
					{
						const double in = src ? static_cast<double> (src[i]) : 0.0;
						dst[i] = static_cast<T> (in + probeTag (bus, ch));
					}
					break;
			}
		}
	}

	// Advance the shared delay cursor exactly once per block, after every
	// channel has read from and written to its ring at the same offsets.
	//
	// The previous version advanced it inside the channel loop *and* here,
	// double-counting; and the in-loop guard compared the flat channel index
	// against `numOutputs`, which is a *bus* count, so it fired on the wrong
	// iteration for any multi-channel bus.
	if (mMode == kModeLatency && !mDelay.empty ())
		mDelayPos = (mDelayPos + frames) % kReportedLatencySamples;
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeProcessor::process (ProcessData& data)
{
	// Leave every output buffer exactly as the host handed it over. A host that
	// assumes its scratch was filled forwards whatever was previously in that
	// memory — the stale-buffer leak, which sounds like a burst of older audio.
	// Returning kResultOk is deliberate: the plugin claims success, so only the
	// buffer contents can reveal the problem.
	if (mMisbehaviour == kMisbehaveProcessWritesNothing)
		return kResultOk;

	consumeParameterChanges (data);

	// Report failure on every call. Legal per the spec — plugins with nothing
	// to render do it — so the host must keep running rather than treat the
	// output as meaningful audio.
	if (mMisbehaviour == kMisbehaveProcessFails)
		return kResultFalse;

	// A parameter-flush call (no audio) is legal and must not be treated as a
	// block: counting it would desynchronise `kModeBlockCounter`.
	if (data.numSamples <= 0 || data.numOutputs <= 0 || !data.outputs)
		return kResultOk;

	if (data.symbolicSampleSize == kSample64)
		renderBlock<double> (data);
	else
		renderBlock<float> (data);

	++mBlockIndex;
	return kResultOk;
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeProcessor::setState (IBStream* state)
{
	// Reject every restore. The host must treat a state round-trip as
	// best-effort rather than failing the whole load over it.
	if (mMisbehaviour == kMisbehaveStateFails)
		return kResultFalse;
	// What the SDK's own Component base returns when state is not overridden.
	if (mMisbehaviour == kMisbehaveStateNotImplemented)
		return kNotImplemented;

	if (!state)
		return kResultFalse;
	IBStreamer s (state, kLittleEndian);
	int32 mode = kModeTagPassthrough;
	double ramp = 0.0, gain = 1.0;
	if (!s.readInt32 (mode) || !s.readDouble (ramp) || !s.readDouble (gain))
		return kResultFalse;
	mMode = mode;
	mRamp = ramp;
	mGain = gain;
	return kResultOk;
}

//-----------------------------------------------------------------------------
tresult PLUGIN_API AudioProbeProcessor::getState (IBStream* state)
{
	if (mMisbehaviour == kMisbehaveStateFails)
		return kResultFalse;
	if (mMisbehaviour == kMisbehaveStateNotImplemented)
		return kNotImplemented;

	if (!state)
		return kResultFalse;
	IBStreamer s (state, kLittleEndian);
	s.writeInt32 (mMode);
	s.writeDouble (mRamp);
	s.writeDouble (mGain);
	return kResultOk;
}

//------------------------------------------------------------------------
} // namespace Vst
} // namespace Steinberg
