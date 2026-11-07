# tutti-plugin-db

Plugin discovery for the Tutti engine.

- `discover` walks directories and returns plugin paths with their format.
- `PluginRecord`, `PluginCatalog`, `CatalogExt`: one row per plugin, and the
  store behind it. `JsonCatalog` (feature `json`) keeps the rows in a JSON file.
- `PluginScanner` scans into a catalog, skips plugins whose modification time
  is unchanged, and blacklists one that fails to load, hangs or crashes its
  probe. A sentinel file (the pedal) blacklists the plugin a scan was probing
  when the host itself died.
- The probe is a parameter: a function from a path and a format to a
  `PluginDescriptor` or a `ProbeFailure`. The crate never loads a plugin.
  `tutti-plugin` supplies the out-of-process one.

Depends on `tutti-plugin-types` among the workspace crates. Licensed under MIT
or Apache-2.0.
