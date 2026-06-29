// End-to-end test of the HTTP transfer protocol (bulk dir-create, parallel
// file push, small-file put-file fast path, chunked large-file path, and mirror
// deletes) without requiring the NAS. A receiver web server is started
// in-process on localhost and the sync engine pushes a generated tree to it.

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use auto_sync::core::backend::Backend;
use auto_sync::core::config::{
    AppConfig, DestinationConfig, MachineConfig, ScheduleConfig, SnapshotBackend, SnapshotConfig,
    SourceGroupConfig, SyncMode,
};
use auto_sync::core::state::State;
use auto_sync::core::sync::{SyncRequestMode, sync_destination_now_with_mode};

const SOURCE_ID: &str = "local_http_e2e";
const DESTINATION_ID: &str = "localweb_dst";
const SOURCE_DIR_NAME: &str = "auto_sync_http_src";

#[test]
fn full_sync_over_http_transfer_protocol() -> Result<()> {
    let base = unique_temp_dir("auto_sync_http_e2e");
    let _cleanup = Cleanup(base.clone());
    let source_root = base.join(SOURCE_DIR_NAME);
    let dest_parent = base.join("dest");
    let state_db = base.join("state.sqlite");
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&dest_parent)?;

    // Build a tree that exercises every transfer path.
    write_bytes(source_root.join("root.txt"), b"root file\n")?; // small -> put-file
    write_bytes(source_root.join("nested/a/b/deep.txt"), b"deep\n")?; // bulk dirs
    write_bytes(source_root.join("space dir/with space.txt"), b"spaces\n")?;
    fs::create_dir_all(source_root.join("empty_dir"))?; // empty dir preserved
    let big = vec![7_u8; 10 * 1024 * 1024]; // 10 MiB -> chunked path (>4 MiB)
    write_bytes(source_root.join("media/big.bin"), &big)?;
    for i in 0..200 {
        write_bytes(source_root.join(format!("many/file_{i:03}.dat")), b"x")?; // parallel small files
    }

    // The mirrored destination root gets a stale extra file that must be deleted.
    let dest_root = dest_parent.join(SOURCE_DIR_NAME);
    write_bytes(dest_root.join("stale/extra.txt"), b"delete me\n")?;

    let server = start_receiver(&base)?;
    let cfg = build_config(&state_db, &source_root, &dest_parent, server.port);

    let mut state = State::open(&state_db)?;
    let started = Instant::now();
    sync_destination_now_with_mode(
        &cfg,
        &mut state,
        SOURCE_ID,
        DESTINATION_ID,
        SyncRequestMode::Full,
    )
    .context("full sync over HTTP transfer failed")?;
    eprintln!("sync completed in {} ms", started.elapsed().as_millis());

    // Destination must verify green.
    let view = state
        .destination_views(&cfg)?
        .into_iter()
        .find(|v| v.destination_id == DESTINATION_ID)
        .context("missing destination view")?;
    assert_eq!(view.status, "green", "status_reason={}", view.status_reason);

    assert_file(&dest_root.join("root.txt"), b"root file\n")?;
    assert_file(&dest_root.join("nested/a/b/deep.txt"), b"deep\n")?;
    assert_file(&dest_root.join("space dir/with space.txt"), b"spaces\n")?;
    assert!(dest_root.join("empty_dir").is_dir(), "empty dir missing");
    assert_size(&dest_root.join("media/big.bin"), big.len() as u64)?;
    assert_file(&big_path(&dest_root), &big)?;
    for i in 0..200 {
        assert_file(&dest_root.join(format!("many/file_{i:03}.dat")), b"x")?;
    }
    assert!(
        !dest_root.join("stale/extra.txt").exists(),
        "mirror should have deleted stale extra file"
    );

    // Second sync should be a no-op (idempotent): no temp leftovers, still green.
    sync_destination_now_with_mode(
        &cfg,
        &mut state,
        SOURCE_ID,
        DESTINATION_ID,
        SyncRequestMode::Full,
    )
    .context("second full sync failed")?;
    assert!(
        !dest_root.join(".auto_sync_tmp").exists(),
        "temp directory should be cleaned up after sync"
    );

    Ok(())
}

fn big_path(dest_root: &Path) -> PathBuf {
    dest_root.join("media/big.bin")
}

#[test]
fn changed_large_file_resyncs_via_delta() -> Result<()> {
    let base = unique_temp_dir("auto_sync_delta_e2e");
    let _cleanup = Cleanup(base.clone());
    let source_root = base.join(SOURCE_DIR_NAME);
    let dest_parent = base.join("dest");
    let state_db = base.join("state.sqlite");
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&dest_parent)?;

    // 2 MiB file -> first sync transfers it whole, establishing a delta basis.
    let v1: Vec<u8> = (0..2_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    write_bytes(source_root.join("db/data.bin"), &v1)?;

    let server = start_receiver(&base)?;
    let cfg = build_config(&state_db, &source_root, &dest_parent, server.port);
    let mut state = State::open(&state_db)?;
    sync_destination_now_with_mode(
        &cfg,
        &mut state,
        SOURCE_ID,
        DESTINATION_ID,
        SyncRequestMode::Full,
    )?;

    let dest_root = dest_parent.join(SOURCE_DIR_NAME);
    assert_file(&dest_root.join("db/data.bin"), &v1)?;

    // Edit a middle region and grow the file: the size change forces a re-copy,
    // which goes through the delta path because the basis already exists.
    let mut v2 = v1.clone();
    for b in v2.iter_mut().skip(900_000).take(2_000) {
        *b = b'Q';
    }
    v2.extend(std::iter::repeat(b'Z').take(300_000));
    write_bytes(source_root.join("db/data.bin"), &v2)?;

    sync_destination_now_with_mode(
        &cfg,
        &mut state,
        SOURCE_ID,
        DESTINATION_ID,
        SyncRequestMode::Full,
    )?;
    assert_file(&dest_root.join("db/data.bin"), &v2)?;

    let view = state
        .destination_views(&cfg)?
        .into_iter()
        .find(|v| v.destination_id == DESTINATION_ID)
        .context("missing destination view")?;
    assert_eq!(view.status, "green", "status_reason={}", view.status_reason);
    Ok(())
}

struct Receiver {
    port: u16,
}

fn start_receiver(base: &Path) -> Result<Receiver> {
    // Backend needs a config path; the transfer endpoints do not use it.
    let cfg_path = base.join("receiver.toml");
    fs::write(&cfg_path, "")?;
    let backend = Backend::new(cfg_path, 0);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind receiver");
            let port = listener.local_addr().expect("local addr").port();
            tx.send(port).expect("send port");
            let app = auto_sync::core::web_api::router(backend);
            axum::serve(listener, app).await.expect("serve");
        });
    });
    let port = rx.recv_timeout(Duration::from_secs(5))?;
    // Wait until the port accepts connections.
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(Receiver { port });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("receiver did not start on port {port}")
}

fn build_config(state_db: &Path, source_root: &Path, dest_parent: &Path, port: u16) -> AppConfig {
    let os = std::env::consts::OS.to_string();
    let mut local = MachineConfig::local();
    local.id = "local".to_string();
    local.os = os.clone();

    let receiver = MachineConfig {
        id: "localweb".to_string(),
        alias_name: String::new(),
        name: "localweb".to_string(),
        host: "127.0.0.1".to_string(),
        web_port: port,
        ssh_user: String::new(),
        ssh_port: 22,
        os,
        enabled: true,
        manual: true,
    };

    let mut cfg = AppConfig::default();
    cfg.app.data_db = state_db.to_path_buf();
    cfg.machines = vec![local, receiver];
    cfg.source_groups = vec![SourceGroupConfig {
        id: SOURCE_ID.to_string(),
        machine_id: "local".to_string(),
        src: source_root.to_path_buf(),
        excludes: Vec::new(),
        enabled: true,
        mode: SyncMode::Mirror,
        snapshot: SnapshotConfig {
            backend: SnapshotBackend::Manifest,
            ..SnapshotConfig::default()
        },
        destinations: vec![DestinationConfig {
            id: DESTINATION_ID.to_string(),
            machine_id: "localweb".to_string(),
            path: dest_parent.to_path_buf(),
            enabled: true,
            schedule: ScheduleConfig::default(),
            sync: None,
        }],
    }];
    cfg
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}_{}_{nanos}", std::process::id()))
}

fn write_bytes(path: PathBuf, value: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, value)?;
    Ok(())
}

fn assert_file(path: &Path, expected: &[u8]) -> Result<()> {
    let actual = fs::read(path).with_context(|| format!("missing {}", path.display()))?;
    if actual != expected {
        bail!(
            "content mismatch at {} (len {} vs {})",
            path.display(),
            actual.len(),
            expected.len()
        );
    }
    Ok(())
}

fn assert_size(path: &Path, expected: u64) -> Result<()> {
    let actual = fs::metadata(path)
        .with_context(|| format!("missing {}", path.display()))?
        .len();
    if actual != expected {
        bail!(
            "size mismatch at {}: {actual} vs {expected}",
            path.display()
        );
    }
    Ok(())
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
