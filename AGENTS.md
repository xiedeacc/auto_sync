# Project Rules

- For every local code-change loop, stop old `auto_sync` processes first:
  `auto_syncd`, `auto_syncctl`, `auto_sync_web`, and `auto_sync_gui`.
- After stopping old processes, run an incremental debug build: `cargo build --bins`.
- On Windows, before starting any `auto_sync` process, copy the freshly compiled binaries into `D:\code\auto_sync\bin` and start them only from that `bin\` directory, never directly from `target\debug\` or `target\release\`.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`. On Windows, deploy binaries into the repository `bin\` directory and use a current-user Startup launcher for both `auto_syncd` and `auto_sync_gui`; always start both from `bin\` after deployment, and do not install or start `auto_syncd` as a Windows service.
- Unless the user explicitly says not to commit, push, or deploy, every completed code/config change must be committed, pushed, and deployed.
- Every commit must include all current repository changes. Do not leave tracked working-tree changes unstaged unless the user explicitly asks to keep them out of the commit.
- When the user asks to deploy or when real remote E2E tests/debugging need the latest code, always use this update path:
  1. On Windows, commit all intended repository changes and push them.
  2. After the push succeeds, deploy this Windows machine and NAS in parallel when possible:
     - Windows: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
     - NAS directly on NAS: `ssh -p 10022 root@192.168.2.247 "cd /opt/auto_sync && git pull && scripts/deploy_local.sh --install-dir /opt/auto_sync"`.
     If parallel execution is not available, run the same two commands sequentially.
- Do not deploy to tiger and do not use Windows-to-Linux cross-compilation for the normal NAS deployment path; build Linux x64 binaries on NAS.
