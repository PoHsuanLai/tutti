# tutti-alsa-ump-sys

Raw, unsafe FFI to the part of alsa-lib's sequencer and UMP API that the
`alsa-sys` crate does not bind: `snd_ump_block_info_*`,
`snd_seq_get_ump_block_info`, `snd_seq_set_client_midi_version`,
`snd_seq_set_client_ump_conversion` and the `snd_seq_ump_event_t` type. The
sequencer functions it declares alongside them are the ones the UMP path calls.
There are no safe wrappers; `tutti-midi-hardware` has those.

Linux only. On other targets the crate is empty.

## Linking and `links`

`alsa-sys` declares `links = "alsa"`, so this crate declares
`links = "asound"` and the two coexist. Both link `libasound`, which is
harmless. The key also carries the build script's probe result to dependents as
`DEP_ASOUND_ALSA_UMP` (alsa-lib >= 1.2.10) and `DEP_ASOUND_ALSA_UMP_CREATE`
(>= 1.2.13). With an older or missing alsa-lib the build warns and sets
neither; nothing is linked.
