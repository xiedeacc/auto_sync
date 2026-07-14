# Project Rules

- For every local code-change loop, do not stop or kill existing `auto_sync` processes unless the user explicitly asks for it.
- For local code-change validation, run an incremental debug build when appropriate: `cargo build --bins`.
- On Windows, before starting any `auto_sync` process, copy the freshly compiled binaries into `D:\code\auto_sync\bin` and start them only from that `bin\` directory, never directly from `target\debug\` or `target\release\`.
- Use the Windows deploy script for full local deployment: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`. On Windows, deploy binaries into the repository `bin\` directory and use a current-user Startup launcher for both `auto_syncd` and `auto_sync_gui`; always start both from `bin\` after deployment, and do not install or start `auto_syncd` as a Windows service.
- Unless the user explicitly says not to commit or push, every completed code/config change must be committed and pushed.
- Do not deploy unless the user explicitly asks to deploy to all three platforms. A code/config change request by itself is not a deployment request; a generic deployment request is not enough unless it clearly names Windows, NAS, and dev or otherwise says all three platforms.
- `auto_sync` is only deployed to Windows, NAS, and dev. Do not add or keep normal `auto_sync` deployment paths for AWS, OpenWrt, or other hosts unless the user explicitly asks for a special one-off.
- TBox is only deployed and started on NAS. Other hosts must keep TBox services disabled/stopped if their deployment scripts touch them.
- Waiwei and Xray are disabled/stopped on every host. Waiwei only has `waiwei-web` and `waiwei-puller`; there must not be a standalone `waiwei` service. Deployment scripts may preserve their files for backup/round-trip purposes, but must not enable or start `waiwei-web`, `waiwei-puller`, `xray`, or related init/systemd units.
- When validating or running collector deployment scripts, execute the target `conf/collector_deploy_*.ps1` script directly with the required `AS_*` environment variables. Do not compile, restart, or deploy the `auto_sync` application itself just to run a collector deployment script unless the user explicitly asks for that.
- Every commit must include all current repository changes. Do not leave tracked working-tree changes unstaged unless the user explicitly asks to keep them out of the commit.
- Deploy scripts must preserve existing `conf/auto_sync.toml`; only initialize it from a template when the target config file does not exist.
- Dev and NAS intentionally use different real paths because their disks are different:
  - Dev has a large root SSD. Keep dev `auto_sync` source at `/root/src/rust/auto_sync` and deployment/runtime at `/usr/local/auto_sync`; Immich at `/usr/local/immich`; shared tooling and source repos under `/root/src/software`; OpenWrt toolchains under `/root/src/software/openwrt`; and `/root` plus `/home/tiger` as real local paths. Do not create or rely on `/root/src/auto_sync`, `/opt/usr/local/auto_sync`, `/opt/immich`, `/opt/user/root`, or `/opt/user/tiger` on dev.
  - NAS has a small root disk. Keep NAS `auto_sync` source at `/opt/src/rust/auto_sync` and deployment/runtime at `/opt/usr/local/auto_sync`; Flutter SDK at `/opt/src/software/flutter`; NAS Immich at `/opt/immich`; and NAS home-directory spillover/symlink targets under `/opt/user/{root,tiger}` when needed. On NAS, every `/root` child except `/root/.ssh` must be a symlink into `/opt/user/root/`. Do not flatten NAS `/opt` layout to match dev. Do not put the `auto_sync` git checkout in `/opt/usr/local/auto_sync`, and do not bind-mount `/opt/usr/local` back onto `/usr/local`; old `/usr/local/{blog,go,halo,shadowsocks,tbox,waiwei,xray,bin}` entries must disappear by removing the bind mount, not by deleting data through the old path.
  - Keep `docs/deployment-paths.md` current whenever deployment paths, collector paths, systemd units, or toolchain locations change.
  - NAS GitLab/repository data that is configured for `/zfs` must stay on `/zfs`; make sure the ZFS pool/disk is available before touching that data.
- Any user-visible Windows path must be rendered through the standard display-path helper (`displayPath` in web UI, `_displayPath` in Flutter UI, or an equivalent backend helper) so extended-length prefixes such as `\\?\` and `\\?\UNC\` are never shown. Keep the underlying stored path unchanged.
- When the user explicitly asks to deploy to all three platforms, always use this update path:
  1. On Windows, commit all intended repository changes and push them.
  2. After the push succeeds, deploy this Windows machine, NAS, and dev in parallel when possible:
     - Windows: `pwsh -ExecutionPolicy Bypass -File scripts/deploy_windows.ps1`.
     - NAS directly on NAS: `ssh -p 10022 root@192.168.2.247 "cd /opt/src/rust/auto_sync && git pull --ff-only && scripts/deploy_nas.sh"`.
     - Dev directly on dev: `ssh -p 10022 root@192.168.2.126 "cd /root/src/rust/auto_sync && git pull --ff-only && scripts/deploy_local.sh --install-dir /usr/local/auto_sync"`.
     If parallel execution is not available, run the same three commands sequentially.
- Do not deploy to tiger and do not use Windows-to-Linux cross-compilation for the normal NAS deployment path; build Linux x64 binaries on NAS.
- For normal NAS deployment, do not delete `/opt/usr/local/auto_sync` runtime data and do not clean `/opt/src/rust/auto_sync/target`; preserving Cargo build cache is required. If tracked files in `/opt/src/rust/auto_sync` are dirty, stop and resolve why instead of masking it with reset.
