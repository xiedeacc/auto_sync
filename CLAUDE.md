# Project Rules

- The project ships a single runtime binary `auto_sync` (scheduler + watcher + web, plus the desktop window when a display is available) and the `auto_syncctl` CLI. The old `auto_syncd` / `auto_sync_gui` / `auto_sync_web` binaries were merged into `auto_sync`.
- For every local code-change loop, stop old `auto_sync` processes first: `auto_sync` (and any legacy `auto_syncd`, `auto_sync_web`, `auto_sync_gui`).
- After stopping old processes, run an incremental debug build: `cargo build --bins`.
- On Windows, before starting any `auto_sync` process, copy the freshly compiled binaries into `D:\code\auto_sync\bin` and start them only from that `bin\` directory, never directly from `target\debug\` or `target\release\`.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`. On Windows, deploy binaries into the repository `bin\` directory and use a current-user Startup launcher for the single `auto_sync` process; always start it from `bin\` after deployment, and do not install or start `auto_sync` as a Windows service.
- Unless the user explicitly says not to commit, push, or deploy, every completed code/config change must be committed, pushed, and deployed.
- Every commit must include all current repository changes. Do not leave tracked working-tree changes unstaged unless the user explicitly asks to keep them out of the commit.
- Deploy scripts must preserve existing `conf/auto_sync.toml`; only initialize it from a template when the target config file does not exist.
- When the user asks to deploy or when real remote E2E tests/debugging need the latest code, always use this update path:
  1. On Windows, commit all intended repository changes and push them.
  2. After the push succeeds, deploy this Windows machine and NAS in parallel when possible:
     - Windows: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
     - NAS directly on NAS: `ssh -p 10022 root@192.168.2.247 "cd /opt/auto_sync && git pull --ff-only && scripts/deploy_local.sh --install-dir /opt/auto_sync"`.
     If parallel execution is not available, run the same two commands sequentially.
- Do not deploy to tiger and do not use Windows-to-Linux cross-compilation for the normal NAS deployment path; build Linux x64 binaries on NAS.
- For normal NAS deployment, do not delete `/opt/auto_sync`, do not run `git reset --hard`, and do not clean `/opt/auto_sync/target`; preserving Cargo build cache is required. If tracked files in `/opt/auto_sync` are dirty, stop and resolve why instead of masking it with reset.
