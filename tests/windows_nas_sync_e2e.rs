use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use auto_sync::core::config::{
    AppConfig, DestinationConfig, MachineConfig, ScheduleConfig, SnapshotBackend, SnapshotConfig,
    SourceGroupConfig, SyncMode,
};
use auto_sync::core::state::State;
use auto_sync::core::sync::{SyncRequestMode, sync_destination_now_with_mode};

const SOURCE_ID: &str = "windows_to_nas_e2e";
const DESTINATION_ID: &str = "nas_dst";

static E2E_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[test]
#[ignore = "requires Windows controller, SSH to NAS, and NAS auto_sync service"]
fn full_sync_windows_to_nas_mirrors_tree() -> Result<()> {
    let _guard = e2e_lock().lock().unwrap();
    let env = TestEnv::reset("full_sync_windows_to_nas_mirrors_tree")?;

    write_text(env.local_root.join("hello.txt"), "hello from windows\n")?;
    write_text(env.local_root.join("nested/child.txt"), "nested child\n")?;
    write_text(
        env.local_root.join("space dir/file with spaces.txt"),
        "space path\n",
    )?;
    fs::create_dir_all(env.local_root.join("empty_dir"))?;
    write_bytes(
        env.local_root.join("binary.bin"),
        &(0_u8..=255).collect::<Vec<_>>(),
    )?;

    env.remote_sh(&format!(
        "mkdir -p {dst}/old_dir && printf stale > {dst}/extra.txt && printf old > {dst}/old_dir/old.txt",
        dst = shell_quote(&env.remote_dest_root)
    ))?;

    env.sync(SyncRequestMode::Full)?;

    env.assert_remote_file("hello.txt", "hello from windows\n")?;
    env.assert_remote_file("nested/child.txt", "nested child\n")?;
    env.assert_remote_file("space dir/file with spaces.txt", "space path\n")?;
    env.assert_remote_dir("empty_dir")?;
    env.assert_remote_size("binary.bin", 256)?;
    env.assert_remote_missing("extra.txt")?;
    env.assert_remote_missing("old_dir/old.txt")?;
    Ok(())
}

#[test]
#[ignore = "requires Windows controller, SSH to NAS, and NAS auto_sync service"]
fn incremental_sync_windows_to_nas_handles_common_changes() -> Result<()> {
    let _guard = e2e_lock().lock().unwrap();
    let env = TestEnv::reset("incremental_sync_windows_to_nas_handles_common_changes")?;

    write_text(env.local_root.join("keep.txt"), "v1\n")?;
    write_text(env.local_root.join("delete.txt"), "delete me\n")?;
    write_text(env.local_root.join("rename_old.txt"), "rename me\n")?;
    write_text(
        env.local_root.join("dir_old/file.txt"),
        "dir before rename\n",
    )?;
    env.sync(SyncRequestMode::Full)?;

    write_text(env.local_root.join("keep.txt"), "v2\n")?;
    fs::remove_file(env.local_root.join("delete.txt"))?;
    fs::rename(
        env.local_root.join("rename_old.txt"),
        env.local_root.join("rename_new.txt"),
    )?;
    fs::rename(
        env.local_root.join("dir_old"),
        env.local_root.join("dir_new"),
    )?;
    write_text(env.local_root.join("created/new.txt"), "created\n")?;
    fs::create_dir_all(env.local_root.join("created/empty"))?;
    write_bytes(env.local_root.join("created/blob.bin"), &[0, 1, 2, 3, 4])?;

    env.sync(SyncRequestMode::Incremental)?;

    env.assert_remote_file("keep.txt", "v2\n")?;
    env.assert_remote_missing("delete.txt")?;
    env.assert_remote_missing("rename_old.txt")?;
    env.assert_remote_file("rename_new.txt", "rename me\n")?;
    env.assert_remote_missing("dir_old/file.txt")?;
    env.assert_remote_file("dir_new/file.txt", "dir before rename\n")?;
    env.assert_remote_file("created/new.txt", "created\n")?;
    env.assert_remote_dir("created/empty")?;
    env.assert_remote_size("created/blob.bin", 5)?;
    Ok(())
}

#[test]
#[ignore = "requires Windows controller, SSH to NAS, and NAS auto_sync service"]
fn incremental_sync_windows_to_nas_replaces_file_and_directory_shapes() -> Result<()> {
    let _guard = e2e_lock().lock().unwrap();
    let env = TestEnv::reset("incremental_sync_windows_to_nas_replaces_file_and_directory_shapes")?;

    write_text(env.local_root.join("path_shape"), "file first\n")?;
    write_text(env.local_root.join("folder_shape/old.txt"), "dir first\n")?;
    env.sync(SyncRequestMode::Full)?;

    fs::remove_file(env.local_root.join("path_shape"))?;
    write_text(
        env.local_root.join("path_shape/child.txt"),
        "now directory\n",
    )?;
    fs::remove_dir_all(env.local_root.join("folder_shape"))?;
    write_text(env.local_root.join("folder_shape"), "now file\n")?;

    env.sync(SyncRequestMode::Incremental)?;

    env.assert_remote_file("path_shape/child.txt", "now directory\n")?;
    env.assert_remote_file("folder_shape", "now file\n")?;
    env.assert_remote_missing("folder_shape/old.txt")?;
    Ok(())
}

fn e2e_lock() -> &'static Mutex<()> {
    E2E_LOCK.get_or_init(|| Mutex::new(()))
}

struct TestEnv {
    local_root: PathBuf,
    state_db: PathBuf,
    nas_host: String,
    nas_port: u16,
    nas_user: String,
    nas_api_port: u16,
    remote_base: String,
    remote_dest_root: String,
}

impl TestEnv {
    fn reset(test_name: &str) -> Result<Self> {
        let local_root = PathBuf::from(
            std::env::var("AUTO_SYNC_E2E_WINDOWS_ROOT")
                .unwrap_or_else(|_| r"C:\Users\tiger\auto_sync_test".to_string()),
        );
        guard_local_root(&local_root)?;

        let nas_host =
            std::env::var("AUTO_SYNC_E2E_NAS_HOST").unwrap_or_else(|_| "192.168.2.247".to_string());
        let nas_port = parse_env_u16("AUTO_SYNC_E2E_NAS_PORT", 10022)?;
        let nas_user =
            std::env::var("AUTO_SYNC_E2E_NAS_USER").unwrap_or_else(|_| "root".to_string());
        let nas_api_port = parse_env_u16("AUTO_SYNC_E2E_NAS_API_PORT", 18765)?;
        let remote_base = trim_remote_path(
            &std::env::var("AUTO_SYNC_E2E_NAS_ROOT").unwrap_or_else(|_| "/zfs/tmp".to_string()),
        );
        let source_name = local_root
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("local source root has no file name"))?;
        let remote_dest_root = format!("{remote_base}/{source_name}");
        guard_remote_dest(&remote_dest_root)?;

        let state_db = std::env::temp_dir().join(format!("auto_sync_{test_name}.sqlite"));
        remove_file_if_exists(&state_db)?;
        remove_dir_if_exists(&local_root)?;
        fs::create_dir_all(&local_root)
            .with_context(|| format!("failed to create {}", local_root.display()))?;

        let env = Self {
            local_root,
            state_db,
            nas_host,
            nas_port,
            nas_user,
            nas_api_port,
            remote_base,
            remote_dest_root,
        };
        env.remote_sh(&format!(
            "rm -rf {dst} && mkdir -p {base}",
            dst = shell_quote(&env.remote_dest_root),
            base = shell_quote(&env.remote_base)
        ))?;
        env.preflight()?;
        Ok(env)
    }

    fn sync(&self, mode: SyncRequestMode) -> Result<()> {
        let cfg = self.config();
        let mut state = State::open(&self.state_db)?;
        sync_destination_now_with_mode(&cfg, &mut state, SOURCE_ID, DESTINATION_ID, mode)
    }

    fn config(&self) -> AppConfig {
        let mut local = MachineConfig::local();
        local.id = "local".to_string();
        local.name = "This machine".to_string();
        local.os = "windows".to_string();

        let nas = MachineConfig {
            id: "nas".to_string(),
            alias_name: "nas".to_string(),
            name: "nas".to_string(),
            host: self.nas_host.clone(),
            port: self.nas_api_port,
            ssh_user: self.nas_user.clone(),
            ssh_port: self.nas_port,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        };

        let mut cfg = AppConfig::default();
        cfg.app.data_db = self.state_db.clone();
        cfg.machines = vec![local, nas];
        cfg.source_groups = vec![SourceGroupConfig {
            id: SOURCE_ID.to_string(),
            machine_id: "local".to_string(),
            src: self.local_root.clone(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: DESTINATION_ID.to_string(),
                machine_id: "nas".to_string(),
                path: PathBuf::from(&self.remote_base),
                enabled: true,
                schedule: ScheduleConfig::default(),
                sync: None,
            }],
        }];
        cfg
    }

    fn preflight(&self) -> Result<()> {
        self.remote_sh("command -v ssh >/dev/null")?;
        let addr = format!("{}:{}", self.nas_host, self.nas_api_port);
        let socket = addr.parse()?;
        std::net::TcpStream::connect_timeout(&socket, Duration::from_secs(3))
            .with_context(|| format!("failed to connect to NAS auto_sync service at {addr}"))?;
        Ok(())
    }

    fn remote_sh(&self, script: &str) -> Result<String> {
        let target = format!("{}@{}", self.nas_user, self.nas_host);
        let output = Command::new("ssh")
            .arg("-p")
            .arg(self.nas_port.to_string())
            .arg(target)
            .arg(script)
            .output()
            .with_context(|| format!("failed to run remote command: {script}"))?;
        if !output.status.success() {
            bail!(
                "remote command failed with status {}: {}\n{}",
                output.status,
                script,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn assert_remote_file(&self, rel: &str, expected: &str) -> Result<()> {
        let path = self.remote_path(rel);
        let actual = self.remote_sh(&format!("cat {}", shell_quote(&path)))?;
        if actual != expected {
            bail!("remote file {path} mismatch: expected {expected:?}, got {actual:?}");
        }
        Ok(())
    }

    fn assert_remote_size(&self, rel: &str, expected: u64) -> Result<()> {
        let path = self.remote_path(rel);
        let actual = self
            .remote_sh(&format!("wc -c < {}", shell_quote(&path)))?
            .trim()
            .parse::<u64>()?;
        if actual != expected {
            bail!("remote file {path} size mismatch: expected {expected}, got {actual}");
        }
        Ok(())
    }

    fn assert_remote_dir(&self, rel: &str) -> Result<()> {
        let path = self.remote_path(rel);
        self.remote_sh(&format!("test -d {}", shell_quote(&path)))?;
        Ok(())
    }

    fn assert_remote_missing(&self, rel: &str) -> Result<()> {
        let path = self.remote_path(rel);
        self.remote_sh(&format!("test ! -e {}", shell_quote(&path)))?;
        Ok(())
    }

    fn remote_path(&self, rel: &str) -> String {
        format!(
            "{}/{}",
            self.remote_dest_root.trim_end_matches('/'),
            rel.trim_start_matches('/')
        )
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = remove_dir_if_exists(&self.local_root);
        let _ = remove_file_if_exists(&self.state_db);
        let _ = self.remote_sh(&format!("rm -rf {}", shell_quote(&self.remote_dest_root)));
    }
}

fn write_text(path: PathBuf, value: &str) -> Result<()> {
    write_bytes(path, value.as_bytes())
}

fn write_bytes(path: PathBuf, value: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, value)?;
    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn guard_local_root(path: &Path) -> Result<()> {
    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    if !normalized.ends_with(r"\auto_sync_test") || !normalized.starts_with(r"c:\users\tiger\") {
        bail!(
            "refusing to reset unexpected Windows test root: {}",
            path.display()
        );
    }
    Ok(())
}

fn guard_remote_dest(path: &str) -> Result<()> {
    if !path.starts_with("/zfs/tmp/") || !path.ends_with("/auto_sync_test") {
        bail!("refusing to reset unexpected NAS test root: {path}");
    }
    Ok(())
}

fn trim_remote_path(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn parse_env_u16(name: &str, default: u16) -> Result<u16> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u16>()
            .with_context(|| format!("invalid {name}: {value}")),
        Err(_) => Ok(default),
    }
}
