# Project Rules

- For every local code-change loop, stop old `auto_sync` processes first:
  `auto_syncd`, `auto_syncctl`, `auto_sync_web`, and `auto_sync_gui`.
- After stopping old processes, run an incremental debug build: `cargo build --bins`.
- After the debug build succeeds, execute the freshly built relevant binary from `target/debug/` or rerun the relevant script.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
