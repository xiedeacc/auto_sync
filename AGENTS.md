# Project Rules

- For every local code-change loop, stop old `auto_sync` processes first:
  `auto_syncd`, `auto_syncctl`, `auto_sync_web`, and `auto_sync_gui`.
- After stopping old processes, run an incremental debug build: `cargo build --bins`.
- On Windows, before starting any `auto_sync` process, copy the freshly compiled binaries into `D:\code\auto_sync\bin` and start them only from that `bin\` directory, never directly from `target\debug\` or `target\release\`.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`. On Windows, deploy binaries into the repository `bin\` directory and use a current-user Startup launcher for both `auto_syncd` and `auto_sync_gui`; always start both from `bin\` after deployment, and do not install or start `auto_syncd` as a Windows service.
- Unless the user explicitly says not to commit or push, every completed code/config change must be committed and pushed.
- Do not deploy unless the user explicitly asks for deployment. A code/config change request by itself is not a deployment request.
- When validating or running collector deployment scripts, execute the target `conf/collector_deploy_*.ps1` script directly with the required `AS_*` environment variables. Do not compile, restart, or deploy the `auto_sync` application itself just to run a collector deployment script unless the user explicitly asks for that.
- Every commit must include all current repository changes. Do not leave tracked working-tree changes unstaged unless the user explicitly asks to keep them out of the commit.
- Deploy scripts must preserve existing `conf/auto_sync.toml`; only initialize it from a template when the target config file does not exist.
- Any user-visible Windows path must be rendered through the standard display-path helper (`displayPath` in web UI, `_displayPath` in Flutter UI, or an equivalent backend helper) so extended-length prefixes such as `\\?\` and `\\?\UNC\` are never shown. Keep the underlying stored path unchanged.
- When the user explicitly asks to deploy, always use this update path:
  1. On Windows, commit all intended repository changes and push them.
  2. After the push succeeds, deploy this Windows machine and NAS in parallel when possible:
     - Windows: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
     - NAS directly on NAS: `ssh -p 10022 root@192.168.2.247 "cd /opt/auto_sync && git pull --ff-only && scripts/deploy_nas.sh"`.
     If parallel execution is not available, run the same two commands sequentially.
- Do not deploy to tiger and do not use Windows-to-Linux cross-compilation for the normal NAS deployment path; build Linux x64 binaries on NAS.
- For normal NAS deployment, do not delete `/opt/auto_sync`, do not run `git reset --hard`, and do not clean `/opt/auto_sync/target`; preserving Cargo build cache is required. If tracked files in `/opt/auto_sync` are dirty, stop and resolve why instead of masking it with reset.
