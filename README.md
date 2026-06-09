# auto_sync

Linux-only Rust directory sync tool.

The GUI is implemented with Tauri and uses WebKitGTK on Linux.

## Build

```bash
cargo build --release
install -m 0755 target/release/auto_syncd bin/auto_syncd
install -m 0755 target/release/auto_syncctl bin/auto_syncctl
install -m 0755 target/release/auto_sync_gui bin/auto_sync_gui
```

## Run

```bash
bin/auto_sync_gui --config conf/auto_sync.toml
bin/auto_syncd --config conf/auto_sync.toml
bin/auto_syncctl --config conf/auto_sync.toml status
bin/auto_syncctl --config conf/auto_sync.toml sync-now --close-current
```

NAS deploy helper:

```bash
bin/auto_syncctl --config conf/auto_sync.toml deploy-nas \
  --host 192.168.3.178 --port 10022 --user root --install-dir /opt/auto_sync
```
