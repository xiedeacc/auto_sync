# Project Rules

- For every local code-change loop, stop old `auto_sync` processes first:
  `auto_syncd`, `auto_syncctl`, `auto_sync_web`, and `auto_sync_gui`.
- After stopping old processes, run an incremental debug build: `cargo build --bins`.
- On Windows, before starting any `auto_sync` process, copy the freshly compiled binaries into `D:\code\auto_sync\bin` and start them only from that `bin\` directory, never directly from `target\debug\` or `target\release\`.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`. On Windows, deploy binaries into the repository `bin\` directory and use a current-user Startup launcher for `auto_syncd`; do not install or start `auto_syncd` as a Windows service.
- When the user asks to deploy or when real remote E2E tests/debugging need the latest code, always use this update path:
  1. On Windows, commit all intended repository changes and push them.
  2. Deploy this Windows machine with `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
  3. Deploy NAS directly on NAS: `ssh -p 10022 root@192.168.2.247 "cd /root/src/rust/auto_sync && git pull && scripts/deploy_local.sh"`.
- Do not deploy to tiger and do not use Windows-to-Linux cross-compilation for the normal NAS deployment path; build Linux x64 binaries on NAS.
