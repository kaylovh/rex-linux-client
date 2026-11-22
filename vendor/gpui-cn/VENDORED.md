# Vendored gpui-cn

This is gpui-cn 0.9.0 (Apache-2.0, https://github.com/prabirshrestha/gpui-cn) as published on
crates.io, minus its Cargo.lock. Its license is in LICENSE-APACHE.

Local changes (each changed file says so at its top):

- `src/terminal/options.rs`: `EngineOptions::with_snapshot` (start from a Ghostty snapshot)
  and `with_source_sized` (the source sets the grid size).
- `src/terminal/core.rs`: `Core::from_snapshot`.
- `src/terminal/engine.rs`: `ByteHandle::route_input` / `InputRoute` (a source can take an
  input instead of having it encoded), `ByteSink::set_grid` (a grid change in stream order), and
  the engine changes behind them.
- `src/terminal/mod.rs`: re-exports `InputRoute`.
- `tests/terminal_rex.rs` (new) and its `[[test]]` entry in `Cargo.toml`; the
  `resolver` line is removed from `Cargo.toml` (the workspace sets it).
