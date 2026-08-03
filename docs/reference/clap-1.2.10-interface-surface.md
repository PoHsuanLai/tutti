# CLAP 1.2.10 — Complete Interface Surface (Host Perspective)

Extracted verbatim from the official `free-audio/clap` C headers at
`~/Documents/dawAI/dawai-refs/clap/include/clap/`.

**Version, from `version.h`:** `CLAP_VERSION_MAJOR 1`, `CLAP_VERSION_MINOR 2`,
`CLAP_VERSION_REVISION 10` → **1.2.10**.
`clap_version_is_compatible(v)` returns `v.major >= 1` — "versions 0.x.y were used during
development stage and aren't compatible".

**Scope:** 27 stable extensions (`ext/*.h`) + 18 draft extensions (`ext/draft/*.h`) = 45 headers,
carrying **51 distinct extension ID strings** (`undo.h` defines 3; 8 headers add a `_COMPAT`
alias). Plus 4 factories, the entry point, `clap_plugin`, `clap_host`, all 13 event types,
`clap_process`, `clap_audio_buffer`, and the streams.

## How to read the thread column

The `[main-thread]` / `[audio-thread]` / `[thread-safe]` annotations in the doc comments **are** the
threading contract. Every annotation in this document is **transcribed verbatim** from the header.
Where a header states no annotation for a function pointer, the column reads **`unannotated`** — that
is a fact about the header, not an inference, and those cases are called out explicitly because they
are the ones where a host must not assume.

The complete annotation vocabulary present in the tree, with occurrence counts (`grep`-verified):

| Annotation (verbatim) | Count |
|---|---|
| `[main-thread]` | 123 |
| `[thread-safe]` | 25 |
| `[main-thread & !active]` | 6 |
| `[background-thread]` | 6 |
| `[main-thread & !floating]` | 5 |
| `[!active]` (on rescan flags, not functions) | 5 |
| `[audio-thread]` | 4 |
| `[active ? audio-thread : main-thread]` | 3 |
| `[thread-safe,!audio-thread]` | 2 |
| `[thread-safe & !floating]` | 2 |
| `[main-thread & plugin-subscribed-to-undo-context]` | 2 |
| `[main-thread & floating]` | 2 |
| `[main-thread & being-activated]` | 2 |
| `[main-thread & active]` | 2 |
| `[audio-thread & in-process]` | 2 |
| `[audio-thread & active & processing]` | 2 |
| `[main-thread,audio-thread]` | 1 |
| `[main-thread & plugin-deactivated]` | 1 |
| `[audio-thread & active]` | 1 |
| `[audio-thread & active & !processing]` | 1 |

**Direction convention used throughout:**
- **host → plugin** = a `clap_plugin_*` struct. The host calls these. We must obey the annotation.
- **plugin → host** = a `clap_host_*` struct. **We must implement these**, and must be callable on
  the annotated thread.

---

# Part 0 — The threading model (`ext/thread-check.h`)

This is normative for everything below. Quoted verbatim.

**main-thread**
> "This is the thread in which most of the interaction between the plugin and host happens."
> "This will be the same OS thread throughout the lifetime of the plug-in."
> "On macOS and Windows, this must be the thread on which gui and timer events are received (i.e., the main thread of the program)."
> "It isn't a realtime thread, yet this thread needs to respond fast enough to allow responsive user interaction…"

**audio-thread**
> "This thread can be used for realtime audio processing. Its execution should be as deterministic as possible to meet the audio interface's deadline (can be <1ms)."
> "There are a known set of operations that should be avoided: malloc() and free(), contended locks and mutexes, I/O, waiting, and so forth."
> "The audio-thread is symbolic, there isn't one OS thread that remains the audio-thread for the plugin lifetime."
> "However, the host must guarantee that single plugin instance will not be two audio-threads at the same time." *(sic)*
> "Functions marked with [audio-thread] **ARE NOT CONCURRENT**."
> "The host may mark any OS thread, including the main-thread as the audio-thread, as long as it can guarantee that only one OS thread is the audio-thread at a time in a plugin instance."
> "The audio-thread can be seen as a concurrency guard for all functions marked with [audio-thread]."

**Render interaction — a direct obligation on us as host:**
> "If a plugin doesn't implement render, then that plugin must have all [audio-thread] functions meet the real time standard."
> "Hosts also provide functions marked [audio-thread]. These can be safely called by a plugin in the audio thread."
> "Therefore hosts must either (1) implement those functions meeting the real-time constraints or (2) not process plugins which advertise a hard realtime constraint or don't implement the render extension."
> "Hosts which provide [audio-thread] functions outside these conditions may experience inconsistent or inaccurate rendering."

**background-thread**
> "This thread is created by the host to run some heavy operation without blocking the main-thread."
> "It is the host's responsiblity to execute background operations in a way that guarentee a race-free and deterministic execution." *(sic)*

**thread-safe**
> "Functions tagged as [thread-safe] can be called from any thread unless explicitly counter-indicated (for instance [thread-safe, !audio-thread]) and may be called concurrently."

> "It is highly recommended that hosts implement this extension."

---

# Part B — Core lifecycle, factories, events

## B.1 `clap_plugin_entry` (`entry.h`)

The DSO's single exported symbol: `CLAP_EXPORT extern const clap_plugin_entry_t clap_entry;`

Field `clap_version` — "initialized to CLAP_VERSION".

| Field | Signature | Thread annotation | Notes |
|---|---|---|---|
| `init` | `bool(CLAP_ABI *init)(const char *plugin_path)` | **unannotated** (prose only, see below) | Must be first call into DSO |
| `deinit` | `void(CLAP_ABI *deinit)(void)` | **unannotated** (prose only) | Pairs with `init` |
| `get_factory` | `const void *(CLAP_ABI *get_factory)(const char *factory_id)` | `[thread-safe]` | Returns null if not provided |

`init`/`deinit` carry no bracketed tag; their threading rules are prose:
> "This function may be called on any thread, including a different one from the one a later call to deinit() (or a later init()) can be made. However, it is forbidden to call this function simultaneously from multiple threads. It is also forbidden to call it simultaneously with *any* other CLAP-related symbols from the DSO, including (but not limited to) deinit()."
> "Unlike init() and deinit(), this function can be called simultaneously by multiple threads." (on `get_factory`)

**Host obligations, verbatim:**
> "This function must be called first, before any-other CLAP-related function or symbol from this DSO."
> "It also must only be called once, until a later call to deinit() is made, after which init() can be called once more to re-initialize the DSO."
> "Returns true on success. If init() returns false, then the DSO must be considered uninitialized, and the host must not call deinit() nor any other CLAP-related symbols from the DSO."
> "After this function is called, no more calls into the DSO must be made, except calling init() again to re-initialize the DSO."
> "It is forbidden to display graphical user interfaces in this call." / "It is forbidden to perform any user interaction in this call."
> "It should be as fast as possible, in order to perform a very quick scan of the plugin descriptors."
> "The returned pointer must *not* be freed by the caller." (on `get_factory`)
> "plugin_path is the path to the DSO (Linux, Windows), or the bundle (macOS)."

**The 1.2.0 reentrancy rationale (matters for us — we host wrappers):**
> "We realized, though, that this is not a requirement hosts can meet. If hosts load a plugin which itself wraps another CLAP for instance, while also loading that same clap in its memory space, both the host and the wrapper will call init() and deinit() and have no means to communicate the state."
> "With CLAP 1.2.0 and beyond we are changing the spec to indicate that a host should make an absolute best effort to call init() and deinit() once, and always in matched pairs (for every init() which returns true, one deinit() should be called)."
> "…the plugin author must maintain a counter and must manage a mutex lock."

**Standard search path (verbatim), plus the `CLAP_PATH` requirement:**
- Linux: `~/.clap`, `/usr/lib/clap`
- Windows: `%COMMONPROGRAMFILES%\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`
- macOS: `/Library/Audio/Plug-Ins/CLAP`, `~/Library/Audio/Plug-Ins/CLAP`

> "In addition to the OS-specific default locations above, a CLAP host must query the environment for a CLAP_PATH variable, which is a list of directories formatted in the same manner as the host OS binary search path (PATH on Unix, separated by `:` and Path on Windows, separated by ';', as of this writing)."
> "Each directory should be recursively searched for files and/or bundles as appropriate in your OS ending with the extension `.clap`."

## B.2 `clap_plugin_factory` (`factory/plugin-factory.h`) — STABLE

`CLAP_PLUGIN_FACTORY_ID = "clap.plugin-factory"`. No compat alias.
Implemented by the plugin DSO; **the host calls it.**

| Field | Signature | Thread annotation |
|---|---|---|
| `get_plugin_count` | `uint32_t(CLAP_ABI *)(const struct clap_plugin_factory *factory)` | `[thread-safe]` |
| `get_plugin_descriptor` | `const clap_plugin_descriptor_t *(CLAP_ABI *)(const struct clap_plugin_factory *factory, uint32_t index)` | `[thread-safe]` |
| `create_plugin` | `const clap_plugin_t *(CLAP_ABI *)(const struct clap_plugin_factory *factory, const clap_host_t *host, const char *plugin_id)` | `[thread-safe]` |

> "Every method must be thread-safe."
> "It is very important to be able to scan the plugin as quickly as possible."
> "The descriptor is owned by the plugin and is valid until the call to clap_plugin_entry->deinit()"
> "The clap_host pointer must be valid until after the call to plugin->destroy(plugin)."
> "The returned pointer is owned by the plugin and must be freed by calling plugin->destroy(plugin);"
> **"The plugin is not allowed to use the host callbacks in the create method."**
> "Returns null in case of error."

### `clap_plugin_descriptor` (`plugin.h`)

| Field | Type | Note |
|---|---|---|
| `clap_version` | `clap_version_t` | "initialized to CLAP_VERSION" |
| `id` | `const char *` | `eg: "com.u-he.diva", mandatory` |
| `name` | `const char *` | `eg: "Diva", mandatory` |
| `vendor` | `const char *` | `eg: "u-he"` |
| `url` | `const char *` | |
| `manual_url` | `const char *` | |
| `support_url` | `const char *` | |
| `version` | `const char *` | `eg: "1.4.4"` |
| `description` | `const char *` | |
| `features` | `const char *const *` | null-terminated array |

> "Mandatory fields must be set and must not be blank."
> "The array of pointers must be null terminated."
> "- version is an arbitrary string … here is a regex like expression which is likely to be understood by most hosts: MAJOR(.MINOR(.REVISION)?)?( (Alpha|Beta) XREV)?"

## B.3 `clap_plugin` (`plugin.h`) — host → plugin

| Field | Signature | Thread annotation | Notes |
|---|---|---|---|
| `desc` | `const clap_plugin_descriptor_t *` | data | |
| `plugin_data` | `void *` | data | "reserved pointer for the plugin" |
| `init` | `bool(CLAP_ABI *init)(const struct clap_plugin *plugin)` | `[main-thread]` | Must be called after creating |
| `destroy` | `void(CLAP_ABI *destroy)(const struct clap_plugin *plugin)` | `[main-thread & !active]` | |
| `activate` | `bool(CLAP_ABI *activate)(const struct clap_plugin *plugin, double sample_rate, uint32_t min_frames_count, uint32_t max_frames_count)` | `[main-thread & !active]` | |
| `deactivate` | `void(CLAP_ABI *deactivate)(const struct clap_plugin *plugin)` | `[main-thread & active]` | |
| `start_processing` | `bool(CLAP_ABI *start_processing)(const struct clap_plugin *plugin)` | `[audio-thread & active & !processing]` | |
| `stop_processing` | `void(CLAP_ABI *stop_processing)(const struct clap_plugin *plugin)` | `[audio-thread & active & processing]` | |
| `reset` | `void(CLAP_ABI *reset)(const struct clap_plugin *plugin)` | `[audio-thread & active]` | |
| `process` | `clap_process_status(CLAP_ABI *process)(const struct clap_plugin *plugin, const clap_process_t *process)` | `[audio-thread & active & processing]` | |
| `get_extension` | `const void *(CLAP_ABI *get_extension)(const struct clap_plugin *plugin, const char *id)` | `[thread-safe]` | |
| `on_main_thread` | `void(CLAP_ABI *on_main_thread)(const struct clap_plugin *plugin)` | `[main-thread]` | |

**Lifecycle contract, verbatim:**
> "Must be called after creating the plugin."
> "If init returns false, the host must destroy the plugin instance."
> "If init returns true, then the plugin is initialized and in the deactivated state."
> "Unlike in `plugin-factory::create_plugin`, in init you have complete access to the host and host extensions, so clap related setup activities should be done here rather than in create_plugin."
> "It is required to deactivate the plugin prior to this call." (destroy)
> "In this call the plugin may allocate memory and prepare everything needed for the process call. The process's sample rate will be constant and process's frame count will included in the [min, max] range, which is bounded by [1, INT32_MAX]."
> "In this call the plugin may call host-provided methods marked [being-activated]."
> **"Once activated the latency and port configuration must remain constant, until deactivation."**
> "Call start processing before processing." / "Call stop processing before sending the plugin to sleep."
> reset: "- Clears all buffers, performs a full reset of the processing state (filters, oscillators, envelopes, lfo, ...) and kills all voices. - The parameter's value remain unchanged. - clap_process.steady_time may jump backward."
> "All the pointers coming from clap_process_t and its nested attributes, are valid until process() returns."

**Extension query validity — before `init()` / after `destroy()`:**
> "The returned pointer is owned by the plugin and is valid until the call to plugin->destroy()."
> **"It is forbidden to call it before plugin->init()."**
> "You can call it within plugin->init() call, and after."

## B.4 `clap_host` (`host.h`) — plugin → host. **We implement all of this.**

| Field | Signature | Thread annotation |
|---|---|---|
| `clap_version` | `clap_version_t` | data — "initialized to CLAP_VERSION" |
| `host_data` | `void *` | data — "reserved pointer for the host" |
| `name` / `vendor` / `url` / `version` | `const char *` | data — "name and version are mandatory." |
| `get_extension` | `const void *(CLAP_ABI *get_extension)(const struct clap_host *host, const char *extension_id)` | `[thread-safe]` |
| `request_restart` | `void(CLAP_ABI *request_restart)(const struct clap_host *host)` | `[thread-safe]` |
| `request_process` | `void(CLAP_ABI *request_process)(const struct clap_host *host)` | `[thread-safe]` |
| `request_callback` | `void(CLAP_ABI *request_callback)(const struct clap_host *host)` | `[thread-safe]` |

> "The returned pointer is owned by the host and is valid until after the call to plugin->destroy()"
> **"It is forbidden to call it before plugin->init()."** / "You can call it within plugin->init() call, and after."
> "Request the host to deactivate and then reactivate the plugin. The operation may be delayed by the host."
> "Request the host to activate and start processing the plugin. This is useful if you have external IO and need to wake up the plugin from 'sleep'."
> "Request the host to schedule a call to plugin->on_main_thread(plugin) on the main thread. This callback should be called as soon as practicable, usually in the host application's next available main thread time slice. Typically callbacks occur within 33ms / 30hz. Despite this guidance, plugins should not make assumptions about the exactness of timing for a main thread callback, but hosts should endeavour to be prompt. For example, in high load situations the environment may starve the gui/main thread in favor of audio processing, leading to substantially longer latencies for the callback than the indicative times given here."

## B.5 `clap_process` (`process.h`)

`clap_process_status` = `int32_t`:

| Constant | Value | Meaning (verbatim) |
|---|---|---|
| `CLAP_PROCESS_ERROR` | 0 | "Processing failed. The output buffer must be discarded." |
| `CLAP_PROCESS_CONTINUE` | 1 | "Processing succeeded, keep processing." |
| `CLAP_PROCESS_CONTINUE_IF_NOT_QUIET` | 2 | "Processing succeeded, keep processing if the output is not quiet." |
| `CLAP_PROCESS_TAIL` | 3 | "Rely upon the plugin's tail to determine if the plugin should continue to process. see clap_plugin_tail" |
| `CLAP_PROCESS_SLEEP` | 4 | "Processing succeeded, but no more processing is required, until the next event or variation in audio input." |

| Field | Type | Doc (verbatim) |
|---|---|---|
| `steady_time` | `int64_t` | "A steady sample time counter. This field can be used to calculate the sleep duration between two process calls. This value may be specific to this plugin instance and have no relation to what other plugin instances may receive. Set to -1 if not available, otherwise the value must be greater or equal to 0, and must be increased by at least `frames_count` for the next call to process." |
| `frames_count` | `uint32_t` | "Number of frames to process" |
| `transport` | `const clap_event_transport_t *` | "time info at sample 0. If null, then this is a free running host, no transport events will be provided" |
| `audio_inputs` | `const clap_audio_buffer_t *` | "Audio buffers, they must have the same count as specified by clap_plugin_audio_ports->count(). The index maps to clap_plugin_audio_ports->get(). Input buffer and its contents are read-only." |
| `audio_outputs` | `clap_audio_buffer_t *` | (same block) |
| `audio_inputs_count` | `uint32_t` | |
| `audio_outputs_count` | `uint32_t` | |
| `in_events` | `const clap_input_events_t *` | "The input event list can't be modified. Input read-only event list. The host will deliver these sorted in sample order." |
| `out_events` | `const clap_output_events_t *` | "Output event list. The plugin must insert events in sample sorted order when inserting events" |

## B.6 `clap_audio_buffer` (`audio-buffer.h`)

| Field | Type | Doc |
|---|---|---|
| `data32` | `float **` | "Either data32 or data64 pointer will be set." |
| `data64` | `double **` | (same comment covers both) |
| `channel_count` | `uint32_t` | |
| `latency` | `uint32_t` | "latency from/to the audio interface" |
| `constant_mask` | `uint64_t` | bit `1 << channel_index` |

**The constant_mask rule, verbatim — the trap for a host:**
> "Note: checking the constant mask is optional, and this implies that the buffer must be filled with the constant value."
> "Rationale: if a buffer reader doesn't check the constant mask, then it may process garbage samples and in result, garbage samples may be transmitted to the audio interface with all the bad consequences it can have."
> "The constant mask is a hint."

## B.7 `clap_istream` / `clap_ostream` (`stream.h`)

| Struct | Field | Signature | Thread annotation |
|---|---|---|---|
| `clap_istream` | `ctx` | `void *` | data — "reserved pointer for the stream" |
| `clap_istream` | `read` | `int64_t(CLAP_ABI *read)(const struct clap_istream *stream, void *buffer, uint64_t size)` | **unannotated** |
| `clap_ostream` | `ctx` | `void *` | data |
| `clap_ostream` | `write` | `int64_t(CLAP_ABI *write)(const struct clap_ostream *stream, const void *buffer, uint64_t size)` | **unannotated** |

Return-value semantics, verbatim:
> "returns the number of bytes read; 0 indicates end of file and -1 a read error"
> "returns the number of bytes written; -1 on write error"

> "When working with `clap_istream` and `clap_ostream` objects to load and save state, it is important to keep in mind that the host may limit the number of bytes that can be read or written at a time. The return values for the stream read and write functions indicate how many bytes were actually read or written. You need to use a loop to ensure that you read or write the entirety of your state. Don't forget to also consider the negative return values for the end of file and IO error codes."

Note: the header states **no** blocking/non-blocking semantics, and gives **no** meaning for a `0`
return from `write`.

## B.8 Events (`events.h`)

**`events.h` contains zero thread annotations.** All three function pointers below are
**unannotated**; their thread context comes from the calling site (`process`/`flush`).

### `clap_event_header`

| Field | Type | Doc (verbatim) |
|---|---|---|
| `size` | `uint32_t` | "event size including this header, eg: sizeof (clap_event_note)" |
| `time` | `uint32_t` | "sample offset within the buffer for this event" |
| `space_id` | `uint16_t` | "event space, see clap_host_event_registry" |
| `type` | `uint16_t` | "event type" |
| `flags` | `uint32_t` | "see clap_event_flags" |

> "clap_event objects are contiguous regions of memory which can be copied with a memcpy of `size` bytes starting at the top of the header. As such, be very careful when designing clap events with internal pointers and other non-value-types to consider the lifetime of those members."

`CLAP_CORE_EVENT_SPACE_ID = 0` — "The clap core event space".

### `enum clap_event_flags`

| Flag | Value | Doc (verbatim) |
|---|---|---|
| `CLAP_EVENT_IS_LIVE` | `1 << 0` | "Indicate a live user event, for example a user turning a physical knob or playing a physical key." |
| `CLAP_EVENT_DONT_RECORD` | `1 << 1` | "Indicate that the event should not be recorded. For example this is useful when a parameter changes because of a MIDI CC, because if the host records both the MIDI CC automation and the parameter automation there will be a conflict." |

### All 13 event types (anonymous enum — there is no `clap_event_type` typedef)

| Constant | Value | Struct |
|---|---|---|
| `CLAP_EVENT_NOTE_ON` | 0 | `clap_event_note` |
| `CLAP_EVENT_NOTE_OFF` | 1 | `clap_event_note` |
| `CLAP_EVENT_NOTE_CHOKE` | 2 | `clap_event_note` |
| `CLAP_EVENT_NOTE_END` | 3 | `clap_event_note` |
| `CLAP_EVENT_NOTE_EXPRESSION` | 4 | `clap_event_note_expression` |
| `CLAP_EVENT_PARAM_VALUE` | 5 | `clap_event_param_value` |
| `CLAP_EVENT_PARAM_MOD` | 6 | `clap_event_param_mod` |
| `CLAP_EVENT_PARAM_GESTURE_BEGIN` | 7 | `clap_event_param_gesture` |
| `CLAP_EVENT_PARAM_GESTURE_END` | 8 | `clap_event_param_gesture` |
| `CLAP_EVENT_TRANSPORT` | 9 | `clap_event_transport` — "update the transport info" |
| `CLAP_EVENT_MIDI` | 10 | `clap_event_midi` — "raw midi event" |
| `CLAP_EVENT_MIDI_SYSEX` | 11 | `clap_event_midi_sysex` — "raw midi sysex event" |
| `CLAP_EVENT_MIDI2` | 12 | `clap_event_midi2` — "raw midi 2 event" |

**Encoding-overlap rule, verbatim:**
> "The preferred way of sending a note event is to use CLAP_EVENT_NOTE_*."
> **"The same event must not be sent twice: it is forbidden to send a the same note on encoded with both CLAP_EVENT_NOTE_ON and CLAP_EVENT_MIDI."**
> "The plugins are encouraged to be able to handle note events encoded as raw midi or midi2, or implement clap_plugin_event_filter and reject raw midi and midi2 events."

**Note semantics, verbatim:**
> "NOTE_ON and NOTE_OFF represent a key pressed and key released event, respectively."
> "A NOTE_ON with a velocity of 0 is valid and should not be interpreted as a NOTE_OFF."
> "NOTE_CHOKE is meant to choke the voice(s)… This event can be sent by the host to the plugin."
> "NOTE_END is sent by the plugin to the host. The port, channel, key and note_id are those given by the host in the NOTE_ON event. In other words, this event is matched against the plugin's note input port."
> "CLAP assumes that the host will allocate a unique voice on NOTE_ON event for a given port, channel and key. This voice will run until the plugin will instruct the host to terminate it by sending a NOTE_END event."
> "When using polyphonic modulations, the host has to allocate and release voices for its polyphonic modulator. Yet only the plugin effectively knows when the host should terminate a voice."

### `clap_event_note` and the PCKN wildcard rules

| Field | Type | Doc (verbatim) |
|---|---|---|
| `note_id` | `int32_t` | "host provided note id >= 0, or -1 if unspecified or wildcard" |
| `port_index` | `int16_t` | "port index from ext/note-ports; -1 for wildcard" |
| `channel` | `int16_t` | "0..15, same as MIDI1 Channel Number, -1 for wildcard" |
| `key` | `int16_t` | "0..127, same as MIDI1 Key Number (60==Middle C), -1 for wildcard" |
| `velocity` | `double` | "0..1" |

> "Clap addresses notes and voices using the 4-value tuple (port, channel, key, note_id)."
> "Values in a note and voice address are either >= 0 if they are specified, or -1 to indicate a wildcard. A wildcard means a voice with any value in that part of the tuple matches the message."
> "For instance, a (PCKN) of (0, 3, -1, -1) will match all voices on channel 3 of port 0. And a PCKN of (-1, 0, 60, -1) will match all channel 0 key 60 voices, independent of port or note id."
> "…a host may choose to issue a note id only at note on. So you may see a message stream like `CLAP_EVENT_NOTE_ON [0,0,60,184]` / `CLAP_EVENT_NOTE_OFF [0,0,60,-1]` and the host will expect the first voice to be released. Well constructed plugins will search for voices and notes using the entire tuple."
> "In the case of note on events: - The port, channel and key must be specified with a value >= 0 - A note-on event with a '-1' for port, channel or key is invalid and can be rejected or ignored by a plugin or host. - A host which does not support note ids should set the note id to -1."
> "In the case of note choke or end events: - the velocity is ignored. - key and channel are used to match active notes - note_id is optionally provided by the host"

### `clap_event_note_expression` + `CLAP_NOTE_EXPRESSION_*`

`typedef int32_t clap_note_expression;`

| Constant | Value | Range (verbatim) |
|---|---|---|
| `CLAP_NOTE_EXPRESSION_VOLUME` | 0 | "with 0 < x <= 4, plain = 20 * log(x)" |
| `CLAP_NOTE_EXPRESSION_PAN` | 1 | "pan, 0 left, 0.5 center, 1 right" |
| `CLAP_NOTE_EXPRESSION_TUNING` | 2 | "Relative tuning in semitones, from -120 to +120. Semitones are in equal temperament and are doubles; the resulting note would be retuned by `100 * evt->value` cents." |
| `CLAP_NOTE_EXPRESSION_VIBRATO` | 3 | "0..1" |
| `CLAP_NOTE_EXPRESSION_EXPRESSION` | 4 | "0..1" |
| `CLAP_NOTE_EXPRESSION_BRIGHTNESS` | 5 | "0..1" |
| `CLAP_NOTE_EXPRESSION_PRESSURE` | 6 | "0..1" |

Fields: `header`, `expression_id`, `note_id` (`int32_t`), `port_index`/`channel`/`key` (`int16_t`),
`value` (`double`, "see expression for the range").

> "Note Expressions are well named modifications of a voice targeted to voices using the same wildcard rules described above. Note Expressions are delivered as sample accurate events and should be applied at the sample when received."
> "Note expressions are a statement of value, not cumulative. A PAN event of 0 followed by 1 followed by 0.5 would pan hard left, hard right, and center. They are intended as an offset from the non-note-expression voice default."
> "A plugin which receives a note expression at the same sample as a NOTE_ON event should apply that expression to all generated samples."

### `clap_event_param_value` / `clap_event_param_mod` / `clap_event_param_gesture`

`param_value`: `header`, `param_id` (`clap_id`, "@ref clap_param_info.id"), `cookie` (`void *`,
"@ref clap_param_info.cookie"), `note_id`/`port_index`/`channel`/`key`, `value` (`double`).
`param_mod`: identical but final field is `amount` (`double`, "modulation amount").
`param_gesture`: `header` + `param_id` only — no note address.

> "PARAM_VALUE sets the parameter's value; PARAM_MOD sets the parameter's modulation amount."
> **"The value heard is: param_value + param_mod."**
> "In case of a concurrent global value/modulation versus a polyphonic one, the voice should only use the polyphonic one and the polyphonic modulation amount will already include the monophonic signal."
> "Indicates that the user started or finished adjusting a knob. This is not mandatory to wrap parameter changes with gesture events, but this improves the user experience a lot when recording automation or overriding automation playback."

### `clap_event_transport` + `enum clap_transport_flags`

All 8 flags (the enum carries no per-value doc comments):
`CLAP_TRANSPORT_HAS_TEMPO` `1<<0`, `HAS_BEATS_TIMELINE` `1<<1`, `HAS_SECONDS_TIMELINE` `1<<2`,
`HAS_TIME_SIGNATURE` `1<<3`, `IS_PLAYING` `1<<4`, `IS_RECORDING` `1<<5`, `IS_LOOP_ACTIVE` `1<<6`,
`IS_WITHIN_PRE_ROLL` `1<<7`.

| Field | Type | Doc |
|---|---|---|
| `flags` | `uint32_t` | "see clap_transport_flags" |
| `song_pos_beats` | `clap_beattime` | "position in beats" |
| `song_pos_seconds` | `clap_sectime` | "position in seconds" |
| `tempo` | `double` | "in bpm" |
| `tempo_inc` | `double` | "tempo increment for each sample and until the next time info event" |
| `loop_start_beats` / `loop_end_beats` | `clap_beattime` | |
| `loop_start_seconds` / `loop_end_seconds` | `clap_sectime` | |
| `bar_start` | `clap_beattime` | "start pos of the current bar" |
| `bar_number` | `int32_t` | "bar at song pos 0 has the number 0" |
| `tsig_num` / `tsig_denom` | `uint16_t` | "time signature numerator" / "denominator" |

> "clap_event_transport provides song position, tempo, and similar information from the host to the plugin. There are two ways a host communicates these values. In the `clap_process` structure sent to each processing block, the host may provide a transport structure which indicates the available information at the start of the block. If the host provides sample-accurate tempo or transport changes, it can also provide subsequent inter-block transport updates by delivering a new event."

### `clap_event_midi` / `_midi_sysex` / `_midi2`

- `clap_event_midi`: `header`, `port_index` (`uint16_t`), `data[3]` (`uint8_t`). No doc comments.
- `clap_event_midi_sysex`: `header`, `port_index`, `buffer` (`const uint8_t *`), `size` (`uint32_t`).
- `clap_event_midi2`: `header`, `port_index`, `data[4]` (`uint32_t`).

**Sysex lifetime — a direct host obligation, verbatim:**
> "The lifetime of this buffer is (from host->plugin) only the process call in which the event is delivered or (from plugin->host) only the duration of a try_push call."
> **"Since `clap_output_events.try_push` requires hosts to make a copy of an event, host implementers receiving sysex messages from plugins need to take care to both copy the event (so header, size, etc...) but also memcpy the contents of the sysex pointer to host-owned memory, and not just copy the data pointer."**
> "Similarly plugins retaining the sysex outside the lifetime of a single process call must copy the sysex buffer to plugin-owned memory."
> "As a consequence, the data structure pointed to by the sysex buffer must be contiguous and copyable with `memcpy` of `size` bytes."
> "While it is possible to use a series of midi2 event to send a sysex, prefer clap_event_midi_sysex if possible for efficiency."

### `clap_input_events` / `clap_output_events`

| Struct | Field | Signature | Thread annotation |
|---|---|---|---|
| `clap_input_events` | `ctx` | `void *` | data |
| `clap_input_events` | `size` | `uint32_t(CLAP_ABI *size)(const struct clap_input_events *list)` | **unannotated** |
| `clap_input_events` | `get` | `const clap_event_header_t *(CLAP_ABI *get)(const struct clap_input_events *list, uint32_t index)` | **unannotated** |
| `clap_output_events` | `ctx` | `void *` | data |
| `clap_output_events` | `try_push` | `bool(CLAP_ABI *try_push)(const struct clap_output_events *list, const clap_event_header_t *event)` | **unannotated** |

**Sorting, verbatim:**
> "Input event list. The host will deliver these sorted in sample order."
> "Output event list. The plugin must insert events in sample sorted order when inserting events"

**Ownership, verbatim:**
> "Don't free the returned event, it belongs to the list"
> "Pushes a copy of the event / returns false if the event could not be pushed to the queue (out of memory?)"

## B.9 `clap_preset_discovery_factory` (`factory/preset-discovery.h`) — STABLE

`CLAP_PRESET_DISCOVERY_FACTORY_ID = "clap.preset-discovery-factory/2"`
`CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT = "clap.preset-discovery-factory/draft-2"`

**Architectural flow, verbatim:**
> "1. clap_plugin_entry.get_factory(CLAP_PRESET_DISCOVERY_FACTORY_ID) 2. clap_preset_discovery_factory_t.create(...) 3. clap_preset_discovery_provider.init() (only necessary the first time, declarations can be cached) `-> clap_preset_discovery_indexer.declare_filetype() `-> clap_preset_discovery_indexer.declare_location() `-> clap_preset_discovery_indexer.declare_soundpack() (optional) `-> clap_preset_discovery_indexer.set_invalidation_watch_file() (optional) 4. crawl the given locations and monitor file system changes `-> clap_preset_discovery_indexer.get_metadata() for each presets files"

> "The design of this API deliberately does not define a fixed set tags or categories. It is the plug-in host's job to try to intelligently map the raw list of features that are found for a preset and to process this list to generate something that makes sense for the host's tagging and categorization system."
> **"VERY IMPORTANT: - the whole indexing process has to be **fast** - clap_preset_provider->get_metadata() has to be fast and avoid unnecessary operations - the whole indexing process must not be interactive - don't show dialogs, windows, ... - don't ask for user input"**

*(Note: the doc flow mentions `indexer.set_invalidation_watch_file()` and `indexer.get_metadata()`,
but neither exists in the actual structs — `get_metadata` is on the **provider**. A documented
discrepancy in the header itself.)*

### `clap_preset_discovery_factory` — host → plugin

| Field | Signature | Thread annotation |
|---|---|---|
| `count` | `uint32_t(CLAP_ABI *count)(const struct clap_preset_discovery_factory *factory)` | `[thread-safe]` |
| `get_descriptor` | `const clap_preset_discovery_provider_descriptor_t *(CLAP_ABI *)(const struct clap_preset_discovery_factory *factory, uint32_t index)` | `[thread-safe]` |
| `create` | `const clap_preset_discovery_provider_t *(CLAP_ABI *)(const struct clap_preset_discovery_factory *factory, const clap_preset_discovery_indexer_t *indexer, const char *provider_id)` | `[thread-safe]` |

> "Every methods in this factory must be thread-safe."
> "It is encouraged to perform preset indexing in background threads, maybe even in background process."
> "The descriptor must not be freed." / "The returned pointer must be freed by calling preset_provider->destroy(preset_provider);"
> **"The preset provider is not allowed to use the indexer callbacks in the create method."**
> **"It is forbidden to call back into the indexer before the indexer calls provider->init()."**

### `clap_preset_discovery_provider` — host → plugin. **All fields unannotated.**

> "This interface isn't thread-safe."

| Field | Signature | Thread annotation |
|---|---|---|
| `desc` / `provider_data` | data | — |
| `init` | `bool(CLAP_ABI *init)(const struct clap_preset_discovery_provider *provider)` | **unannotated** |
| `destroy` | `void(CLAP_ABI *destroy)(const struct clap_preset_discovery_provider *provider)` | **unannotated** |
| `get_metadata` | `bool(CLAP_ABI *get_metadata)(const struct clap_preset_discovery_provider *provider, uint32_t location_kind, const char *location, const clap_preset_discovery_metadata_receiver_t *metadata_receiver)` | **unannotated** |
| `get_extension` | `const void *(CLAP_ABI *get_extension)(const struct clap_preset_discovery_provider *provider, const char *extension_id)` | **unannotated** |

> "It should declare all its locations, filetypes and sound packs." / "Returns false if initialization failed."
> "It is forbidden to call it before provider->init()." / "You can call it within provider->init() call, and after."

### `clap_preset_discovery_indexer` — plugin → host. **We implement. All fields unannotated.**

> "This interface isn't thread-safe"

Data: `clap_version` ("initialized to CLAP_VERSION"), `name` (`eg: "Bitwig Studio"`), `vendor`,
`url`, `version`, `indexer_data`.

| Field | Signature | Thread annotation |
|---|---|---|
| `declare_filetype` | `bool(CLAP_ABI *)(const struct clap_preset_discovery_indexer *indexer, const clap_preset_discovery_filetype_t *filetype)` | **unannotated** |
| `declare_location` | `bool(CLAP_ABI *)(const struct clap_preset_discovery_indexer *indexer, const clap_preset_discovery_location_t *location)` | **unannotated** |
| `declare_soundpack` | `bool(CLAP_ABI *)(const struct clap_preset_discovery_indexer *indexer, const clap_preset_discovery_soundpack_t *soundpack)` | **unannotated** |
| `get_extension` | `const void *(CLAP_ABI *)(const struct clap_preset_discovery_indexer *indexer, const char *extension_id)` | **unannotated** |

> **"Don't callback into the provider during this call."** (repeated on all three `declare_*`)
> "The returned pointer is owned by the indexer."

### `clap_preset_discovery_metadata_receiver` — plugin → host. **We implement. All unannotated.**

> "Receiver that receives the metadata for a single preset file. The host would define the various callbacks in this interface and the preset parser function would then call them."
> "This interface isn't thread-safe."

| Field | Signature | Thread annotation |
|---|---|---|
| `receiver_data` | `void *` | data |
| `on_error` | `void(CLAP_ABI *)(const struct ... *receiver, int32_t os_error, const char *error_message)` | **unannotated** |
| `begin_preset` | `bool(CLAP_ABI *)(const struct ... *receiver, const char *name, const char *load_key)` | **unannotated** |
| `add_plugin_id` | `void(CLAP_ABI *)(const struct ... *receiver, const clap_universal_plugin_id_t *plugin_id)` | **unannotated** |
| `set_soundpack_id` | `void(CLAP_ABI *)(const struct ... *receiver, const char *soundpack_id)` | **unannotated** |
| `set_flags` | `void(CLAP_ABI *)(const struct ... *receiver, uint32_t flags)` | **unannotated** |
| `add_creator` | `void(CLAP_ABI *)(const struct ... *receiver, const char *creator)` | **unannotated** |
| `set_description` | `void(CLAP_ABI *)(const struct ... *receiver, const char *description)` | **unannotated** |
| `set_timestamps` | `void(CLAP_ABI *)(const struct ... *receiver, clap_timestamp creation_time, clap_timestamp modification_time)` | **unannotated** |
| `add_feature` | `void(CLAP_ABI *)(const struct ... *receiver, const char *feature)` | **unannotated** |
| `add_extra_info` | `void(CLAP_ABI *)(const struct ... *receiver, const char *key, const char *value)` | **unannotated** |

> **"This must be called for every preset in the file and before any preset metadata is sent with the calls below."** (begin_preset)
> "If the preset file is a preset container then name and load_key are mandatory, otherwise they are optional."
> **"If the function returns false, then the provider must stop calling back into the receiver."**
> "If unset, they are then inherited from the location." (set_flags)
> "If one of the times isn't known, set it to CLAP_TIMESTAMP_UNKNOWN." / "If this function is not called, then the indexer may look at the file's creation and modification time."
> "The feature string is arbitrary, it is the indexer's job to understand it and remap it to its internal categorization and tagging system."

### Preset-discovery enums

`enum clap_preset_discovery_location_kind`:

| Constant | Value | Meaning (verbatim) |
|---|---|---|
| `CLAP_PRESET_DISCOVERY_LOCATION_FILE` | 0 | "The preset are located in a file on the OS filesystem… So both '/' and '\' shall work on Windows as a separator." |
| `CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN` | 1 | "The preset is bundled within the plugin DSO itself. The location must then be null…" |

`enum clap_preset_discovery_flags`:

| Constant | Value | Meaning (verbatim) |
|---|---|---|
| `CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT` | `1<<0` | "This is for factory or sound-pack presets." |
| `CLAP_PRESET_DISCOVERY_IS_USER_CONTENT` | `1<<1` | "This is for user presets." |
| `CLAP_PRESET_DISCOVERY_IS_DEMO_CONTENT` | `1<<2` | "This location is meant for demo presets, those are preset which may trigger some limitation in the plugin…" |
| `CLAP_PRESET_DISCOVERY_IS_FAVORITE` | `1<<3` | "This preset is a user's favorite" |

Structs: `clap_preset_discovery_filetype` {`name`, `description`, `file_extension` — "`.' isn't
included in the string. If empty or NULL then every file should be matched."};
`clap_preset_discovery_location` {`flags`, `name`, `kind`, `location`};
`clap_preset_discovery_soundpack` {`flags`, `id`, `name`, `description`, `homepage_url`, `vendor`,
`image_path`, `release_timestamp`};
`clap_preset_discovery_provider_descriptor` {`clap_version`, `id`, `name`, `vendor`}.

## B.10 `clap_plugin_invalidation_factory` (`factory/draft/plugin-invalidation.h`) — DRAFT

`CLAP_PLUGIN_INVALIDATION_FACTORY_ID = "clap.plugin-invalidation-factory/1"`. No compat alias.

| Field | Signature | Thread annotation |
|---|---|---|
| `count` | `uint32_t(CLAP_ABI *count)(const struct clap_plugin_invalidation_factory *factory)` | **unannotated** |
| `get` | `const clap_plugin_invalidation_source_t *(CLAP_ABI *get)(const struct clap_plugin_invalidation_factory *factory, uint32_t index)` | `[thread-safe]` |
| `refresh` | `bool(CLAP_ABI *refresh)(const struct clap_plugin_invalidation_factory *factory)` | **unannotated** |

Note the asymmetry: only `get` is annotated.

`clap_plugin_invalidation_source` {`directory` — "must be absolute", `filename_glob` — "in the form
*.dll", `recursive_scan`}.

> "Imagine a situation with a single entry point: my-plugin.clap which then scans itself a set of 'sub-plugins'. New plugin may be available even if my-plugin.clap file doesn't change."
> "In case the host detected a invalidation event, it can call refresh() to let the plugin_entry update the set of plugins available. If the function returned false, then the plugin needs to be reloaded."

## B.11 `clap_plugin_state_converter_factory` (`factory/draft/plugin-state-converter.h`) — DRAFT

`CLAP_PLUGIN_STATE_CONVERTER_FACTORY_ID = "clap.plugin-state-converter-factory/1"`. No compat alias.

`clap_plugin_state_converter_factory` (host → plugin):

| Field | Signature | Thread annotation |
|---|---|---|
| `count` | `uint32_t(CLAP_ABI *count)(const struct clap_plugin_state_converter_factory *factory)` | `[thread-safe]` |
| `get_descriptor` | `const clap_plugin_state_converter_descriptor_t *(CLAP_ABI *)(const struct ... *factory, uint32_t index)` | `[thread-safe]` |
| `create` | `clap_plugin_state_converter_t *(CLAP_ABI *)(const struct ... *factory, const char *converter_id)` | `[thread-safe]` |

`clap_plugin_state_converter` (host → plugin) — note **non-const** receiver, unlike the rest of CLAP:

| Field | Signature | Thread annotation |
|---|---|---|
| `desc` / `converter_data` | data | — |
| `destroy` | `void(CLAP_ABI *destroy)(struct clap_plugin_state_converter *converter)` | **unannotated** |
| `convert_state` | `bool(CLAP_ABI *)(struct clap_plugin_state_converter *converter, const clap_istream_t *src, const clap_ostream_t *dst, char *error_buffer, size_t error_buffer_size)` | `[thread-safe]` |
| `convert_normalized_value` | `bool(CLAP_ABI *)(struct ... *converter, clap_id src_param_id, double src_normalized_value, clap_id *dst_param_id, double *dst_normalized_value)` | `[thread-safe]` |
| `convert_plain_value` | `bool(CLAP_ABI *)(struct ... *converter, clap_id src_param_id, double src_plain_value, clap_id *dst_param_id, double *dst_plain_value)` | `[thread-safe]` |

> "This is useful to convert from one plugin ABI to another one." / "This is also useful to offer an upgrade path: from EQ version 1 to EQ version 2."
> "error_buffer is a place holder of error_buffer_size bytes for storing a null-terminated error message in case of failure, which can be displayed to the user."
> "The returned pointer must be freed by calling converter->destroy(converter);"

`clap_plugin_state_converter_descriptor` {`clap_version`, `src_plugin_id`, `dst_plugin_id` (both
`clap_universal_plugin_id_t`), `id`, `name`, `vendor`, `version`, `description`}.

---

# Part A — Extensions

## A.1 Master index

**Stable (27), `ext/*.h`:**

| Extension | ID string | COMPAT alias | plugin struct | host struct |
|---|---|---|---|---|
| ambisonic | `clap.ambisonic/3` | `clap.ambisonic.draft/3` | yes (2) | yes (1) |
| audio-ports | `clap.audio-ports` | — | yes (2) | yes (2) |
| audio-ports-activation | `clap.audio-ports-activation/2` | `clap.audio-ports-activation/draft-2` | yes (2) | — |
| audio-ports-config | `clap.audio-ports-config` | — | yes (3) | yes (1) |
| audio-ports-config-info | `clap.audio-ports-config-info/1` | `clap.audio-ports-config-info/draft-0` | yes (2) | — |
| configurable-audio-ports | `clap.configurable-audio-ports/1` | `clap.configurable-audio-ports.draft1` | yes (2) | — |
| context-menu | `clap.context-menu/1` | `clap.context-menu.draft/0` | yes (2) | yes (4) |
| event-registry | `clap.event-registry` | — | — | yes (1) |
| gui | `clap.gui` | — | yes (15) | yes (5) |
| latency | `clap.latency` | — | yes (1) | yes (1) |
| log | `clap.log` | — | — | yes (1) |
| note-name | `clap.note-name` | — | yes (2) | yes (1) |
| note-ports | `clap.note-ports` | — | yes (2) | yes (2) |
| param-indication | `clap.param-indication/4` | `clap.param-indication.draft/4` | yes (2) | — |
| params | `clap.params` | — | yes (6) | yes (3) |
| posix-fd-support | `clap.posix-fd-support` | — | yes (1) | yes (3) |
| preset-load | `clap.preset-load/2` | `clap.preset-load.draft/2` | yes (1) | yes (2) |
| remote-controls | `clap.remote-controls/2` | `clap.remote-controls.draft/2` | yes (2) | yes (2) |
| render | `clap.render` | — | yes (2) | — |
| state | `clap.state` | — | yes (2) | yes (1) |
| state-context | `clap.state-context/2` | — | yes (2) | — |
| surround | `clap.surround/4` | `clap.surround.draft/4` | yes (2) | yes (1) |
| tail | `clap.tail` | — | yes (1) | yes (1) |
| thread-check | `clap.thread-check` | — | — | yes (2) |
| thread-pool | `clap.thread-pool` | — | yes (1) | yes (1) |
| timer-support | `clap.timer-support` | — | yes (1) | yes (2) |
| track-info | `clap.track-info/1` | `clap.track-info.draft/1` | yes (1) | yes (1) |
| voice-info | `clap.voice-info` | — | yes (1) | yes (1) |

*(28 rows: `audio-ports-config.h` defines two extension IDs — the base config and the
`audio-ports-config-info/1` sub-extension — from one of the 27 stable headers.)*

**Draft (18), `ext/draft/*.h`:**

| Extension | ID string | plugin struct | host struct |
|---|---|---|---|
| background-activation | `clap.background-activation/1` | yes (2) | — |
| background-progress | `clap.background-progress/1` | — | yes (2) |
| background-state-context | `clap.background-state-context/1` | yes (2) | — |
| extensible-audio-ports | `clap.extensible-audio-ports/1` | yes (2) | — |
| flush-events | `clap.flush-events/1` | yes (1) | yes (1) |
| gain-adjustment-metering | `clap.gain-adjustment-metering/0` | yes (1) | — |
| mini-curve-display | `clap.mini-curve-display/3` | yes (4) | yes (3) |
| octave-number | `clap.octave-number/1` | yes (1) | — |
| param-hovered | `clap.param-hovered/1` | — | yes (1) |
| params-origin | `clap.params-origin/1` | yes (1) | yes (1) |
| project-location | `clap.project-location/2` | yes (1) | — |
| resource-directory | `clap.resource-directory/1` | yes (4) | yes (2) |
| scratch-memory | `clap.scratch-memory/1` | — | yes (2) |
| transport-control | `clap.transport-control/2` | — | yes (13) |
| triggers | `clap.triggers/1` | yes (2) | yes (2) |
| tuning | `clap.tuning/2` | yes (1) | yes (4) |
| undo | `clap.undo/4` | — | yes (6) |
| undo (context) | `clap.undo_context/4` | yes (4) | — |
| undo (delta) | `clap.undo_delta/4` | yes (4) | — |
| webview | `clap.webview/3` | yes (3) | yes (1) |

*(20 rows from 18 headers: `undo.h` defines 3 IDs.)*

**All 8 `_COMPAT` aliases** carry the same note: *"The latest draft is 100% compatible. This compat
ID may be removed in 2026."* Two are irregular and worth hardcoding carefully:
`clap.configurable-audio-ports.draft1` (dot, no slash before `draft1`) and
`clap.context-menu.draft/0` (stable is `/1` but compat is `.draft/0`).

---

## A.2 Audio ports family

### `clap.audio-ports` (`ext/audio-ports.h`) — STABLE

> "This extension provides a way for the plugin to describe its current audio ports."

**host → plugin — `clap_plugin_audio_ports`**

| Field | Signature | Thread | Notes |
|---|---|---|---|
| `count` | `uint32_t(CLAP_ABI *count)(const clap_plugin_t *plugin, bool is_input)` | `[main-thread]` | |
| `get` | `bool(CLAP_ABI *get)(const clap_plugin_t *plugin, uint32_t index, bool is_input, clap_audio_port_info_t *info)` | `[main-thread]` | |

Struct-level: "The audio ports scan has to be done while the plugin is deactivated."

**plugin → host — `clap_host_audio_ports`** (we implement)

| Field | Signature | Thread |
|---|---|---|
| `is_rescan_flag_supported` | `bool(CLAP_ABI *)(const clap_host_t *host, uint32_t flag)` | `[main-thread]` |
| `rescan` | `void(CLAP_ABI *rescan)(const clap_host_t *host, uint32_t flags)` | `[main-thread]` |

Port flags: `CLAP_AUDIO_PORT_IS_MAIN` `1<<0`, `SUPPORTS_64BITS` `1<<1`, `PREFERS_64BITS` `1<<2`,
`REQUIRES_COMMON_SAMPLE_SIZE` `1<<3`.
Rescan flags: `NAMES` `1<<0`, `FLAGS` `1<<1` *(`[!active]`)*, `CHANNEL_COUNT` `1<<2` *(`[!active]`)*,
`PORT_TYPE` `1<<3` *(`[!active]`)*, `IN_PLACE_PAIR` `1<<4` *(`[!active]`)*, `LIST` `1<<5`
*(`[!active]`)*.
Port types: `CLAP_PORT_MONO = "mono"`, `CLAP_PORT_STEREO = "stereo"`.

`clap_audio_port_info` {`id`, `name[CLAP_NAME_SIZE]`, `flags`, `channel_count`, `port_type`,
`in_place_pair`}.

> "If the plugin does not implement this extension, it won't have audio ports."
> "32 bits support is required for both host and plugins. 64 bits audio is optional."
> **"The plugin is only allowed to change its ports configuration while it is deactivated."**
> "There can be only one main input and main output. Main port must be at index 0."
> "id identifies a port and must be stable. id may overlap between input and output ports."
> **"It is illegal to ask the host to rescan with a flag that is not supported. Certain flags require the plugin to be de-activated."**

### `clap.audio-ports-config` + `clap.audio-ports-config-info/1` (`ext/audio-ports-config.h`) — STABLE

**host → plugin — `clap_plugin_audio_ports_config`**

| Field | Signature | Thread |
|---|---|---|
| `count` | `uint32_t(CLAP_ABI *count)(const clap_plugin_t *plugin)` | `[main-thread]` |
| `get` | `bool(CLAP_ABI *get)(const clap_plugin_t *plugin, uint32_t index, clap_audio_ports_config_t *config)` | `[main-thread]` |
| `select` | `bool(CLAP_ABI *select)(const clap_plugin_t *plugin, clap_id config_id)` | **`[main-thread & plugin-deactivated]`** |

**host → plugin — `clap_plugin_audio_ports_config_info`** (ext id `clap.audio-ports-config-info/1`)

| Field | Signature | Thread |
|---|---|---|
| `current_config` | `clap_id(CLAP_ABI *current_config)(const clap_plugin_t *plugin)` | `[main-thread]` |
| `get` | `bool(CLAP_ABI *get)(const clap_plugin_t *plugin, clap_id config_id, uint32_t port_index, bool is_input, clap_audio_port_info_t *info)` | `[main-thread]` |

**plugin → host — `clap_host_audio_ports_config`**: `rescan` — `void(CLAP_ABI *rescan)(const
clap_host_t *host)` — `[main-thread]`.

`clap_audio_ports_config` {`id`, `name`, `input_port_count`, `output_port_count`, `has_main_input`,
`main_input_channel_count`, `main_input_port_type`, `has_main_output`,
`main_output_channel_count`, `main_output_port_type`}.

> **"The host can only select a configuration if the plugin is deactivated."**
> "Once applied the host should scan again the audio ports."
> "The audio ports config scan has to be done while the plugin is deactivated."
> "…returns CLAP_INVALID_ID if the current port layout isn't part of the config list."

### `clap.audio-ports-activation/2` (`ext/audio-ports-activation.h`) — STABLE

| Field | Signature | Thread |
|---|---|---|
| `can_activate_while_processing` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin)` | `[main-thread]` |
| `set_active` | `bool(CLAP_ABI *set_active)(const clap_plugin_t *plugin, bool is_input, uint32_t port_index, bool is_active, uint32_t sample_size)` | **`[active ? audio-thread : main-thread]`** |

No host struct. `sample_size` is 32, 64, or 0 if unspecified.

> "Audio ports can only be activated or deactivated when the plugin is deactivated, unless can_activate_while_processing() returns true."
> **"Audio buffers must still be provided if the audio port is deactivated. In such case, they shall be filled with 0 (or whatever is the neutral value in your context) and the constant_mask shall be set."**
> "Audio ports are initially in the active state after creating the plugin instance."
> **"Audio ports state are not saved in the plugin state, so the host must restore the audio ports state after creating the plugin instance."**
> "Audio ports state is invalidated by clap_plugin_audio_ports_config.select() and clap_host_audio_ports.rescan(CLAP_AUDIO_PORTS_RESCAN_LIST)."

### `clap.configurable-audio-ports/1` (`ext/configurable-audio-ports.h`) — STABLE

> "This extension lets the host configure the plugin's input and output audio ports. This is a 'push' approach to audio ports configuration."

| Field | Signature | Thread |
|---|---|---|
| `can_apply_configuration` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin, const struct clap_audio_port_configuration_request *requests, uint32_t request_count)` | **`[main-thread & !active]`** |
| `apply_configuration` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin, const struct clap_audio_port_configuration_request *requests, uint32_t request_count)` | **`[main-thread & !active]`** |

No host struct. `clap_audio_port_configuration_request` {`is_input`, `port_index`, `channel_count`,
`port_type`, `port_details`}.

> "Submit a bunch of configuration requests which will atomically be applied together, or discarded together."
> "Once the configuration is successfully applied, it isn't necessary for the plugin to call clap_host_audio_ports->changed(); and it isn't necessary for the host to scan the audio ports."
> `port_details` cast: "CLAP_PORT_MONO: (discard) - CLAP_PORT_STEREO: (discard) - CLAP_PORT_SURROUND: const uint8_t *channel_map - CLAP_PORT_AMBISONIC: const clap_ambisonic_config_t *info"

### `clap.note-ports` (`ext/note-ports.h`) — STABLE

**host → plugin**: `count` `uint32_t(CLAP_ABI *count)(const clap_plugin_t *plugin, bool is_input)`
— `[main-thread]`; `get` `bool(CLAP_ABI *get)(const clap_plugin_t *plugin, uint32_t index, bool
is_input, clap_note_port_info_t *info)` — `[main-thread]`.

**plugin → host**: `supported_dialects` `uint32_t(CLAP_ABI *)(const clap_host_t *host)` —
`[main-thread]`; `rescan` `void(CLAP_ABI *rescan)(const clap_host_t *host, uint32_t flags)` —
`[main-thread]`.

`enum clap_note_dialect`: `CLAP` `1<<0` ("Uses clap_event_note and clap_event_note_expression"),
`MIDI` `1<<1` ("no polyphonic expression"), `MIDI_MPE` `1<<2` ("with polyphonic expression (MPE)"),
`MIDI2` `1<<3`.
Rescan flags: `CLAP_NOTE_PORTS_RESCAN_ALL` `1<<0`, `CLAP_NOTE_PORTS_RESCAN_NAMES` `1<<1`.

`clap_note_port_info` {`id`, `supported_dialects`, `preferred_dialect`, `name`}.

> "If the plugin does not implement this extension, it won't have note input or output."
> **"The plugin is only allowed to change its note ports configuration while it is deactivated."**
> "The note ports scan has to be done while the plugin is deactivated."
> "This flag can only be used if the plugin is not active. If the plugin active, call host->request_restart() and then call rescan() when the host calls deactivate()" (RESCAN_ALL)

---

## A.3 `clap.params` (`ext/params.h`) — STABLE

The single densest extension for a host. **host → plugin — `clap_plugin_params`:**

| Field | Signature | Thread |
|---|---|---|
| `count` | `uint32_t(CLAP_ABI *count)(const clap_plugin_t *plugin)` | `[main-thread]` |
| `get_info` | `bool(CLAP_ABI *get_info)(const clap_plugin_t *plugin, uint32_t param_index, clap_param_info_t *param_info)` | `[main-thread]` |
| `get_value` | `bool(CLAP_ABI *get_value)(const clap_plugin_t *plugin, clap_id param_id, double *out_value)` | `[main-thread]` |
| `value_to_text` | `bool(CLAP_ABI *value_to_text)(const clap_plugin_t *plugin, clap_id param_id, double value, char *out_buffer, uint32_t out_buffer_capacity)` | `[main-thread]` |
| `text_to_value` | `bool(CLAP_ABI *text_to_value)(const clap_plugin_t *plugin, clap_id param_id, const char *param_value_text, double *out_value)` | `[main-thread]` |
| `flush` | `void(CLAP_ABI *flush)(const clap_plugin_t *plugin, const clap_input_events_t *in, const clap_output_events_t *out)` | **`[active ? audio-thread : main-thread]`** |

**plugin → host — `clap_host_params`** (we implement):

| Field | Signature | Thread |
|---|---|---|
| `rescan` | `void(CLAP_ABI *rescan)(const clap_host_t *host, clap_param_rescan_flags flags)` | `[main-thread]` |
| `clear` | `void(CLAP_ABI *clear)(const clap_host_t *host, clap_id param_id, clap_param_clear_flags flags)` | `[main-thread]` |
| `request_flush` | `void(CLAP_ABI *request_flush)(const clap_host_t *host)` | **`[thread-safe,!audio-thread]`** |

> **"This method must not be called concurrently to clap_plugin->process()."** (flush)
> "Note: if the plugin is processing, then the process() call will already achieve the parameter update (bi-directional), so a call to flush isn't required, also be aware that the plugin may use the sample offset in process(), while this information would be lost within flush()."
> "This function is always safe to use and should not be called from an [audio-thread] as the plugin would already be within process() or flush()." (request_flush)
> "The host will then schedule a call to either: - clap_plugin.process() - clap_plugin_params.flush()."

### `clap_param_info_flags` (all 17)

| Flag | Value | Meaning (verbatim, abridged where long) |
|---|---|---|
| `CLAP_PARAM_IS_STEPPED` | `1<<0` | "integer values only … converted to integer using a cast (equivalent to trunc)" |
| `CLAP_PARAM_IS_PERIODIC` | `1<<1` | "Useful for periodic parameters like a phase" |
| `CLAP_PARAM_IS_HIDDEN` | `1<<2` | "should not be shown to the user, because it is currently not used" |
| `CLAP_PARAM_IS_READONLY` | `1<<3` | "The parameter can't be changed by the host." |
| `CLAP_PARAM_IS_BYPASS` | `1<<4` | "merge the plugin and host bypass button… Only zero or one bypass parameter is allowed per plugin. min: 0 -> bypass off; max: 1 -> bypass on." |
| `CLAP_PARAM_IS_AUTOMATABLE` | `1<<5` | "automation can be recorded / played back" |
| `..._PER_NOTE_ID` | `1<<6` | per note automations |
| `..._PER_KEY` | `1<<7` | per key automations |
| `..._PER_CHANNEL` | `1<<8` | per channel automations |
| `..._PER_PORT` | `1<<9` | per port automations |
| `CLAP_PARAM_IS_MODULATABLE` | `1<<10` | "support the modulation signal?" |
| `..._PER_NOTE_ID` | `1<<11` | per note modulations |
| `..._PER_KEY` | `1<<12` | per key modulations |
| `..._PER_CHANNEL` | `1<<13` | per channel modulations |
| `..._PER_PORT` | `1<<14` | per port modulations |
| `CLAP_PARAM_REQUIRES_PROCESS` | `1<<15` | "requires to be done via process() if the plugin is active" |
| `CLAP_PARAM_IS_ENUM` | `1<<16` | "you must set CLAP_PARAM_IS_STEPPED too. All values from min to max must not have a blank value_to_text()." |

Bypass caveat: "The value of this parameter should not influence whether the host calls
plugin->process() or not."

### `clap_param_rescan_flags`

| Flag | Value | Meaning |
|---|---|---|
| `CLAP_PARAM_RESCAN_VALUES` | `1<<0` | "The parameter values did change, eg. after loading a preset… The host will not record those changes as automation points. New values takes effect immediately." |
| `CLAP_PARAM_RESCAN_TEXT` | `1<<1` | "The value to text conversion changed" |
| `CLAP_PARAM_RESCAN_INFO` | `1<<2` | "name change - module change - is_periodic (flag) - is_hidden (flag). New info takes effect immediately." |
| `CLAP_PARAM_RESCAN_ALL` | `1<<3` | **"Invalidates everything the host knows about parameters. It can only be used while the plugin is deactivated."** |

`CLAP_PARAM_RESCAN_ALL` full contract, verbatim:
> "If the plugin is activated use clap_host->restart() and delay any change until the host calls clap_plugin->deactivate(). You must use this flag if: - some parameters were added or removed. - some parameters had critical changes: - is_per_note (flag) - is_per_key (flag) - is_per_channel (flag) - is_per_port (flag) - is_readonly (flag) - is_bypass (flag) - is_stepped (flag) - is_modulatable (flag) - min_value - max_value - cookie"

### `clap_param_clear_flags`

`CLAP_PARAM_CLEAR_ALL` `1<<0`, `CLAP_PARAM_CLEAR_AUTOMATIONS` `1<<1`,
`CLAP_PARAM_CLEAR_MODULATIONS` `1<<2`.

### `clap_param_info`

| Field | Type | Doc |
|---|---|---|
| `id` | `clap_id` | **"Stable parameter identifier, it must never change."** |
| `flags` | `clap_param_info_flags` | |
| `cookie` | `void *` | fast-access pointer cache, see below |
| `name` | `char[CLAP_NAME_SIZE]` | "The display name. eg: 'Volume'. This does not need to be unique. Do not include the module text in this." |
| `module` | `char[CLAP_PATH_SIZE]` | "eg: 'Oscillators/Wavetable 1'. '/' will be used as a separator to show a tree-like structure." |
| `min_value` | `double` | "Minimum plain value. Must be finite (`std::isfinite` true)" |
| `max_value` | `double` | "Maximum plain value. Must be finite" |
| `default_value` | `double` | "Default plain value. Must be in [min, max] range." |

**Cookie contract — direct host obligations, verbatim:**
> "- The cookie is invalidated by a call to clap_host_params->rescan(CLAP_PARAM_RESCAN_ALL) or when the plugin is destroyed."
> **"- The host will either provide the cookie as issued or nullptr in events addressing parameters."**
> "- The plugin must gracefully handle the case of a cookie which is nullptr."
> "- Many plugins will process the parameter events more quickly if the host can provide the cookie in a faster time than a hashmap lookup per param per event."

### The value-synchronization contract, verbatim

> "The host sees the plugin as an atomic entity; and acts as a controller on top of its parameters."
> "The plugin is responsible for keeping its audio processor and its GUI in sync."
> "The host can at any time read parameters' value on the [main-thread] using @ref clap_plugin_params.get_value()."
> **"There are two options to communicate parameter value changes, and they are not concurrent."** — "send automation points during clap_plugin.process()" / "send automation points during clap_plugin_params.flush(), for parameter changes without processing audio"
> **"When the plugin changes a parameter value, it must inform the host."** "It will send @ref CLAP_EVENT_PARAM_VALUE event during process() or flush()."
> "If the user is adjusting the value, don't forget to mark the beginning and end of the gesture by sending CLAP_EVENT_PARAM_GESTURE_BEGIN and CLAP_EVENT_PARAM_GESTURE_END events."

MIDI CC conflict:
> "@note MIDI CCs are tricky because you may not know when the parameter adjustment ends. Also if the host records incoming MIDI CC and parameter change automation at the same time, there will be a conflict at playback: MIDI CC vs Automation. The parameter automation will always target the same parameter because the param_id is stable. The MIDI CC may have a different mapping in the future and may result in a different playback."
> "When a MIDI CC changes a parameter's value, set the flag CLAP_EVENT_DONT_RECORD in clap_event_param.header.flags."

**Advice for the host (verbatim) — this is the design guidance for our automation model:**
> "- store plain values in the document (automation)"
> "- store modulation amount in plain value delta, not in percentage"
> "- when you apply a CC mapping, remember the min/max plain values so you can adjust"
> **"- do not implement a parameter saving fall back for plugins that don't implement the state extension"**

> "Plugins are responsible for persisting their parameter's values between sessions by implementing the state extension. Otherwise parameter value will not be recalled when reloading a project. Hosts should _not_ try to save and restore parameter values for plugins that don't implement the state extension."

Range-change risk:
> "There are two approaches to automations, either you automate the plain value, or you automate the knob position. The first option will be robust to a range increase, while the second won't be. Though, stepped parameters should be stored as plain value in the document."

---

## A.4 State

### `clap.state` (`ext/state.h`) — STABLE

**host → plugin**: `save` `bool(CLAP_ABI *save)(const clap_plugin_t *plugin, const clap_ostream_t
*stream)` — `[main-thread]`; `load` `bool(CLAP_ABI *load)(const clap_plugin_t *plugin, const
clap_istream_t *stream)` — `[main-thread]`.

**plugin → host**: `mark_dirty` `void(CLAP_ABI *mark_dirty)(const clap_host_t *host)` —
`[main-thread]`.

> "Plugins can implement this extension to save and restore both parameter values and non-parameter state. This is used to persist a plugin's state between project reloads, when duplicating and copying plugin instances, and for host-side preset management."
> "If a parameter value changes, then it is implicit that the state is dirty."

### `clap.state-context/2` (`ext/state-context.h`) — STABLE

**host → plugin**: `save` `bool(CLAP_ABI *save)(const clap_plugin_t *plugin, const clap_ostream_t
*stream, uint32_t context_type)` — `[main-thread]`; `load` `bool(CLAP_ABI *load)(const clap_plugin_t
*plugin, const clap_istream_t *stream, uint32_t context_type)` — `[main-thread]`. No host struct.

`enum clap_plugin_state_context_type`: `CLAP_STATE_CONTEXT_FOR_PRESET` = 1,
`..._FOR_DUPLICATE` = 2, `..._FOR_PROJECT` = 3.

> **"If the plugin implements CLAP_EXT_STATE_CONTEXT then it is mandatory to also implement CLAP_EXT_STATE."**
> "All three operations should be equivalent: 1. clap_plugin_state_context.load(clap_plugin_state.save(), CLAP_STATE_CONTEXT_FOR_PRESET) 2. clap_plugin_state.load(clap_plugin_state_context.save(CLAP_STATE_CONTEXT_FOR_PRESET)) 3. clap_plugin_state_context.load(clap_plugin_state_context.save(CLAP_STATE_CONTEXT_FOR_PRESET), CLAP_STATE_CONTEXT_FOR_PRESET)"
> "It is unspecified which context is equivalent to clap_plugin_state.{save,load}()"
> "If in doubt, fallback to clap_plugin_state."

---

## A.5 Latency / tail / render / voice-info

### `clap.latency` (`ext/latency.h`) — STABLE

| Direction | Field | Signature | Thread |
|---|---|---|---|
| host → plugin | `get` | `uint32_t(CLAP_ABI *get)(const clap_plugin_t *plugin)` | **`[main-thread & (being-activated \| active)]`** |
| plugin → host | `changed` | `void(CLAP_ABI *changed)(const clap_host_t *host)` | **`[main-thread & being-activated]`** |

> **"The latency is only allowed to change during plugin->activate. If the plugin is activated, call host->request_restart()"**

### `clap.tail` (`ext/tail.h`) — STABLE

| Direction | Field | Signature | Thread |
|---|---|---|---|
| host → plugin | `get` | `uint32_t(CLAP_ABI *get)(const clap_plugin_t *plugin)` | **`[main-thread,audio-thread]`** |
| plugin → host | `changed` | `void(CLAP_ABI *changed)(const clap_host_t *host)` | **`[audio-thread]`** |

> "Returns tail length in samples. **Any value greater or equal to INT32_MAX implies infinite tail.**"

### `clap.render` (`ext/render.h`) — STABLE. No host struct.

| Field | Signature | Thread |
|---|---|---|
| `has_hard_realtime_requirement` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin)` | `[main-thread]` |
| `set` | `bool(CLAP_ABI *set)(const clap_plugin_t *plugin, clap_plugin_render_mode mode)` | `[main-thread]` |

`typedef int32_t clap_plugin_render_mode`: `CLAP_RENDER_REALTIME` = 0 ("Default setting, for
'realtime' processing"), `CLAP_RENDER_OFFLINE` = 1 ("For processing without realtime pressure. The
plugin may use more expensive algorithms for higher sound quality.").

> "Returns true if the plugin has a hard requirement to process in real-time. This is especially useful for plugin acting as a proxy to an hardware device."
> "If this information does not influence your rendering code, then don't implement this extension."

### `clap.voice-info` (`ext/voice-info.h`) — STABLE

| Direction | Field | Signature | Thread |
|---|---|---|---|
| host → plugin | `get` | `bool(CLAP_ABI *get)(const clap_plugin_t *plugin, clap_voice_info_t *info)` | **`[main-thread & active]`** |
| plugin → host | `changed` | `void(CLAP_ABI *changed)(const clap_host_t *host)` | `[main-thread]` |

`CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES` `1<<0` — "Allows the host to send overlapping NOTE_ON
events. The plugin will then rely upon the note_id to distinguish between them."

`clap_voice_info` {`voice_count`, `voice_capacity`, `flags`}.

> **"1 <= voice_count <= voice_capacity"**
> "voice_count should not be confused with the number of active voices."
> "If the voice_count is 1, then the synth is working in mono and the host can decide to only use global modulation mapping."

---

## A.6 GUI (`ext/gui.h`) — STABLE

> "This extension defines how the plugin will present its GUI."

**The full lifecycle sequence, verbatim:**
> "Showing the GUI works as follow:
>  1. clap_plugin_gui->is_api_supported(), check what can work
>  2. clap_plugin_gui->create(), allocates gui resources
>  3. if the plugin window is floating
>  4.    -> clap_plugin_gui->set_transient()
>  5.    -> clap_plugin_gui->suggest_title()
>  6. else
>  7.    -> clap_plugin_gui->set_scale()
>  8.    -> clap_plugin_gui->can_resize()
>  9.    -> if resizable and has known size from previous session, clap_plugin_gui->set_size()
> 10.    -> else clap_plugin_gui->get_size(), gets initial size
> 11.    -> clap_plugin_gui->set_parent()
> 12. clap_plugin_gui->show()
> 13. clap_plugin_gui->hide()/show() ...
> 14. clap_plugin_gui->destroy() when done with the gui"

> "Resizing the window (initiated by the plugin, if embedded): 1. Plugins calls clap_host_gui->request_resize() 2. If the host returns true the new size is accepted, the host doesn't have to call clap_plugin_gui->set_size(). If the host returns false, the new size is rejected."
> "Resizing the window (drag, if embedded)): 1. Only possible if clap_plugin_gui->can_resize() returns true 2. Mouse drag -> new_size 3. clap_plugin_gui->adjust_size(new_size) -> working_size 4. clap_plugin_gui->set_size(working_size)"
> "The Embedding protocol is by far the most common, supported by all hosts to date, and a plugin author should support at least that case."

**host → plugin — `clap_plugin_gui` (15 functions)**

| Field | Signature | Thread |
|---|---|---|
| `is_api_supported` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin, const char *api, bool is_floating)` | `[main-thread]` |
| `get_preferred_api` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin, const char **api, bool *is_floating)` | `[main-thread]` |
| `create` | `bool(CLAP_ABI *create)(const clap_plugin_t *plugin, const char *api, bool is_floating)` | `[main-thread]` |
| `destroy` | `void(CLAP_ABI *destroy)(const clap_plugin_t *plugin)` | `[main-thread]` |
| `set_scale` | `bool(CLAP_ABI *set_scale)(const clap_plugin_t *plugin, double scale)` | `[main-thread]` |
| `get_size` | `bool(CLAP_ABI *get_size)(const clap_plugin_t *plugin, uint32_t *width, uint32_t *height)` | `[main-thread]` |
| `can_resize` | `bool(CLAP_ABI *can_resize)(const clap_plugin_t *plugin)` | **`[main-thread & !floating]`** |
| `get_resize_hints` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin, clap_gui_resize_hints_t *hints)` | **`[main-thread & !floating]`** |
| `adjust_size` | `bool(CLAP_ABI *adjust_size)(const clap_plugin_t *plugin, uint32_t *width, uint32_t *height)` | **`[main-thread & !floating]`** |
| `set_size` | `bool(CLAP_ABI *set_size)(const clap_plugin_t *plugin, uint32_t width, uint32_t height)` | **`[main-thread & !floating]`** |
| `set_parent` | `bool(CLAP_ABI *set_parent)(const clap_plugin_t *plugin, const clap_window_t *window)` | **`[main-thread & !floating]`** |
| `set_transient` | `bool(CLAP_ABI *set_transient)(const clap_plugin_t *plugin, const clap_window_t *window)` | **`[main-thread & floating]`** |
| `suggest_title` | `void(CLAP_ABI *suggest_title)(const clap_plugin_t *plugin, const char *title)` | **`[main-thread & floating]`** |
| `show` | `bool(CLAP_ABI *show)(const clap_plugin_t *plugin)` | `[main-thread]` |
| `hide` | `bool(CLAP_ABI *hide)(const clap_plugin_t *plugin)` | `[main-thread]` |

**plugin → host — `clap_host_gui` (we implement). Note these are `[thread-safe]`, not
`[main-thread]` — gui is the only stable extension where that is true.**

| Field | Signature | Thread |
|---|---|---|
| `resize_hints_changed` | `void(CLAP_ABI *)(const clap_host_t *host)` | **`[thread-safe & !floating]`** |
| `request_resize` | `bool(CLAP_ABI *request_resize)(const clap_host_t *host, uint32_t width, uint32_t height)` | **`[thread-safe & !floating]`** |
| `request_show` | `bool(CLAP_ABI *request_show)(const clap_host_t *host)` | `[thread-safe]` |
| `request_hide` | `bool(CLAP_ABI *request_hide)(const clap_host_t *host)` | `[thread-safe]` |
| `closed` | `void(CLAP_ABI *closed)(const clap_host_t *host, bool was_destroyed)` | `[thread-safe]` |

Window API constants:

| Constant | Value | Note (verbatim) |
|---|---|---|
| `CLAP_WINDOW_API_WIN32` | `"win32"` | "uses physical size"; embed via `SetParent` |
| `CLAP_WINDOW_API_COCOA` | `"cocoa"` | "uses logical size, don't call clap_plugin_gui->set_scale()" |
| `CLAP_WINDOW_API_UIKIT` | `"uikit"` | "uses logical size, don't call clap_plugin_gui->set_scale()" |
| `CLAP_WINDOW_API_X11` | `"x11"` | "uses physical size"; embed via XEmbed |
| `CLAP_WINDOW_API_WAYLAND` | `"wayland"` | "embed is currently not supported, use floating windows" |

`clap_window` {`api`, union{`cocoa` (`void*`), `uikit` (`void*`), `x11` (`unsigned long`), `win32`
(`void*`), `ptr` (`void*`)}}. The union is **anonymous**.

`clap_gui_resize_hints` {`can_resize_horizontally`, `can_resize_vertically`,
`preserve_aspect_ratio`, `aspect_ratio_width`, `aspect_ratio_height`}.

**Host obligations, verbatim:**
> **"If was_destroyed is true, then the host must call clap_plugin_gui->destroy() to acknowledge the gui destruction."**
> "clap_plugin_gui->create() must have been called prior to asking the size."
> "Note: if not called from the main thread, then a return value simply means that the host acknowledged the request and will process it asynchronously. If the request then can't be satisfied then the host will call set_size() to revert the operation." (request_resize)
> "Size (width, height) is in pixels; the corresponding windowing system extension is responsible for defining if it is physical pixels or logical pixels."
> "After this call, the GUI may not be visible yet; don't forget to call show()."
> "Hide the window, this method does not free the resources, it just hides the window content."
> "The const char **api variable should be explicitly assigned as a pointer to one of the CLAP_WINDOW_API_ constants defined above, not strcopied."

---

## A.7 Host services the plugin calls into

### `clap.log` (`ext/log.h`) — STABLE. **Host-only — no plugin struct.**

`log` — `void(CLAP_ABI *log)(const clap_host_t *host, clap_log_severity severity, const char *msg)`
— **`[thread-safe]`**.

`typedef int32_t clap_log_severity`: `CLAP_LOG_DEBUG` 0, `INFO` 1, `WARNING` 2, `ERROR` 3, `FATAL` 4,
`CLAP_LOG_HOST_MISBEHAVING` 5, `CLAP_LOG_PLUGIN_MISBEHAVING` 6.

> "These severities should be used to report misbehaviour. The plugin one can be used by a layer between the plugin and the host."

### `clap.thread-check` (`ext/thread-check.h`) — STABLE. **Host-only.**

`is_main_thread` — `bool(CLAP_ABI *)(const clap_host_t *host)` — **`[thread-safe]`**
`is_audio_thread` — `bool(CLAP_ABI *)(const clap_host_t *host)` — **`[thread-safe]`**

> "It is highly recommended that hosts implement this extension." (full model quoted in Part 0)

### `clap.event-registry` (`ext/event-registry.h`) — STABLE. **Host-only.**

`query` — `bool(CLAP_ABI *query)(const clap_host_t *host, const char *space_name, uint16_t
*space_id)` — `[main-thread]`.

> "The space id 0 is reserved for CLAP's core events. See CLAP_CORE_EVENT_SPACE."
> **"Return false and sets *space_id to UINT16_MAX if the space name is unknown to the host."**

*(The comment says `CLAP_CORE_EVENT_SPACE`; the actual constant in `events.h` is
`CLAP_CORE_EVENT_SPACE_ID`. Stale cross-reference in the header.)*

### `clap.timer-support` (`ext/timer-support.h`) — STABLE

host → plugin: `on_timer` — `void(CLAP_ABI *on_timer)(const clap_plugin_t *plugin, clap_id
timer_id)` — `[main-thread]`.
plugin → host: `register_timer` — `bool(CLAP_ABI *)(const clap_host_t *host, uint32_t period_ms,
clap_id *timer_id)` — `[main-thread]`; `unregister_timer` — `bool(CLAP_ABI *)(const clap_host_t
*host, clap_id timer_id)` — `[main-thread]`.

> "The host may adjust the period if it is under a certain threshold. **30 Hz should be allowed.**"

### `clap.posix-fd-support` (`ext/posix-fd-support.h`) — STABLE

> "This extension let your plugin hook itself into the host select/poll/epoll/kqueue reactor. This is useful to handle asynchronous I/O on the main thread."

host → plugin: `on_fd` — `void(CLAP_ABI *on_fd)(const clap_plugin_t *plugin, int fd,
clap_posix_fd_flags_t flags)` — `[main-thread]`.
plugin → host: `register_fd` / `modify_fd` — `bool(CLAP_ABI *)(const clap_host_t *host, int fd,
clap_posix_fd_flags_t flags)`; `unregister_fd` — `bool(CLAP_ABI *)(const clap_host_t *host, int fd)`
— all three `[main-thread]`.

`typedef uint32_t clap_posix_fd_flags_t`: `CLAP_POSIX_FD_READ` `1<<0`, `CLAP_POSIX_FD_WRITE` `1<<1`,
`CLAP_POSIX_FD_ERROR` `1<<2`.

> **"This callback is 'level-triggered'. It means that a writable fd will continuously produce 'on_fd()' events; don't forget using modify_fd() to remove the write notification once you're done writing."**

### `clap.thread-pool` (`ext/thread-pool.h`) — STABLE

**The one genuinely important unannotated function in the stable set.**

| Direction | Field | Signature | Thread |
|---|---|---|---|
| host → plugin | `exec` | `void(CLAP_ABI *exec)(const clap_plugin_t *plugin, uint32_t task_index)` | **unannotated** — the header says only `// Called by the thread pool` |
| plugin → host | `request_exec` | `bool(CLAP_ABI *request_exec)(const clap_host_t *host, uint32_t num_tasks)` | `[audio-thread]` |

Verified directly: `clap_plugin_thread_pool::exec` carries **no** bracketed tag. Its realtime nature
is implied only by `request_exec`'s prose and the header's usage example, never stated as an
annotation.

> **"The plugin must provide @ref clap_plugin_thread_pool, and the host may provide @ref clap_host_thread_pool."**
> "Schedule num_tasks jobs in the host thread pool. **It can't be called concurrently or from the thread pool. Will block until all the tasks are processed. This must be used exclusively for realtime processing within the process call.**"
> **"The host should check that the plugin is within the process call, and if not, reject the exec request."**
> "Be aware that using a thread pool may break hard real-time rules due to the thread synchronization involved. If the host knows that it is running under hard real-time pressure it may decide to not provide this interface."
> "If it doesn't, the plugin should process its data by its own means. In the worst case, a single threaded for-loop."

---

## A.8 Presentation / metadata extensions

### `clap.note-name` (`ext/note-name.h`) — STABLE

host → plugin: `count` — `uint32_t(CLAP_ABI *count)(const clap_plugin_t *plugin)` — `[main-thread]`;
`get` — `bool(CLAP_ABI *get)(const clap_plugin_t *plugin, uint32_t index, clap_note_name_t
*note_name)` — `[main-thread]`.
plugin → host: `changed` — `void(CLAP_ABI *changed)(const clap_host_t *host)` — `[main-thread]`.

`clap_note_name` {`name[CLAP_NAME_SIZE]`, `port` ("-1 for every port"), `key` ("-1 for every key"),
`channel` ("-1 for every channel")} — all `int16_t`.

### `clap.track-info/1` (`ext/track-info.h`) — STABLE

host → plugin: `changed` — `void(CLAP_ABI *changed)(const clap_plugin_t *plugin)` — `[main-thread]`.
plugin → host: `get` — `bool(CLAP_ABI *get)(const clap_host_t *host, clap_track_info_t *info)` —
`[main-thread]`.

Flags: `HAS_TRACK_NAME` `1<<0`, `HAS_TRACK_COLOR` `1<<1`, `HAS_AUDIO_CHANNEL` `1<<2`,
`IS_FOR_RETURN_TRACK` `1<<3` ("initialize with wet 100%"), `IS_FOR_BUS` `1<<4`, `IS_FOR_MASTER`
`1<<5`.

`clap_track_info` {`flags` (`uint64_t`), `name`, `color` (`clap_color_t`), `audio_channel_count`
(`int32_t`), `audio_port_type`}.

> "It is useful when the plugin is created, to initialize some parameters (mix, dry, wet) and pick a suitable configuration regarding audio port type and channel count."

### `clap.remote-controls/2` (`ext/remote-controls.h`) — STABLE

host → plugin: `count` — `uint32_t(CLAP_ABI *count)(const clap_plugin_t *plugin)` — `[main-thread]`;
`get` — `bool(CLAP_ABI *get)(const clap_plugin_t *plugin, uint32_t page_index,
clap_remote_controls_page_t *page)` — `[main-thread]`.
plugin → host: `changed` — `void(CLAP_ABI *)(const clap_host_t *host)` — `[main-thread]`;
`suggest_page` — `void(CLAP_ABI *suggest_page)(const clap_host_t *host, clap_id page_id)` —
`[main-thread]`.

`CLAP_REMOTE_CONTROLS_COUNT = 8`. `clap_remote_controls_page` {`section_name`, `page_id`,
`page_name`, `param_ids[8]`, `is_for_preset`}.

> "A page contains up to 8 controls, which references parameters using param_id."
> "This is used to separate device pages versus preset pages. If true, then this page is specific to this preset."
> "Suggest a page to the host because it corresponds to what the user is currently editing in the plugin's GUI."

### `clap.param-indication/4` (`ext/param-indication.h`) — STABLE. No host struct.

| Field | Signature | Thread |
|---|---|---|
| `set_mapping` | `void(CLAP_ABI *set_mapping)(const clap_plugin_t *plugin, clap_id param_id, bool has_mapping, const clap_color_t *color, const char *label, const char *description)` | `[main-thread]` |
| `set_automation` | `void(CLAP_ABI *set_automation)(const clap_plugin_t *plugin, clap_id param_id, uint32_t automation_state, const clap_color_t *color)` | `[main-thread]` |

Automation states: `CLAP_PARAM_INDICATION_AUTOMATION_NONE` 0, `PRESENT` 1, `PLAYING` 2, `RECORDING`
3, `OVERRIDING` 4 ("the host should play an automation for this parameter, but the user has started
to adjust this parameter and is overriding the automation playback").

> **"Parameter indications should not be saved in the plugin context, and are off by default."** (stated on both functions)
> "The color semantic depends upon the host here and the goal is to have a consistent experience across all plugins."

### `clap.preset-load/2` (`ext/preset-load.h`) — STABLE

host → plugin: `from_location` — `bool(CLAP_ABI *from_location)(const clap_plugin_t *plugin,
uint32_t location_kind, const char *location, const char *load_key)` — `[main-thread]`.
plugin → host: `on_error` — `void(CLAP_ABI *on_error)(const clap_host_t *host, uint32_t
location_kind, const char *location, const char *load_key, int32_t os_error, const char *msg)` —
`[main-thread]`; `loaded` — `void(CLAP_ABI *loaded)(const clap_host_t *host, uint32_t location_kind,
const char *location, const char *load_key)` — `[main-thread]`.

> **"If the preset was loaded from a container file, then the load_key must be set, otherwise it must be null."**
> "This contributes to keep in sync the host preset browser and plugin preset browser."
> "os_error: the operating system error, if applicable. If not applicable set it to a non-error value, eg: 0 on unix and Windows."

### `clap.context-menu/1` (`ext/context-menu.h`) — STABLE

**host → plugin — `clap_plugin_context_menu`**

| Field | Signature | Thread |
|---|---|---|
| `populate` | `bool(CLAP_ABI *populate)(const clap_plugin_t *plugin, const clap_context_menu_target_t *target, const clap_context_menu_builder_t *builder)` | `[main-thread]` |
| `perform` | `bool(CLAP_ABI *perform)(const clap_plugin_t *plugin, const clap_context_menu_target_t *target, clap_id action_id)` | `[main-thread]` |

**plugin → host — `clap_host_context_menu`** (we implement)

| Field | Signature | Thread |
|---|---|---|
| `populate` | `bool(CLAP_ABI *populate)(const clap_host_t *host, const clap_context_menu_target_t *target, const clap_context_menu_builder_t *builder)` | `[main-thread]` |
| `perform` | `bool(CLAP_ABI *perform)(const clap_host_t *host, const clap_context_menu_target_t *target, clap_id action_id)` | `[main-thread]` |
| `can_popup` | `bool(CLAP_ABI *can_popup)(const clap_host_t *host)` | `[main-thread]` |
| `popup` | `bool(CLAP_ABI *popup)(const clap_host_t *host, const clap_context_menu_target_t *target, int32_t screen_index, int32_t x, int32_t y)` | `[main-thread]` |

**`clap_context_menu_builder` — both methods unannotated:**

| Field | Signature | Thread |
|---|---|---|
| `ctx` | `void *` | data |
| `add_item` | `bool(CLAP_ABI *add_item)(const struct clap_context_menu_builder *builder, clap_context_menu_item_kind_t item_kind, const void *item_data)` | **unannotated** |
| `supports` | `bool(CLAP_ABI *supports)(const struct clap_context_menu_builder *builder, clap_context_menu_item_kind_t item_kind)` | **unannotated** |

> **"This object isn't thread-safe and must be used on the same thread as it was provided."**

Target kinds: `CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL` 0, `..._PARAM` 1.
`clap_context_menu_target` {`kind`, `id`}.

Item kinds (`clap_context_menu_item_kind_t` = `uint32_t`, implicit values 0–5):

| Kind | Value | `item_data` |
|---|---|---|
| `CLAP_CONTEXT_MENU_ITEM_ENTRY` | 0 | `clap_context_menu_entry_t*` |
| `CLAP_CONTEXT_MENU_ITEM_CHECK_ENTRY` | 1 | `clap_context_menu_check_entry_t*` |
| `CLAP_CONTEXT_MENU_ITEM_SEPARATOR` | 2 | `NULL` |
| `CLAP_CONTEXT_MENU_ITEM_BEGIN_SUBMENU` | 3 | `clap_context_menu_submenu_t*` |
| `CLAP_CONTEXT_MENU_ITEM_END_SUBMENU` | 4 | `NULL` |
| `CLAP_CONTEXT_MENU_ITEM_TITLE` | 5 | `clap_context_menu_item_title_t*` |

*(Naming discrepancy in the header: doc comments say `clap_context_menu_item_entry_t` /
`_item_check_entry_t` / `_item_begin_submenu_t`, but the declared structs are
`clap_context_menu_entry_t` / `_check_entry_t` / `_submenu_t`.)*

Payloads: `entry` {`label`, `is_enabled`, `action_id`}; `check_entry` {`label`, `is_enabled`,
`is_checked`, `action_id`}; `title` {`title`, `is_enabled`}; `submenu` {`label`, `is_enabled`}.

> "If target is null, assume global context." (×5)
> "Performs the given action, which was previously provided to the host via populate()."
> **"This may depend upon the current windowing system used to display the plugin, so the return value is invalidated after creating the plugin window."** (can_popup)
> "If the plugin is using embedded GUI, then x and y are relative to the plugin's window, otherwise they're absolute coordinate, and screen index might be set accordingly."

### `clap.surround/4` (`ext/surround.h`) — STABLE

host → plugin: `is_channel_mask_supported` — `bool(CLAP_ABI *)(const clap_plugin_t *plugin, uint64_t
channel_mask)` — `[main-thread]`; `get_channel_map` — `uint32_t(CLAP_ABI *get_channel_map)(const
clap_plugin_t *plugin, bool is_input, uint32_t port_index, uint8_t *channel_map, uint32_t
channel_map_capacity)` — `[main-thread]`.
plugin → host: `changed` — `void(CLAP_ABI *changed)(const clap_host_t *host)` — `[main-thread]`.

`CLAP_PORT_SURROUND = "surround"`. Channel ids 0–19: `FL` 0, `FR` 1, `FC` 2, `LFE` 3, `BL` 4, `BR` 5,
`FLC` 6, `FRC` 7, `BC` 8, `SL` 9, `SR` 10, `TC` 11, `TFL` 12, `TFC` 13, `TFR` 14, `TBL` 15, `TBC` 16,
`TBR` 17, `TSL` 18, `TSR` 19.

> **"The channel map can only change when the plugin is de-activated."**
> "channel_map_capacity must be greater or equal to the channel count of the given port."
> Negotiation workflow: "1. the plugin queries the host preferred channel mapping and adjusts its configuration to match it. 2. the host checks how the plugin is effectively configured and honors it."
> Host-initiated change: "1. deactivate the plugin 2. host pushes a new configuration using clap_plugin_configurable_audio_ports 3. host activates the plugin and can start processing audio"
> Plugin-initiated change: "1. call host->request_restart() if the plugin is active 2. once deactivated plugin calls clap_host_surround->changed() 3. host calls clap_plugin_surround->get_channel_map() 4. host activates the plugin and can start processing audio"

### `clap.ambisonic/3` (`ext/ambisonic.h`) — STABLE

host → plugin: `is_config_supported` — `bool(CLAP_ABI *)(const clap_plugin_t *plugin, const
clap_ambisonic_config_t *config)` — `[main-thread]`; `get_config` — `bool(CLAP_ABI
*get_config)(const clap_plugin_t *plugin, bool is_input, uint32_t port_index,
clap_ambisonic_config_t *config)` — `[main-thread]`.
plugin → host: `changed` — `void(CLAP_ABI *changed)(const clap_host_t *host)` — `[main-thread]`.

`CLAP_PORT_AMBISONIC = "ambisonic"`.
`enum clap_ambisonic_ordering`: `FUMA` 0, `ACN` 1.
`enum clap_ambisonic_normalization`: `MAXN` 0, `SN3D` 1, `N3D` 2, `SN2D` 3, `N2D` 4.
`clap_ambisonic_config` {`ordering` (`uint32_t`), `normalization` (`uint32_t`)} — plain `uint32_t`,
not the enum types.

> **"The info can only change when the plugin is de-activated."**

---

## A.9 Draft extensions (18 headers, 20 IDs)

### `clap.background-activation/1` (DRAFT)

> "Some plugin needs to perform complex computation or even I/O during activation and this blocks the main thread. This extension is here to offer an alternative way to activate the plugin from a background thread, to keep the main-thread running."
> **"Background activation must not be concurrent to a plugin activation/deactivation on the main-thread."**
> "Implementing this extension implies that background activation and deactivation are beneficial and preferred for this plugin."

| Field | Signature | Thread |
|---|---|---|
| `activate_from_background_thread` | `bool(CLAP_ABI *)(clap_plugin_t *plugin, double sample_rate, uint32_t min_frames_count, uint32_t max_frames_count)` | **`[background-thread]`** |
| `deactivate_from_background_thread` | `void(CLAP_ABI *)(const struct clap_plugin *plugin)` | **`[background-thread]`** |

Note: `activate_from_background_thread` takes a **non-const** `clap_plugin_t *` — inconsistent with
the rest of CLAP; likely a draft artifact.

### `clap.background-progress/1` (DRAFT). **Host-only — we implement.**

| Field | Signature | Thread |
|---|---|---|
| `is_canceled` | `bool(CLAP_ABI *is_canceled)(const clap_host_t *host)` | **`[background-thread]`** |
| `progress` | `void(CLAP_ABI *progress)(const clap_host_t *host, double progress, const char *msg)` | **`[background-thread]`** |

> "This interface is used by CLAP_EXT_BACKGROUND_ACTIVATION and CLAP_EXT_BACKGROUND_STATE_CONTEXT."
> "The progress value is from 0 to 1. 0 at the begining and 1 at the end. **Be aware that the progress may go backward if a sub-task fails and the plugin decides to retry it.**"
> "msg: an optional null terminated message (maybe nullptr)…"

### `clap.background-state-context/1` (DRAFT)

| Field | Signature | Thread |
|---|---|---|
| `save_from_background_thread` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin, const clap_ostream_t *stream, uint32_t context_type)` | **`[background-thread]`** |
| `load_from_background_thread` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin, const clap_istream_t *stream, uint32_t context_type)` | **`[background-thread]`** |

> **"Background save and load must not be concurrent to main-thread save and load."**

### `clap.extensible-audio-ports/1` (DRAFT). No host struct.

| Field | Signature | Thread |
|---|---|---|
| `add_port` | `bool(CLAP_ABI *add_port)(const clap_plugin_t *plugin, bool is_input, uint32_t channel_count, const char *port_type, const void *port_details)` | **`[main-thread & !active]`** |
| `remove_port` | `bool(CLAP_ABI *remove_port)(const clap_plugin_t *plugin, bool is_input, uint32_t index)` | **`[main-thread & !active]`** |

> "Asks the plugin to add a new port (at the end of the list)…"

### `clap.flush-events/1` (DRAFT)

> "This interface is useful to exchange events with the plugin while not processing. This is useful to perform MIDI 2.0 non realtime communication on the main-thread before activating the plugin."

| Direction | Field | Signature | Thread |
|---|---|---|---|
| host → plugin | `flush` | `void(CLAP_ABI *flush)(const clap_plugin_t *plugin, const clap_input_events_t *in, const clap_output_events_t *out)` | **`[active ? audio-thread : main-thread]`** |
| plugin → host | `request_flush` | `void(CLAP_ABI *request_flush)(const clap_host_t *host)` | **`[thread-safe,!audio-thread]`** |

> **"This method must not be called concurrently to clap_plugin->process()."**
> "The host will then schedule a call to either: - clap_plugin.process() - clap_plugin_flush_events.flush()"

### `clap.gain-adjustment-metering/0` (DRAFT). No host struct. Only `/0` ID in the tree.

`get` — `double(CLAP_ABI *get)(const clap_plugin_t *plugin)` — **`[audio-thread]`**.

> "The returned value represents the gain adjustment that the plugin applied to the last sample in the most recently processed block."
> "Zero means the plugin is applying no gain reduction, or is not processing. A negative value means the plugin is applying gain reduction… A positive value means the plugin is adding gain…"
> "The value represents the dynamic gain reduction or expansion applied by the plugin, before any make-up gain or other adjustment. **A single value is returned for all audio channels.**"

### `clap.mini-curve-display/3` (DRAFT)

**host → plugin — note `get_curve_count` is unannotated (verified directly against the header):**

| Field | Signature | Thread |
|---|---|---|
| `get_curve_count` | `uint32_t(CLAP_ABI *get_curve_count)(const clap_plugin_t *plugin)` | **unannotated** |
| `render` | `uint32_t(CLAP_ABI *render)(const clap_plugin_t *plugin, clap_mini_curve_display_curve_data_t *curves, uint32_t curves_size)` | `[main-thread]` |
| `set_observed` | `void(CLAP_ABI *set_observed)(const clap_plugin_t *plugin, bool is_observed)` | `[main-thread]` |
| `get_axis_name` | `bool(CLAP_ABI *get_axis_name)(const clap_plugin_t *plugin, uint32_t curve_index, char *x_name, char *y_name, uint32_t name_capacity)` | `[main-thread]` |

**plugin → host:** `get_hints` — `bool(CLAP_ABI *get_hints)(const clap_host_t *host, uint32_t kind,
clap_mini_curve_display_curve_hints_t *hints)` — `[main-thread]`; `set_dynamic` — `void(CLAP_ABI
*set_dynamic)(const clap_host_t *host, bool is_dynamic)` — `[main-thread]`; `changed` —
`void(CLAP_ABI *changed)(const clap_host_t *host, uint32_t flags)` — `[main-thread]`.

`enum clap_mini_curve_display_curve_kind`: `UNSPECIFIED` 0, `GAIN_RESPONSE` 1 (y dB, x Hz log),
`PHASE_RESPONSE` 2 (y radians, x Hz log), `TRANSFER_CURVE` 3 (both dB), `GAIN_REDUCTION` 4 (x
seconds, y dB), `TIME_SERIES` 5. Trailing note: "more entries could be added here in the future".

`enum clap_mini_curve_display_change_flags`: `CURVE_CHANGED` `1<<0` ("Can only be called if the curve
is observed and is static."), `AXIS_NAME_CHANGED` `1<<1` ("Can only be called if the curve is
observed.").

`clap_mini_curve_display_curve_hints` {`x_min`, `x_max`, `y_min`, `y_max`}.
`clap_mini_curve_display_curve_data` {`curve_kind` (`int32_t`), `values` (`uint16_t*`),
`values_count`}.

> "curves is an array, and each entries (up to curves_size) contains pre-allocated values buffer that must be filled by the plugin."
> "The host will 'stack' the curves, from the first one to the last one. curves[0] is the first curve to be painted. curves[n + 1] will be painted over curves[n]."
> **"When it isn't observed render() can't be called."**
> "When is_obseverd becomes true, the curve content and axis name are implicitly invalidated. So the plugin don't need to call host->changed." *(sic)*
> "The value 0 and UINT16_MAX won't be painted. The value 1 will be at the bottom of the curve and UINT16_MAX - 1 will be at the top."
> "The curve is initially considered as static… When static, the curve changes will be notified by calling host->changed(). When dynamic, the curve is constantly changing and the host is expected to periodically re-render."

### `clap.octave-number/1` (DRAFT). No host struct.

`set_note60_octave` — `void(CLAP_ABI *set_note60_octave)(const clap_plugin_t *plugin, int8_t
octave_number)` — `[main-thread]`.

> "Since various hosts and standards call note 60 either C3, C4 or C5, host displays and plugin displays often mismatch absent this information being shared."
> "For instance, calling this with '3' would indicate to the plugin that the consistent name for note 60 is 'C3', for 72 is 'C4' etc…"

### `clap.param-hovered/1` (DRAFT). **Host-only — we implement.**

`update` — `void(CLAP_ABI *update)(const clap_host_t *host, clap_id hovered_param_id)` —
`[main-thread]`.

> "Should be called whenever the hovered UI control's parameter ID changes or when it changes from hovered to not being hovered."
> **"Only one parameter can be hovered at a time. Use CLAP_INVALID_ID as param_id if no parameter is hovered."**

### `clap.params-origin/1` (DRAFT)

| Direction | Field | Signature | Thread |
|---|---|---|---|
| host → plugin | `get` | `bool(CLAP_ABI *get)(const clap_plugin_t *plugin, clap_id param_id, double *out_value)` | `[main-thread]` |
| plugin → host | `changed` | `void(CLAP_ABI *changed)(const clap_host_t *host)` | `[main-thread]` |

> **"The host must not call this for params with CLAP_PARAM_IS_ENUM flag set."**
> "out_value constraints: - has to be in the range from param_info.min_value to param_info.max_value - has to be an integer value if CLAP_PARAM_IS_STEPPED flag is set"
> "Note: If the plugin calls params.rescan with CLAP_PARAM_RESCAN_ALL, all previously scanned parameter origins must be considered invalid. It is thus not necessary for the plugin to call param_origin.changed in this case."

### `clap.project-location/2` (DRAFT). No host struct.

`set` — `void(CLAP_ABI *set)(const clap_plugin_t *plugin, const clap_project_location_element_t
*path, uint32_t num_elements)` — `[main-thread]`.

`enum clap_project_location_kind`: `PROJECT` 1, `TRACK_GROUP` 2, `TRACK` 3, `DEVICE` 4,
`NESTED_DEVICE_CHAIN` 5.
`enum clap_project_location_track_kind`: `INSTUMENT_TRACK` 1 *(sic — missing R)*, `AUDIO_TRACK` 2,
`HYBRID_TRACK` 3, `RETURN_TRACK` 4, `MASTER_TRACK` 5.
`enum clap_project_location_flags`: `HAS_INDEX` `1<<0`, `HAS_COLOR` `1<<1`.

`clap_project_location_element` {`flags` (`uint64_t`), `kind`, `track_kind`, `index`,
`id[CLAP_PATH_SIZE]`, `name[CLAP_NAME_SIZE]`, `color`}.

> **"The path is expected to be something like: PROJECT > TRACK_GROUP+ > TRACK > (DEVICE > NESTED_DEVICE_CHAIN)* > DEVICE"**
> "The last item in this array always refers to the device itself, and as such is expected to be of kind CLAP_PLUGIN_LOCATION_DEVICE."
> "The first item in this array always refers to the project this device is in and must be of kind CLAP_PROJECT_LOCATION_PROJECT."
> "Its parent must be a device." (NESTED_DEVICE_CHAIN)
> "The first device within a track group has the index of the last track or track group within this group + 1."

### `clap.resource-directory/1` (DRAFT)

**host → plugin:**

| Field | Signature | Thread |
|---|---|---|
| `set_directory` | `void(CLAP_ABI *set_directory)(const clap_plugin_t *plugin, const char *path, bool is_shared)` | `[main-thread]` |
| `collect` | `void(CLAP_ABI *collect)(const clap_plugin_t *plugin, bool all)` | `[main-thread]` |
| `get_files_count` | `uint32_t(CLAP_ABI *get_files_count)(const clap_plugin_t *plugin)` | `[main-thread]` |
| `get_file_path` | `int32_t(CLAP_ABI *get_file_path)(const clap_plugin_t *plugin, uint32_t index, char *path, uint32_t path_size)` | `[main-thread]` |

**plugin → host:** `request_directory` — `bool(CLAP_ABI *)(const clap_host_t *host, bool is_shared)`
— `[main-thread]`; `release_directory` — `void(CLAP_ABI *)(const clap_host_t *host, bool is_shared)`
— `[main-thread]`.

> **"The plugin must store relative path in its state toward resource directories."**
> "shared directory is shared among all plugin instances, hence mostly appropriate for read-only content"
> "exclusive directory is exclusive to the plugin instance -> if the plugin, then its exclusive directory must be duplicated too"
> "exclusive folder content is deleted when the plugin instance is removed from the project"
> "shared folder content isn't managed by the host, until all plugins using the shared directory are removed from the project"
> "The directory remains valid until it is overridden or the plugin is destroyed. If path is null or blank, it clears the directory location. **path must be absolute.**"
> "If is_shared = false, then the host may delete the directory content."
> "host can 'garbage collect' the files in the shared folder using … but be **very** careful before deleting any resources"

### `clap.scratch-memory/1` (DRAFT). **Host-only — we implement.**

| Field | Signature | Thread |
|---|---|---|
| `reserve` | `bool(CLAP_ABI *reserve)(const clap_host_t *host, uint32_t scratch_size_bytes, uint32_t max_concurrency_hint)` | **`[main-thread & being-activated]`** |
| `access` | `void *(CLAP_ABI *access)(const clap_host_t *host)` | **`[audio-thread]`** |

> "The scratch memory is thread-local, and can be accessed during `clap_plugin->process()` and `clap_plugin_thread_pool->exec()`; its content is not persistent between callbacks."
> "The motivation for this extension is to allow the plugin host to 'share' a single scratch buffer across multiple plugin instances."
> "If the plugin calls `reserve()` multiple times, then the last call invalidates all previous calls. **De-activating the plugin releases the scratch memory.**"
> "`max_concurrency_hint` is an optional hint which indicates the maximum number of threads concurrently accessing the scratch memory. Set to 0 if unspecified."
> "If the scratch memory wasn't successfully reserved, returns NULL. If the plugin crosses `max_concurrency_hint`, then the return value is either NULL or a valid scratch memory pointer."
> **"The plugin must not hold any references to data that lives in the scratch memory after returning from the callback, as that data will likely be over-written by another plugin using the same scratch memory."**
> "The provided memory is not initialized… so the plugin must correctly initialize the memory when using it. The provided memory is owned by the host, so the plugin must not free the memory."

### `clap.transport-control/2` (DRAFT). **Host-only — 13 functions, all `[main-thread]`.**

> **"The host has no obligation to execute these requests, so the interface may be partially working."**

| Field | Signature | Thread | Doc |
|---|---|---|---|
| `request_start` | `void(CLAP_ABI *)(const clap_host_t *host)` | `[main-thread]` | "Jumps back to the start point and starts the transport" |
| `request_stop` | `void(CLAP_ABI *)(const clap_host_t *host)` | `[main-thread]` | "Stops the transport, and jumps to the start point" |
| `request_continue` | `void(CLAP_ABI *)(const clap_host_t *host)` | `[main-thread]` | "If not playing, starts the transport from its current position" |
| `request_pause` | `void(CLAP_ABI *)(const clap_host_t *host)` | `[main-thread]` | "If playing, stops the transport at the current position" |
| `request_toggle_play` | `void(CLAP_ABI *)(const clap_host_t *host)` | `[main-thread]` | "Equivalent to what 'space bar' does with most DAWs" |
| `request_jump` | `void(CLAP_ABI *)(const clap_host_t *host, clap_beattime position)` | `[main-thread]` | "Jumps the transport to the given position. Does not start the transport." |
| `request_loop_region` | `void(CLAP_ABI *)(const clap_host_t *host, clap_beattime start, clap_beattime duration)` | `[main-thread]` | "Sets the loop region" |
| `request_toggle_loop` | `void(CLAP_ABI *)(const clap_host_t *host)` | `[main-thread]` | "Toggles looping" |
| `request_enable_loop` | `void(CLAP_ABI *)(const clap_host_t *host, bool is_enabled)` | `[main-thread]` | "Enables/Disables looping" |
| `request_record` | `void(CLAP_ABI *)(const clap_host_t *host, bool is_recording)` | `[main-thread]` | "Enables/Disables recording" |
| `request_toggle_record` | `void(CLAP_ABI *)(const clap_host_t *host)` | `[main-thread]` | "Toggles recording" |
| `request_tempo` | `void(CLAP_ABI *)(const clap_host_t *host, double tempo)` | `[main-thread]` | "Sets tempo" |
| `request_time_signature` | `void(CLAP_ABI *)(const clap_host_t *host, uint16_t tsig_num, uint16_t tsig_denom)` | `[main-thread]` | "Sets time signature, same format as in clap_event_transport_t." |

### `clap.triggers/1` (DRAFT)

host → plugin: `count` — `uint32_t(CLAP_ABI *count)(const clap_plugin_t *plugin)` — `[main-thread]`;
`get_info` — `bool(CLAP_ABI *get_info)(const clap_plugin_t *plugin, uint32_t index,
clap_trigger_info_t *trigger_info)` — `[main-thread]`.
plugin → host: `rescan` — `void(CLAP_ABI *rescan)(const clap_host_t *host, clap_trigger_rescan_flags
flags)` — `[main-thread]`; `clear` — `void(CLAP_ABI *clear)(const clap_host_t *host, clap_id
trigger_id, clap_trigger_clear_flags flags)` — `[main-thread]`.

`clap_trigger_info_flags`: `IS_AUTOMATABLE_PER_NOTE_ID` `1<<0`, `PER_KEY` `1<<1`, `PER_CHANNEL`
`1<<2`, `PER_PORT` `1<<3`.
`CLAP_EVENT_TRIGGER = 0` (within the extension's own event space).
`clap_trigger_rescan_flags`: `INFO` `1<<0`, `ALL` `1<<1`.
`clap_trigger_clear_flags`: `ALL` `1<<0`, `AUTOMATIONS` `1<<1`.

`clap_event_trigger` {`header`, `trigger_id`, `cookie`, `note_id`, `port_index`, `channel`, `key`}.
`clap_trigger_info` {`id`, `flags`, `cookie`, `name`, `module`}.

> **"Given that this extension is still draft, it'll use the event-registry and its own event namespace until we stabilize it."**
> "stable trigger identifier, it must never change."
> "`CLAP_TRIGGER_RESCAN_ALL` … It can only be used while the plugin is deactivated."
> "Some examples for triggers: - trigger an envelope which is independent of the notes - trigger a sample-and-hold unit (maybe even per-voice)"

### `clap.tuning/2` (DRAFT)

host → plugin: `changed` — `void(CLAP_ABI *changed)(const clap_plugin_t *plugin)` — `[main-thread]`
("Called when a tuning is added or removed from the pool.").

**plugin → host — note the two `[audio-thread & in-process]` entries:**

| Field | Signature | Thread |
|---|---|---|
| `get_relative` | `double(CLAP_ABI *get_relative)(const clap_host_t *host, clap_id tuning_id, int32_t channel, int32_t key, uint32_t sample_offset)` | **`[audio-thread & in-process]`** |
| `should_play` | `bool(CLAP_ABI *should_play)(const clap_host_t *host, clap_id tuning_id, int32_t channel, int32_t key)` | **`[audio-thread & in-process]`** |
| `get_tuning_count` | `uint32_t(CLAP_ABI *get_tuning_count)(const clap_host_t *host)` | `[main-thread]` |
| `get_info` | `bool(CLAP_ABI *get_info)(const clap_host_t *host, uint32_t tuning_index, clap_tuning_info_t *info)` | `[main-thread]` |

`clap_event_tuning` {`header`, `port_index` ("-1 global"), `channel` ("0..15, -1 global"),
`tunning_id` *(sic — double n)*}.
`clap_tuning_info` {`tuning_id`, `name`, `is_dynamic` ("true if the values may vary with time")}.

> "Gets the relative tuning in semitones against equal temperament with A4=440Hz."
> "The plugin may query the tuning at a rate that makes sense for *low* frequency modulations."
> **"If the tuning_id is not found or equals to CLAP_INVALID_ID, then the function shall gracefully return a sensible value."**
> "should_play(...) should be checked before calling this function."
> "Use clap_host_event_registry->query(host, CLAP_EXT_TUNING, &space_id) to know the event space."

### `clap.undo/4` + `clap.undo_context/4` + `clap.undo_delta/4` (DRAFT) — three IDs in one header

**plugin → host — `clap_host_undo` (`clap.undo/4`). We implement.**

| Field | Signature | Thread |
|---|---|---|
| `begin_change` | `void(CLAP_ABI *begin_change)(const clap_host_t *host)` | `[main-thread]` |
| `cancel_change` | `void(CLAP_ABI *cancel_change)(const clap_host_t *host)` | `[main-thread]` |
| `change_made` | `void(CLAP_ABI *change_made)(const clap_host_t *host, const char *name, const void *delta, size_t delta_size, bool delta_can_undo)` | `[main-thread]` |
| `request_undo` | `void(CLAP_ABI *request_undo)(const clap_host_t *host)` | `[main-thread]` |
| `request_redo` | `void(CLAP_ABI *request_redo)(const clap_host_t *host)` | `[main-thread]` |
| `set_wants_context_updates` | `void(CLAP_ABI *set_wants_context_updates)(const clap_host_t *host, bool is_subscribed)` | `[main-thread]` |

**host → plugin — `clap_plugin_undo_context` (`clap.undo_context/4`)**

| Field | Signature | Thread |
|---|---|---|
| `set_can_undo` | `void(CLAP_ABI *set_can_undo)(const clap_plugin_t *plugin, bool can_undo)` | **`[main-thread & plugin-subscribed-to-undo-context]`** |
| `set_can_redo` | `void(CLAP_ABI *set_can_redo)(const clap_plugin_t *plugin, bool can_redo)` | **`[main-thread & plugin-subscribed-to-undo-context]`** |
| `set_undo_name` | `void(CLAP_ABI *set_undo_name)(const clap_plugin_t *plugin, const char *name)` | **`[main-thread & plugin-subscribed-to-undo-context]`** |
| `set_redo_name` | `void(CLAP_ABI *set_redo_name)(const clap_plugin_t *plugin, const char *name)` | **`[main-thread & plugin-subscribed-to-undo-context]`** |

**host → plugin — `clap_plugin_undo_delta` (`clap.undo_delta/4`)**

| Field | Signature | Thread |
|---|---|---|
| `get_delta_properties` | `void(CLAP_ABI *)(const clap_plugin_t *plugin, clap_undo_delta_properties_t *properties)` | `[main-thread]` |
| `can_use_delta_format_version` | `bool(CLAP_ABI *)(const clap_plugin_t *plugin, clap_id format_version)` | `[main-thread]` |
| `undo` | `bool(CLAP_ABI *undo)(const clap_plugin_t *plugin, clap_id format_version, const void *delta, size_t delta_size)` | `[main-thread]` |
| `redo` | `bool(CLAP_ABI *redo)(const clap_plugin_t *plugin, clap_id format_version, const void *delta, size_t delta_size)` | `[main-thread]` |

`clap_undo_delta_properties` {`has_delta`, `are_deltas_persistent`, `format_version`}.

> "This extension enables the plugin to merge its undo history with the host. This leads to a single undo history shared by the host and many plugins."
> "If the plugin uses this interface then its undo and redo should be entirely delegated to the host."
> **"The plugin must not call this twice: there must be either a call to cancel_change() or change_made() before calling begin_change() again."**
> **"cancel_change() must not be called without a preceding begin_change()."**
> "The host may group changes together… The host will then create a single undo step that will merge all the changes into C0."
> **"starting a long running change without terminating is **VERY BAD**, because while a change is running it is impossible to call undo or redo."**
> "At the moment of this function call, plugin_state->save() would include the current change."
> "delta: optional… When not available the host will save the plugin state and use state->load() to perform undo and redo. **The plugin must be able to perform a redo operation using the delta, though the undo operation is only possible if delta_can_undo is true.**"
> "Special case: for objects with shared and synchronized state, changes shouldn't be reported as the host already knows about it. For example, plugin parameter changes shouldn't produce a call to change_made()."
> "Note: if the plugin asked for this interface, then host_state->mark_dirty() will not create an implicit undo step."
> "Note: this maybe a complex and asynchronous operation, which may complete after this function returns." (request_undo/redo)
> "Initial state is unsubscribed. **It is mandatory for the plugin to implement CLAP_EXT_UNDO_CONTEXT when using this method.**"
> "If false, then format_version must be set to CLAP_INVALID_ID." (are_deltas_persistent)

### `clap.webview/3` (DRAFT)

**host → plugin:**

| Field | Signature | Thread |
|---|---|---|
| `get_uri` | `int32_t(CLAP_ABI *get_uri)(const clap_plugin_t *plugin, char *uri, uint32_t uri_capacity)` | `[main-thread]` |
| `get_resource` | `bool(CLAP_ABI *get_resource)(const clap_plugin_t *plugin, const char *path, char *mime, uint32_t mime_capacity, const clap_ostream_t *data_stream)` | `[main-thread]` |
| `receive` | `bool(CLAP_ABI *receive)(const clap_plugin_t *plugin, const void *buffer, uint32_t size)` | `[main-thread]` |

**plugin → host:** `send` — `bool(CLAP_ABI *send)(const clap_host_t *host, const void *buffer,
uint32_t size)` — `[main-thread]`.

`CLAP_WINDOW_API_WEBVIEW = "webview"` — a `clap.gui` API constant, not an extension id.
> "The pointer in clap_window must be NULL, but sizing methods are useful. This uses logical size, don't call clap_plugin_gui->set_scale()"

> "Messages are received in the webview using a standard MessageEvent, with the data in an ArrayBuffer. They are posted back to the plugin using window.parent.postMessage(), with the data in an ArrayBuffer or TypedArray."
> **"This must be called at least once before any messages are sent (or accepted) by the host."** (get_uri)
> "Relative URIs (with absolute paths) refer to resources provided by .get_resource()… The host may also translate `file:` URIs to some other scheme or path root, to limit access scope or handle virtual filesystems. Therefore, when using either relative or `file:` URIs, **pages must not assume a particular absolute path, only relative paths between resources.**"
> "Returns either the full length of the URI (including the null terminator), or <= 0 for an error. If the value returned is greater than the capacity, then the result was truncated. If the capacity is 0, `uri` may be a null pointer."
> "The path must be absolute (starting with `/`) with any host-defined path prefix removed."
> "It must fail (false) if the webview is not open." (send)

---

# Part C — Supporting vocabulary

## `id.h`
`typedef uint32_t clap_id;` · `CLAP_INVALID_ID = UINT32_MAX`.

## `string-sizes.h`
`CLAP_NAME_SIZE = 256` — "String capacity for names that can be displayed to the user."
`CLAP_PATH_SIZE = 1024` — "String capacity for describing a path… This is not suited for describing a
file path on the disk, as NTFS allows up to 32K long paths."

## `fixedpoint.h`
`CLAP_BEATTIME_FACTOR = 1LL << 31` · `CLAP_SECTIME_FACTOR = 1LL << 31` — both marked **"This will
never change"**. `typedef int64_t clap_beattime;` `typedef int64_t clap_sectime;`
> "double x = ...; // in beats / clap_beattime y = round(CLAP_BEATTIME_FACTOR * x);"

## `timestamp.h`
`typedef uint64_t clap_timestamp;` — "the number of seconds since UNIX EPOCH."
`CLAP_TIMESTAMP_UNKNOWN = 0`.

## `color.h`
`clap_color` {`alpha`, `red`, `green`, `blue`} — all `uint8_t`. **Field order is ARGB, not RGBA.**
`CLAP_COLOR_TRANSPARENT = {0,0,0,0}`.

## `universal-plugin-id.h`
`clap_universal_plugin_id` {`abi`, `id`}.

| ABI | `id` format | Example |
|---|---|---|
| CLAP | "use the plugin id" | `"com.u-he.diva"` |
| AU | `"type:subt:manu"` | `"aumu:SgXT:VmbA"` |
| VST2 | "print the id as a signed 32-bits integer" | `"-4382976"` |
| VST3 | "print the id as a standard UUID" | `"123e4567-e89b-12d3-a456-426614174000"` |

## `plugin-features.h` — all 40 constants

> "For practical reasons we'll avoid spaces and use `-` instead… Non-standard features should be formatted as follow: `$namespace:$feature`"

**Category (5):** `INSTRUMENT` `"instrument"` · `AUDIO_EFFECT` `"audio-effect"` · `NOTE_EFFECT`
`"note-effect"` · `NOTE_DETECTOR` `"note-detector"` · `ANALYZER` `"analyzer"`

**Sub-category (31):** `SYNTHESIZER` `"synthesizer"` · `SAMPLER` `"sampler"` · `DRUM` `"drum"` ·
`DRUM_MACHINE` `"drum-machine"` · `FILTER` `"filter"` · `PHASER` `"phaser"` · `EQUALIZER`
`"equalizer"` · `DEESSER` `"de-esser"` · `PHASE_VOCODER` `"phase-vocoder"` · `GRANULAR` `"granular"`
· `FREQUENCY_SHIFTER` `"frequency-shifter"` · `PITCH_SHIFTER` `"pitch-shifter"` · `DISTORTION`
`"distortion"` · `TRANSIENT_SHAPER` `"transient-shaper"` · `COMPRESSOR` `"compressor"` · `EXPANDER`
`"expander"` · `GATE` `"gate"` · `LIMITER` `"limiter"` · `FLANGER` `"flanger"` · `CHORUS` `"chorus"`
· `DELAY` `"delay"` · `REVERB` `"reverb"` · `TREMOLO` `"tremolo"` · `GLITCH` `"glitch"` · `UTILITY`
`"utility"` · `PITCH_CORRECTION` `"pitch-correction"` · `RESTORATION` `"restoration"` ·
`MULTI_EFFECTS` `"multi-effects"` · `MIXING` `"mixing"` · `MASTERING` `"mastering"`

**Audio capabilities (4):** `MONO` `"mono"` · `STEREO` `"stereo"` · `SURROUND` `"surround"` ·
`AMBISONIC` `"ambisonic"`

Note the macro/string mismatch on `CLAP_PLUGIN_FEATURE_DEESSER` → `"de-esser"`.

## `private/macros.h`
`CLAP_ABI` = `__cdecl` on Win32/Cygwin, empty elsewhere. `CLAP_EXPORT` = `dllexport` on Windows,
`visibility("default")` on GCC≥4/clang. `CLAP_CONSTEXPR` = `constexpr` under C++11+, else empty.

---

# Appendix — Host implementation checklist derived from the above

**Every function pointer that is `unannotated` in the headers** (do not assume a thread for these):

| Location | Function | Why it matters |
|---|---|---|
| `entry.h` | `init`, `deinit` | Prose says any thread, but never concurrently with any other DSO symbol |
| `ext/thread-pool.h` | `clap_plugin_thread_pool::exec` | Called from our pool during `process()`; the header never says `[audio-thread]` |
| `ext/context-menu.h` | builder `add_item`, `supports` | Struct says "isn't thread-safe and must be used on the same thread as it was provided" |
| `ext/draft/mini-curve-display.h` | `get_curve_count` | The sibling three are `[main-thread]`; this one is not annotated |
| `stream.h` | `clap_istream::read`, `clap_ostream::write` | We supply these; called from `[main-thread]` state save/load, and `[background-thread]` under background-state-context |
| `events.h` | `input_events::size`/`get`, `output_events::try_push` | Context-dependent: `process()` is `[audio-thread]`, `flush()` is `[active ? audio-thread : main-thread]` |
| `factory/preset-discovery.h` | all 19 fns across receiver/provider/indexer | All three structs say "isn't thread-safe"; only the factory itself is `[thread-safe]` |
| `factory/draft/plugin-invalidation.h` | `count`, `refresh` | `get` is `[thread-safe]`; these two are not annotated |
| `factory/draft/plugin-state-converter.h` | `destroy` | The three convert fns are `[thread-safe]` |

**Host-side structs we must implement, by thread class:**

- `[thread-safe]` (callable from anywhere, concurrently): `clap_host` core 4, `clap_host_log`,
  `clap_host_thread_check`, `clap_host_gui` (`request_show`/`request_hide`/`closed`).
- `[thread-safe & !floating]`: `clap_host_gui::resize_hints_changed`, `request_resize`.
- `[thread-safe,!audio-thread]`: `clap_host_params::request_flush`,
  `clap_host_flush_events::request_flush`.
- `[audio-thread]` — **must meet realtime constraints or we must refuse hard-realtime plugins**:
  `clap_host_thread_pool::request_exec`, `clap_host_tail::changed`,
  `clap_host_scratch_memory::access`.
- `[audio-thread & in-process]`: `clap_host_tuning::get_relative`, `should_play`.
- `[background-thread]`: `clap_host_background_progress::is_canceled`, `progress`.
- `[main-thread & being-activated]`: `clap_host_latency::changed`,
  `clap_host_scratch_memory::reserve`.
- Everything else host-side: `[main-thread]`.

**The four state-qualified plugin calls a host must gate on activation state:**
- `[active ? audio-thread : main-thread]` — `params::flush`,
  `audio_ports_activation::set_active`, `flush_events::flush`. We must track activation to dispatch
  these on the right thread.
- `[main-thread & !active]` — `plugin::destroy`, `plugin::activate`,
  `configurable_audio_ports::{can_apply,apply}_configuration`,
  `extensible_audio_ports::{add,remove}_port`.
- `[main-thread & active]` — `plugin::deactivate`, `voice_info::get`.
- `[main-thread & plugin-deactivated]` — `audio_ports_config::select`.
