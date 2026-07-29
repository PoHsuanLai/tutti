//! C ABI over Steinberg's `HostCheck`, so a Rust test can validate the
//! `ProcessData` our host actually built.
//!
//! The Rust side passes the *live* `ProcessData*` captured by the
//! `conformance` observer seam — this shim never reconstructs one. That is
//! the whole point: a reconstructed struct would test the reconstruction.
//!
//! `HostCheck` is a singleton (`HostCheck::Instance()`), so the test must
//! serialize calls; it does, via a mutex.

#include "hostcheck.h"
#include "logevents.h"

#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/base/funknown.h"

#include <cstring>
#include <vector>

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace {

/// Minimal `IComponent` reporting a caller-supplied bus layout.
///
/// `HostCheck::checkAudioBuffers` calls `mComponent->getBusInfo` *outside* its
/// own `if (mComponent)` guard, so a null component segfaults it. We always
/// install one. Its bus counts/channel counts are set from what the host under
/// test actually negotiated, so a mismatch between the host's `ProcessData`
/// and the plugin's real layout is reported rather than masked.
class StubComponent : public IComponent
{
public:
	int32 eventIn = 1, eventOut = 0;
	int32 eventChannels = 16;
	/// Per-bus channel counts, in bus-index order — mirroring what the plugin
	/// under test actually reported. A single count for all buses would be a
	/// lie for any multi-bus plugin (HostChecker itself declares 11 input
	/// buses of mixed width) and would manufacture channel-count "violations"
	/// that are the stub's fault, not the host's.
	std::vector<int32> chIn {2};
	std::vector<int32> chOut {2};

	tresult PLUGIN_API queryInterface (const TUID, void** obj) SMTG_OVERRIDE
	{ *obj = this; return kResultOk; }
	uint32 PLUGIN_API addRef () SMTG_OVERRIDE { return 1; }
	uint32 PLUGIN_API release () SMTG_OVERRIDE { return 1; }

	tresult PLUGIN_API initialize (FUnknown*) SMTG_OVERRIDE { return kResultOk; }
	tresult PLUGIN_API terminate () SMTG_OVERRIDE { return kResultOk; }

	tresult PLUGIN_API getControllerClassId (TUID) SMTG_OVERRIDE { return kNotImplemented; }
	tresult PLUGIN_API setIoMode (IoMode) SMTG_OVERRIDE { return kResultOk; }

	int32 PLUGIN_API getBusCount (MediaType type, BusDirection dir) SMTG_OVERRIDE
	{
		if (type == kAudio)
			return static_cast<int32> ((dir == kInput ? chIn : chOut).size ());
		return dir == kInput ? eventIn : eventOut;
	}

	tresult PLUGIN_API getBusInfo (MediaType type, BusDirection dir, int32 index,
	                               BusInfo& info) SMTG_OVERRIDE
	{
		if (index < 0 || index >= getBusCount (type, dir))
			return kInvalidArgument;
		memset (&info, 0, sizeof (info));
		info.mediaType = type;
		info.direction = dir;
		// Bus 0 is the main bus; anything beyond it is aux/sidechain. The
		// distinction matters: HostCheck reports a null aux channel buffer
		// under a different log id than a null main one.
		info.busType = (index == 0) ? kMain : kAux;
		info.flags = BusInfo::kDefaultActive;
		info.channelCount = (type == kAudio)
		                        ? (dir == kInput ? chIn : chOut)[index]
		                        : eventChannels;
		return kResultOk;
	}

	tresult PLUGIN_API getRoutingInfo (RoutingInfo&, RoutingInfo&) SMTG_OVERRIDE
	{ return kNotImplemented; }
	tresult PLUGIN_API activateBus (MediaType, BusDirection, int32, TBool) SMTG_OVERRIDE
	{ return kResultOk; }
	tresult PLUGIN_API setActive (TBool) SMTG_OVERRIDE { return kResultOk; }
	tresult PLUGIN_API setState (IBStream*) SMTG_OVERRIDE { return kResultOk; }
	tresult PLUGIN_API getState (IBStream*) SMTG_OVERRIDE { return kResultOk; }
};

StubComponent gComponent;

} // namespace

extern "C" {

int hc_num_log_events () { return kNumLogEvents; }

const char* hc_log_description (int id)
{ return (id >= 0 && id < kNumLogEvents) ? logEventDescriptions[id] : nullptr; }

const char* hc_log_severity (int id)
{ return (id >= 0 && id < kNumLogEvents) ? logEventSeverity[id] : nullptr; }

/// Tell the checker what the host negotiated at activation, and what bus
/// layout the plugin reported. Call before `hc_validate`.
///
/// `in_channels` / `out_channels` are per-bus channel counts in bus-index
/// order (length `n_in` / `n_out`), copied verbatim from the plugin's own
/// reported layout.
void hc_configure (double sample_rate, int max_block, int sample_size, int process_mode,
                   const int* in_channels, int n_in, const int* out_channels, int n_out,
                   int event_in, int event_out)
{
	gComponent.chIn.assign (in_channels, in_channels + (n_in > 0 ? n_in : 0));
	gComponent.chOut.assign (out_channels, out_channels + (n_out > 0 ? n_out : 0));
	gComponent.eventIn = event_in;
	gComponent.eventOut = event_out;

	ProcessSetup setup {};
	setup.processMode = process_mode;
	setup.symbolicSampleSize = sample_size;
	setup.maxSamplesPerBlock = max_block;
	setup.sampleRate = sample_rate;

	HostCheck& hc = HostCheck::Instance ();
	hc.setProcessSetup (setup);
	hc.setComponent (&gComponent);
	hc.getEventLogger ().resetLogEvents ();
}

/// Register a parameter id as valid, so param-change queues carrying unknown
/// ids are reported. Call once per parameter the plugin exposes.
void hc_add_parameter (unsigned int param_id)
{
	HostCheck::Instance ().addParameter (param_id);
}

/// Validate the *live* ProcessData our host built.
///
/// `data` is the pointer captured by the conformance observer, reinterpreted
/// here — no copy, no reconstruction. `counts` receives `hc_num_log_events()`
/// entries. Returns 1 when the block is clean, 0 when any check fired.
int hc_validate (const void* data, int min_in_buffers, int min_out_buffers, long long* counts)
{
	if (!data)
		return -1;

	HostCheck& hc = HostCheck::Instance ();
	hc.getEventLogger ().resetLogEvents ();

	// `validate` takes a non-const ref but does not mutate the struct.
	ProcessData& pd = *const_cast<ProcessData*> (static_cast<const ProcessData*> (data));
	bool clean = hc.validate (pd, min_in_buffers, min_out_buffers);

	const auto& logs = hc.getEventLogs ();
	for (size_t i = 0; i < logs.size (); ++i)
		counts[i] = logs[i].count;

	return clean ? 1 : 0;
}

} // extern "C"
