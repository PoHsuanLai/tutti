# tutti-au-ffi

The unsafe half of the Audio Unit host. Every `unsafe` the host needs to talk to
AudioToolbox, CoreAudio and CoreFoundation lives here, behind safe handles; the
host crate above it, `tutti-au-host`, is `#![forbid(unsafe_code)]` and holds the
logic (the bus and stream configuration, the parameter and preset vocabulary,
the lifecycle).

The crate is empty off macOS.

## What is here

| | |
|---|---|
| `ComponentRef` | an `AudioComponent` found in the registry; `Copy`, and the only way to name a component |
| `Unit` | an owned `AudioComponentInstance`: it disposes on drop, and every AudioToolbox call on it is a method |
| `Plain` | the sealed set of plain-data types a property or parameter can be read into or written from |
| `CfString`, `CfUrl`, `CfArray`, `CfPlist` | owned CoreFoundation values (Create rule), released on drop |

Each `unsafe` block carries one `// SAFETY:` argument where it is introduced;
`undocumented_unsafe_blocks` and `missing_safety_doc` are denied.
