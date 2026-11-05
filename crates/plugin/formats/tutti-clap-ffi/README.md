# tutti-clap-ffi

The unsafe half of the CLAP host. Every `unsafe` the host needs to talk to a
CLAP plugin lives here, behind safe handles over [`clap-sys`]; the host crate
above it, `tutti-clap-host`, is `#![forbid(unsafe_code)]` and holds the logic
(the lifecycle, the shell, the request words, the edit ring).

This crate names nothing of tutti's except `tutti-plugin-types`, for the one
value whose unsafe constructor carries a claim this crate relies on
(`WindowHandle`, a valid parent window).

## What is here

| | |
|---|---|
| `Bundle` | opens a library, runs its entry once per image, lists the factory's descriptors, instantiates one |
| `Plugin` | the **main-thread half** of an instance: every extension call is a method, and its drop deactivates and destroys the plugin |
| `AudioSide` | the **audio-thread half**: `process`, `reset`, `stop`, each under the instance's audio claim; it names no main-thread entry |
| `Host`, `HostCallbacks` | the `clap_host` vtable and every extension vtable a plugin reaches through it, each `extern "C"` body under the panic guard, forwarding to a safe trait the embedder implements |
| `EventSource`, `EventSink` | the host's input and output event lists as safe traits; the raw `clap_input_events` and `clap_output_events` exist only on the stack of the call that needs them |
| `PortScratch`, `ProcessCall` | the channel-pointer tables and the one call that binds a block's buffers and runs the plugin |
| `InputStream`, `OutputStream` | `clap_istream` and `clap_ostream` over byte buffers |
| `TaskPool`, `PoolHandle` | `clap.thread-pool`: a plugin's `request_exec` served on a pool for the span of its `process` call; a handle is bound to the instance it was made for and ignored by any other |
| `ThreadRoles`, `AudioClaim` | which thread is the `[main-thread]` and `[audio-thread]`, and the lock that keeps audio-side entries from overlapping |

## How the thread rules are in the types

CLAP annotates every entry with the thread it may be called on. Here a
`[main-thread]` entry is a method of `Plugin`, which is `Send` but not `Sync`,
and an `[audio-thread]` entry is a method of `AudioSide`, which takes the
instance's `AudioClaim` for the whole call. A processor that holds an
`AudioSide` therefore cannot make a main-thread call, and a controller that
holds the `Plugin` cannot make an audio-thread one, however the embedder wires
them.

The two halves share one instance through an `Arc` of private state, and the
`unsafe impl Sync` on that state is argued in `plugin.rs`: the entries each half
calls are the ones CLAP lets run beside each other; audio-side entries are
serialised by the claim; `activate` needs the main-thread half and no live
audio half; `deactivate` and `destroy` take the claim, so they wait out a
`process` in flight; and a half that outlives the `Plugin` finds the instance
retired and calls nothing.

## What the embedder owes

Nothing unsafe. The one thing that crosses the boundary on trust is
`WindowHandle`, whose `unsafe` constructor is where "this is a valid parent
window" is claimed. Everything else a plugin hands back (strings, structs,
events, pointers into its own storage) is read here, bounds-checked or
validated, and returned as owned or borrowed safe values.

## Testing

The wrappers are tested against the reference plugin (`tutti-clap-test-plugin`)
in `tests/plugin_handle.rs`, and against stub vtables in the unit tests beside
the code they cover. A test that needs a raw list or vtable (a null pointer, an
undersized header, a misaligned event) uses the `*_raw_for_test` hooks, which
are `#[doc(hidden)]`, behind the `test-support` feature (off in a production
build; the host crate's dev-dependency turns it on) and leak a few bytes on
purpose.

[`clap-sys`]: https://docs.rs/clap-sys
