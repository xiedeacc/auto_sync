# Project Rules

- The project ships a single runtime binary `auto_sync` (scheduler + watcher + web, plus the desktop window when a display is available) and the `auto_syncctl` CLI. The old `auto_syncd` / `auto_sync_gui` / `auto_sync_web` binaries were merged into `auto_sync`.
- For every local code-change loop, stop old `auto_sync` processes first: `auto_sync` (and any legacy `auto_syncd`, `auto_sync_web`, `auto_sync_gui`).
- After stopping old processes, run an incremental debug build: `cargo build --bins`.
- On Windows, before starting any `auto_sync` process, copy the freshly compiled binaries into `D:\code\auto_sync\bin` and start them only from that `bin\` directory, never directly from `target\debug\` or `target\release\`.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`. On Windows, deploy binaries into the repository `bin\` directory and use a current-user Startup launcher for the single `auto_sync` process; always start it from `bin\` after deployment, and do not install or start `auto_sync` as a Windows service.
- When the user asks to deploy or when real remote E2E tests/debugging need the latest code, always use this update path:
  1. On Windows, commit all intended repository changes and push them.
  2. Deploy this Windows machine with `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
  3. Deploy NAS directly on NAS: `ssh -p 10022 root@192.168.2.247 "cd /root/src/rust/auto_sync && git pull && scripts/deploy_local.sh"`.
- Do not deploy to tiger and do not use Windows-to-Linux cross-compilation for the normal NAS deployment path; build Linux x64 binaries on NAS.
