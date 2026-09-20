# MIDI 2.0 specifications

The specification PDFs this crate is written against **are not vendored here.**
They are copyrighted MIDI Association documents: free to download, but not ours
to redistribute. They were in this directory while the engine lived inside a
private repo, and were removed from the tree and from history when it was
published.

Download them from <https://midi.org/specifications>:

| Doc | Title |
|---|---|
| M2-100-U v1.1     | MIDI 2.0 Specification Overview |
| M2-101-UM v1.2.1  | MIDI-CI Specification |
| M2-102-U v1.1     | Common Rules for MIDI-CI Profiles |
| M2-103-UM v1.2    | Common Rules for MIDI-CI Property Exchange |
| M2-104-UM v1.1.2  | UMP and MIDI 2.0 Protocol Specification |
| M2-116-U v1.0     | MIDI Clip File Specification |

M2-104 is the one to reach for first: it defines the Universal MIDI Packet, which
is this crate's native representation. M2-116 covers the Clip File format that
`tutti-midi-file` reads and writes alongside SMF.

Citations in this crate's doc comments give the document number and section, so
they stay resolvable against your own copy.
