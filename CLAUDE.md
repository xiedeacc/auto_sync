# Project Rules

- The project ships a single runtime binary `auto_sync` (scheduler + watcher + web, plus the desktop window when a display is available). The old split binaries were merged into `auto_sync`; scriptable control goes through the HTTP API and UI.
- For every local code-change loop, do not stop or kill existing `auto_sync` processes unless the user explicitly asks for it.
- For local code-change validation, run an incremental debug build when appropriate: `cargo build --bins`.
- On Windows, before starting any `auto_sync` process, copy the freshly compiled binaries into `D:\code\auto_sync\bin` and start them only from that `bin\` directory, never directly from `target\debug\` or `target\release\`.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`. On Windows, deploy binaries into the repository `bin\` directory and use a current-user Startup launcher for the single `auto_sync` process; always start it from `bin\` after deployment, and do not install or start `auto_sync` as a Windows service.
- Unless the user explicitly says not to commit or push, every completed code/config change must be committed and pushed.
- Do not deploy unless the user explicitly asks to deploy to all three platforms. A code/config change request by itself is not a deployment request; a generic deployment request is not enough unless it clearly names Windows, NAS, and dev or otherwise says all three platforms.
- Every commit must include all current repository changes. Do not leave tracked working-tree changes unstaged unless the user explicitly asks to keep them out of the commit.
- Deploy scripts must preserve existing `conf/auto_sync.toml`; only initialize it from a template when the target config file does not exist.
- When the user explicitly asks to deploy to all three platforms, always use this update path:
  1. On Windows, commit all intended repository changes and push them.
  2. After the push succeeds, deploy this Windows machine, NAS, and dev in parallel when possible:
     - Windows: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
     - NAS directly on NAS: `ssh -p 10022 root@192.168.2.247 "cd /opt/usr/local/auto_sync && git pull --ff-only && scripts/deploy_nas.sh"`.
     - Dev directly on dev: `ssh -p 10022 root@192.168.2.126 "cd /root/src/auto_sync && git pull --ff-only && scripts/deploy_local.sh --install-dir /usr/local/auto_sync"`.
     If parallel execution is not available, run the same three commands sequentially.
- Do not deploy to tiger and do not use Windows-to-Linux cross-compilation for the normal NAS deployment path; build Linux x64 binaries on NAS.
- Always deploy through the existing project scripts when deployment is explicitly requested, never with ad-hoc commands. On Windows use `scripts/deploy_windows.ps1`; on NAS use `scripts/deploy_nas.sh`. Do not run ad-hoc `cargo build`/`cargo check` against other targets (e.g. `--target *-linux-*`) on Windows to "validate" Linux-only code; cross-target builds pull in unbuildable native deps (libdbus, etc.). Validate Linux/`cfg`-gated code on the relevant Linux host when needed.
- For normal NAS deployment, do not delete `/opt/usr/local/auto_sync`, do not run `git reset --hard`, and do not clean `/opt/usr/local/auto_sync/target`; preserving Cargo build cache is required. If tracked files in `/opt/usr/local/auto_sync` are dirty, stop and resolve why instead of masking it with reset.
