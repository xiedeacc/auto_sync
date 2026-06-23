# Project Rules

- For every local code-change loop, stop old `auto_sync` processes first:
  `auto_syncd`, `auto_syncctl`, `auto_sync_web`, and `auto_sync_gui`.
- After stopping old processes, run an incremental debug build: `cargo build --bins`.
- After the debug build succeeds, execute the freshly built relevant binary from `target/debug/` or rerun the relevant script.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
- When the user asks to deploy or when real remote E2E tests/debugging need the latest code, always use this update path:
  1. On Windows, commit all intended repository changes and push them.
  2. Deploy this Windows machine with `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
  3. SSH to `root@tiger`; in `/root/src/rust/auto_sync`, run `git pull`.
  4. Compile and deploy from tiger by running `scripts/deploy_local.sh` for tiger and `scripts/deploy_nas.sh` for nas.
- Do not use Windows-to-Linux cross-compilation for the normal tiger/nas deployment path; build Linux binaries on tiger through the deploy scripts above.
