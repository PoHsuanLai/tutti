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

constexpr uint32_t kLevel = 0, kAux = 1, kMode = 2, kParams = 3, kModes = 5;
constexpr uint32_t kLatency = 137, kTail = 8191;
constexpr uint32_t kMagic = 0x31505354; // "TSP1"
constexpr size_t kStateHead = 24;

inline float tag (uint32_t bus, size_t ch) { return float (bus * 1000 + ch + 1); }

struct Ev { int kind; uint32_t offset; uint32_t id; double value; }; // 1 param, 2 on, 3 off

struct Core
{
	std::atomic<double> level {0.5}, aux {0.25};
	std::atomic<uint32_t> mode {0};
	std::atomic<uint64_t> calls {0}, frames {0};
	std::atomic<uint32_t> prepares {0}, maxBlock {0};
	std::atomic<double> rate {0};
	std::array<std::atomic<uint32_t>, 16> blocks {};
	std::atomic<uint32_t> blocksN {0};
	struct Log { std::atomic<uint64_t> head {0}, id {0}; std::atomic<double> value {0}; };
	std::array<Log, 32> log;
	std::atomic<uint32_t> logN {0};
	std::atomic<uint32_t> overflow {0};

	// audio side
	uint32_t dspMode = 0; double dspLevel = 0.5; int held = -1; size_t pos = 0;
	std::vector<std::vector<double>> delay;
	std::vector<Ev> events;

	static bool known (uint32_t id) { return id < kParams; }
	bool param (uint32_t id, double& v) const
	{
		if (id == kLevel) v = level; else if (id == kAux) v = aux;
		else if (id == kMode) v = double (mode) / double (kModes - 1); else return false;
		return true;
	}
	void setParam (uint32_t id, double v)
	{
		v = std::min (1.0, std::max (0.0, v));
		if (id == kLevel) level = v; else if (id == kAux) aux = v;
		else if (id == kMode) mode = std::min<uint32_t> (kModes - 1, uint32_t (std::lround (v * (kModes - 1))));
	}
	uint32_t latency () const { return mode == 1 ? kLatency : 0; }

	void prepare (double r, size_t maxB)
	{
		dspMode = mode; held = -1; pos = 0;
		delay.assign (8, std::vector<double> (kLatency, 0.0));
		events.clear (); events.reserve (128);
		calls = 0; frames = 0; blocksN = 0; logN = 0; overflow = 0; rate = r; maxBlock = uint32_t (maxB); prepares++;
	}
	void reset ()
	{
		held = -1;
		for (auto& l : delay) std::fill (l.begin (), l.end (), 0.0);
	}
	void push (Ev e)
	{
		uint32_t n = logN;
		if (n < 32)
		{
			log[n].head = (uint64_t (e.kind) << 32) | e.offset; log[n].id = e.id; log[n].value = e.value;
			logN = n + 1;
		}
		else
			overflow = 1;
		if (events.size () < 128) events.push_back (e);
		else
			overflow = 1;
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
		put<uint32_t> (b, kMagic); put<uint32_t> (b, mode); put<double> (b, level); put<double> (b, aux);
		put<uint64_t> (b, calls); put<uint64_t> (b, frames); put<uint32_t> (b, prepares);
		put<double> (b, rate); put<uint32_t> (b, maxBlock); put<uint32_t> (b, overflow);
		uint32_t nb = blocksN, kept = std::min<uint32_t> (nb, 16);
		put<uint32_t> (b, kept);
		for (uint32_t k = 0; k < kept; ++k) put<uint32_t> (b, blocks[(nb - kept + k) % 16]);
		uint32_t ne = logN; put<uint32_t> (b, ne);
		for (uint32_t k = 0; k < ne; ++k) { put<uint64_t> (b, log[k].head); put<uint64_t> (b, log[k].id); put<double> (b, log[k].value); }
		return b;
	}
	bool load (const uint8_t* p, size_t n)
	{
		if (n < kStateHead) return false;
		uint32_t magic, m; double l, a;
		std::memcpy (&magic, p, 4); std::memcpy (&m, p + 4, 4); std::memcpy (&l, p + 8, 8); std::memcpy (&a, p + 16, 8);
		if (magic != kMagic || m >= kModes || !std::isfinite (l) || !std::isfinite (a)) return false;
		mode = m; setParam (kLevel, l); setParam (kAux, a);
		return true;
	}
};

} // namespace shared
