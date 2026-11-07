# tutti-plugin-db

Plugin discovery for the Tutti engine.

- `discover` walks directories and returns plugin paths with their format. Each
  directory and each plugin is visited once by its canonical path, so a symlink
  cycle ends the walk, and a plugin is reported under its canonical path (with
  the format of the name it was found under), so every route to it is one row.
- `PluginDb` is a plain table, `path -> FileRow`: the file's modification time,
  then either the plugins it holds or a blacklist reason. The file is the unit
  because a crash blacklists a file before any plugin id is known. A `Delta`
  replaces or drops one row, and `PluginDb::apply` is the only code that
  changes the table. `save` and `load` (feature `json`) write and read it as a
  versioned file: a file of another version, or an old one with none, is moved
  aside and rebuilt by a rescan, and a corrupt one is quarantined.
- `Scanner` probes the files that are new or changed since their row was
  stamped, and blacklists one that fails to load, hangs or crashes its probe.
  A scan runs on a worker over a snapshot and sends deltas; the owner applies
  them with `Scan::poll`, so the table stays readable for the whole scan. A
  sentinel file (the pedal) blacklists the file a scan was probing when the
  host itself died. One pedal serves one scan at a time: a second scan or
  recovery on the same pedal is refused with `ScanBusy` until the first
  worker thread has exited. Dropping a `Scan` stops its worker after the file
  in flight.
- The probe is a parameter: a function from a path and a format to a
  `PluginDescriptor` or a `ProbeFailure`. The crate never loads a plugin.
  `tutti-plugin` supplies the out-of-process one.

The table belongs to one thread. Nothing here runs on, or reaches, the audio
thread.

Depends on `tutti-plugin-types` among the workspace crates. Licensed under MIT
or Apache-2.0.
