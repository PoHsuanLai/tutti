// The spec of tutti-shared-plugin's core, in C++. No SDK types in this file.
#pragma once
#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <vector>

namespace shared {

constexpr uint32_t kLevel = 0, kAux = 1, kMode = 2, kTone = 3, kParams = 4, kModes = 5;
// `tone`'s plain domain (core.rs, "Spec"): a decreasing exponential, 20000 * 1000^(-n).
constexpr double kTonePlainMax = 20000.0, kTonePlainMin = 20.0;
inline double tonePlain (double n)
{
	return kTonePlainMax * std::pow (kTonePlainMin / kTonePlainMax, std::min (1.0, std::max (0.0, n)));
}
inline double toneNormalized (double plain)
{
	double p = std::min (kTonePlainMax, std::max (kTonePlainMin, plain));
	return std::min (1.0, std::max (0.0, std::log (kTonePlainMax / p) / std::log (kTonePlainMax / kTonePlainMin)));
}
constexpr uint32_t kLatency = 137, kTail = 8191;
constexpr uint32_t kMagic = 0x31505354; // "TSP1"
constexpr size_t kStateHead = 24;
// A transport's flags (core.rs, "State"), and how many the log keeps.
constexpr uint32_t kPlaying = 1, kRecording = 2, kCycle = 4, kTempoValid = 8, kBeatValid = 16;
constexpr uint32_t kLogTransports = 16;
// The notice byte (core.rs, "Notice"): none, or an edit on every state load.
constexpr uint32_t kNoticeOnLoad = 1, kNotices = 2;

inline float tag (uint32_t bus, size_t ch) { return float (bus * 1000 + ch + 1); }

// 1 param, 2 on, 3 off, 4 control change (recorded, never acted on; this plugin's VST3 shell receives none),
// 5 data event (id: first byte, value: size), 6 null parameter list, 7 flush (core.rs, "VST3 controllers and flushes")
struct Ev { int kind; uint32_t offset; uint32_t id; double value; };

struct Core
{
	std::atomic<double> level {0.5}, aux {0.25}, tone {0.5};
	std::atomic<uint32_t> mode {0}, notice {0};
	std::atomic<uint64_t> calls {0}, frames {0};
	std::atomic<uint32_t> prepares {0}, maxBlock {0};
	std::atomic<double> rate {0};
	std::array<std::atomic<uint32_t>, 16> blocks {};
	std::atomic<uint32_t> blocksN {0};
	struct Log { std::atomic<uint64_t> head {0}, id {0}; std::atomic<double> value {0}; };
	std::array<Log, 32> log;
	std::atomic<uint32_t> logN {0};
	std::atomic<uint32_t> overflow {0};
	struct Told { std::atomic<uint64_t> head {0}; std::atomic<uint32_t> flags {0}; std::atomic<double> tempo {0}, beat {0}; };
	std::array<Told, kLogTransports> told;
	std::atomic<uint32_t> toldN {0};

	// audio side
	uint32_t dspMode = 0; double dspLevel = 0.5; int held = -1; size_t pos = 0;
	std::vector<std::vector<double>> delay;
	std::vector<Ev> events;

	static bool known (uint32_t id) { return id < kParams; }
	bool param (uint32_t id, double& v) const
	{
		if (id == kLevel) v = level; else if (id == kAux) v = aux; else if (id == kTone) v = tone;
		else if (id == kMode) v = double (mode) / double (kModes - 1); else return false;
		return true;
	}
	void setParam (uint32_t id, double v)
	{
		v = std::min (1.0, std::max (0.0, v));
		if (id == kLevel) level = v; else if (id == kAux) aux = v; else if (id == kTone) tone = v;
		else if (id == kMode) mode = std::min<uint32_t> (kModes - 1, uint32_t (std::lround (v * (kModes - 1))));
	}
	uint32_t latency () const { return mode == 1 ? kLatency : 0; }

	void prepare (double r, size_t maxB)
	{
		dspMode = mode; held = -1; pos = 0;
		delay.assign (8, std::vector<double> (kLatency, 0.0));
		events.clear (); events.reserve (128);
		calls = 0; frames = 0; blocksN = 0; logN = 0; toldN = 0; overflow = 0; rate = r; maxBlock = uint32_t (maxB); prepares++;
	}
	void reset ()
	{
		held = -1;
		for (auto& l : delay) std::fill (l.begin (), l.end (), 0.0);
	}
	void push (Ev e)
	{
		record (e);
		if (events.size () < 128) events.push_back (e);
		else
			overflow = 1;
	}
	// A VST3 parameter flush (a process call with numSamples == 0): records it,
	// applies the parameter points queued since the last block at once and
	// forgets the points. No block is counted and nothing is rendered.
	void flush ()
	{
		record ({7, 0, 0, 0});
		events.erase (std::remove_if (events.begin (), events.end (), [this] (const Ev& e) {
			if (e.kind != 1) return false;
			setParam (e.id, e.value);
			return true;
		}), events.end ());
	}
	void record (Ev e)
	{
		uint32_t n = logN;
		if (n < 32)
		{
			log[n].head = (uint64_t (e.kind) << 32) | e.offset; log[n].id = e.id; log[n].value = e.value;
			logN = n + 1;
		}
		else
			overflow = 1;
	}

	// A transport the host sent with the block about to be processed, as sent.
	void logTransport (uint32_t offset, uint32_t flags, double tempo, double beat)
	{
		uint32_t n = toldN;
		if (n < kLogTransports)
		{
			told[n].head = (std::min<uint64_t> (calls, UINT32_MAX) << 32) | offset;
			told[n].flags = flags; told[n].tempo = tempo; told[n].beat = beat;
		}
		if (n < UINT32_MAX) toldN = n + 1;
	}

	template <typename S>
	void render (size_t from, size_t to, uint32_t bus, const std::vector<std::vector<S>>& in,
	             S* const* out, size_t nOut)
	{
		for (size_t c = 0; c < nOut; ++c)
			for (size_t i = from; i < to; ++i)
			{
				double x = c < in.size () ? double (in[c][i]) : 0.0;
				if (dspMode == 0) out[c][i] = S (x + tag (bus, c));
				else if (dspMode == 1)
				{
					auto& l = delay[std::min<size_t> (c, 7)];
					size_t k = (pos + i) % l.size ();
					double old = l[k]; l[k] = x; out[c][i] = S (old + tag (bus, c));
				}
				else if (dspMode == 2) out[c][i] = S (dspLevel);
				else if (dspMode == 3) out[c][i] = S (held < 0 ? 0.0 : double (held) + 1000.0 * double (c));
			}
	}

	// `in` is a copy of the input (never aliases `out`).
	template <typename S>
	void process (size_t nFrames, uint32_t bus, const std::vector<std::vector<S>>& in, S* const* out, size_t nOut)
	{
		uint64_t n = calls++; frames += nFrames;
		if (n >= 16) overflow = 1;
		blocks[n % 16] = uint32_t (nFrames); blocksN = uint32_t (std::min<uint64_t> (n + 1, UINT32_MAX));
		dspLevel = level;
		std::stable_sort (events.begin (), events.end (), [] (const Ev& a, const Ev& b) { return a.offset < b.offset; });
		size_t at = 0;
		for (auto& e : events)
		{
			size_t o = std::min<size_t> (e.offset, nFrames);
			render (at, o, bus, in, out, nOut); at = o;
			if (e.kind == 1) { setParam (e.id, e.value); if (e.id == kLevel) dspLevel = std::min (1.0, std::max (0.0, e.value)); }
			else if (e.kind == 2) held = int (e.id);
			else if (e.kind == 3 && held == int (e.id)) held = -1;
		}
		render (at, nFrames, bus, in, out, nOut);
		pos = (pos + nFrames) % kLatency;
		events.clear ();
	}

	template <typename T> static void put (std::vector<uint8_t>& b, T v)
	{
		auto p = reinterpret_cast<const uint8_t*> (&v); b.insert (b.end (), p, p + sizeof (T));
	}
	std::vector<uint8_t> save () const
	{
		std::vector<uint8_t> b;
		put<uint32_t> (b, kMagic); put<uint32_t> (b, mode | notice << 16); put<double> (b, level); put<double> (b, aux);
		put<uint64_t> (b, calls); put<uint64_t> (b, frames); put<uint32_t> (b, prepares);
		put<double> (b, rate); put<uint32_t> (b, maxBlock); put<uint32_t> (b, overflow);
		uint32_t nb = blocksN, kept = std::min<uint32_t> (nb, 16);
		put<uint32_t> (b, kept);
		for (uint32_t k = 0; k < kept; ++k) put<uint32_t> (b, blocks[(nb - kept + k) % 16]);
		uint32_t ne = logN; put<uint32_t> (b, ne);
		for (uint32_t k = 0; k < ne; ++k) { put<uint64_t> (b, log[k].head); put<uint64_t> (b, log[k].id); put<double> (b, log[k].value); }
		uint32_t nt = toldN; put<uint32_t> (b, nt);
		for (uint32_t k = 0; k < std::min (nt, kLogTransports); ++k)
		{
			uint64_t h = told[k].head;
			put<uint32_t> (b, uint32_t (h >> 32)); put<uint32_t> (b, uint32_t (h)); put<uint32_t> (b, told[k].flags);
			put<uint32_t> (b, 0); put<double> (b, told[k].tempo); put<double> (b, told[k].beat);
		}
		return b;
	}
	bool load (const uint8_t* p, size_t n)
	{
		if (n < kStateHead) return false;
		uint32_t magic, word, m, editor, nt; double l, a;
		std::memcpy (&magic, p, 4); std::memcpy (&word, p + 4, 4); std::memcpy (&l, p + 8, 8); std::memcpy (&a, p + 16, 8);
		m = word & 0xFF; editor = (word >> 8) & 0xFF; nt = word >> 16;
		// This plugin has no editor: it refuses the editor byte, and takes the notice byte.
		if (magic != kMagic || m >= kModes || editor != 0 || nt >= kNotices || !std::isfinite (l) || !std::isfinite (a)) return false;
		mode = m; notice = nt; setParam (kLevel, l); setParam (kAux, a);
		return true;
	}
};

} // namespace shared
