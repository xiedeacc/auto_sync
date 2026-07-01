use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::mem;
use std::os::fd::RawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

use crate::core::config::{AppConfig, SourceGroupConfig, machine_id_or_local};
use crate::core::state::State;

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
const FAN_REPORT_FID: u32 = 0x0000_0200;
const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
const FAN_REPORT_NAME: u32 = 0x0000_0800;
const FAN_REPORT_TARGET_FID: u32 = 0x0000_1000;
const FAN_MARK_ADD: u32 = 0x0000_0001;
const FAN_MARK_ONLYDIR: u32 = 0x0000_0008;
const FAN_MARK_MOUNT: u32 = 0x0000_0010;
const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;

const FAN_MODIFY: u64 = 0x0000_0002;
const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
const FAN_MOVED_FROM: u64 = 0x0000_0040;
const FAN_MOVED_TO: u64 = 0x0000_0080;
const FAN_CREATE: u64 = 0x0000_0100;
const FAN_DELETE: u64 = 0x0000_0200;
const FAN_DELETE_SELF: u64 = 0x0000_0400;
const FAN_MOVE_SELF: u64 = 0x0000_0800;
const FAN_Q_OVERFLOW: u64 = 0x0000_4000;
const FAN_EVENT_ON_CHILD: u64 = 0x0800_0000;
const FAN_ONDIR: u64 = 0x4000_0000;
const FAN_NOFD: i32 = -1;
const FANOTIFY_METADATA_VERSION: u8 = 3;
const FAN_EVENT_INFO_TYPE_FID: u8 = 1;
const FAN_EVENT_INFO_TYPE_DFID_NAME: u8 = 2;
const FAN_EVENT_INFO_TYPE_DFID: u8 = 3;
const FAN_EVENT_INFO_TYPE_OLD_DFID_NAME: u8 = 10;
const FAN_EVENT_INFO_TYPE_NEW_DFID_NAME: u8 = 12;
const MAX_HANDLE_SZ: usize = 128;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FanotifyEventMetadata {
    event_len: u32,
    vers: u8,
    reserved: u8,
    metadata_len: u16,
    mask: u64,
    fd: i32,
    pid: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FanotifyEventInfoHeader {
    info_type: u8,
    pad: u8,
    len: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FileHandleHeader {
    handle_bytes: u32,
    handle_type: i32,
}

#[derive(Debug, Clone)]
struct SourceRoot {
    id: String,
    root: PathBuf,
    is_file: bool,
    handle_paths: HashMap<Vec<u8>, PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum FanotifyMode {
    FidName,
    FdPath,
}

pub fn spawn_fanotify_thread(
    cfg: AppConfig,
    db_path: PathBuf,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let sources: Vec<_> = cfg
            .source_groups
            .iter()
            .filter(|source| source_needs_fanotify(source))
            .cloned()
            .collect();
        if sources.is_empty() {
            info!("fanotify watcher has no realtime sources");
            while !shutdown.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
            }
            return;
        }

        let mut handles = Vec::new();
        for source in sources {
            let mut source_cfg = cfg.clone();
            source_cfg.source_groups = vec![source];
            let db_path = db_path.clone();
            let shutdown = shutdown.clone();
            handles.push(thread::spawn(move || {
                // Supervise: a watcher that errors out (read error, mark/setup
                // failure) is restarted after a short backoff instead of dying
                // silently for the lifetime of the process.
                while !shutdown.load(Ordering::SeqCst) {
                    match run_fanotify_loop(source_cfg.clone(), db_path.clone(), shutdown.clone()) {
                        Ok(()) => break,
                        Err(err) => {
                            error!(error = %err, "fanotify source watcher stopped; restarting after backoff");
                            for _ in 0..50 {
                                if shutdown.load(Ordering::SeqCst) {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }
                }
            }));
        }

        while !shutdown.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
        }
        for handle in handles {
            if let Err(err) = handle.join() {
                warn!(?err, "fanotify source watcher join failed");
            }
        }
    })
}

pub fn spawn_source_watcher_thread(
    cfg: AppConfig,
    db_path: PathBuf,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    spawn_fanotify_thread(cfg, db_path, shutdown)
}

fn source_needs_fanotify(source: &SourceGroupConfig) -> bool {
    source.enabled
        && machine_id_or_local(&source.machine_id) == "local"
        && source
            .destinations
            .iter()
            .any(|dst| dst.enabled)
}

fn run_fanotify_loop(cfg: AppConfig, db_path: PathBuf, shutdown: Arc<AtomicBool>) -> Result<()> {
    let mut sources = source_roots(&cfg)?;
    if sources.is_empty() {
        info!("fanotify watcher has no enabled sources");
        while !shutdown.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
        }
        return Ok(());
    }

    let state = State::open(&db_path)?;
    state.ensure_config(&cfg)?;
    state.ensure_open_cycles(&cfg)?;

    let fid_mask = FAN_MODIFY
        | FAN_CLOSE_WRITE
        | FAN_CREATE
        | FAN_DELETE
        | FAN_MOVED_FROM
        | FAN_MOVED_TO
        | FAN_DELETE_SELF
        | FAN_MOVE_SELF
        | FAN_ONDIR;
    let (fd, mode, mask) = match setup_fid_name_fanotify(&sources, fid_mask) {
        Ok(fd) => {
            info!("fanotify FID/name watcher enabled");
            (fd, FanotifyMode::FidName, fid_mask)
        }
        Err(err) => {
            warn!(
                error = %err,
                "fanotify FID/name watcher unavailable; falling back to fd path watcher"
            );
            let fd = setup_fd_path_fanotify(&sources)?;
            (fd, FanotifyMode::FdPath, FAN_MODIFY | FAN_CLOSE_WRITE)
        }
    };
    let _guard = FdGuard(fd);

    let mut buf = vec![0_u8; 1024 * 64];
    while !shutdown.load(Ordering::SeqCst) {
        let n = unsafe {
            libc::read(
                fd,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len() as libc::size_t,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            return Err(err).context("failed to read fanotify fd");
        }
        if n == 0 {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        parse_events(&state, &mut sources, fd, mode, mask, &buf[..n as usize])?;
    }
    Ok(())
}

fn setup_fid_name_fanotify(sources: &[SourceRoot], mask: u64) -> Result<RawFd> {
    let fd = fanotify_init(
        FAN_REPORT_DIR_FID | FAN_REPORT_NAME | FAN_REPORT_FID | FAN_REPORT_TARGET_FID,
    )
    .context("fanotify_init FID/name mode failed")?;
    if let Err(err) = mark_sources_fid(fd, sources, mask) {
        close_event_fd(fd);
        return Err(err);
    }
    Ok(fd)
}

fn setup_fd_path_fanotify(sources: &[SourceRoot]) -> Result<RawFd> {
    let fd = fanotify_init(0).context("fanotify_init fd path mode failed")?;
    let mask = FAN_MODIFY | FAN_CLOSE_WRITE;
    if let Err(err) = mark_sources_fd(fd, sources, mask) {
        close_event_fd(fd);
        return Err(err);
    }
    Ok(fd)
}

fn fanotify_init(report_flags: u32) -> Result<RawFd> {
    let flags = FAN_CLOEXEC | FAN_NONBLOCK | FAN_CLASS_NOTIF | report_flags;
    let event_f_flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_LARGEFILE;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_fanotify_init,
            flags as libc::c_uint,
            event_f_flags as libc::c_uint,
        ) as RawFd
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("fanotify_init failed");
    }
    Ok(fd)
}

fn mark_sources_fid(fd: RawFd, sources: &[SourceRoot], mask: u64) -> Result<()> {
    for source in sources {
        mark_source_fid(fd, source, mask)
            .with_context(|| format!("failed to mark source {}", source.root.display()))?;
        info!(source = source.id, root = %source.root.display(), "fanotify FID/name mark registered");
    }
    Ok(())
}

fn mark_source_fid(fd: RawFd, source: &SourceRoot, mask: u64) -> Result<()> {
    let root = &source.root;
    if try_mark(fd, root, FAN_MARK_ADD | FAN_MARK_FILESYSTEM, mask).is_ok() {
        return Ok(());
    }
    let fs_err = std::io::Error::last_os_error();
    if source.is_file {
        warn!(
            path = %root.display(),
            error = %fs_err,
            "fanotify FID/name filesystem mark failed; trying file mark"
        );
        return try_mark(fd, root, FAN_MARK_ADD, mask);
    }

    warn!(
        path = %root.display(),
        error = %fs_err,
        "fanotify FID/name filesystem mark failed; trying recursive directory marks"
    );
    mark_directory_tree(fd, root, mask | FAN_EVENT_ON_CHILD)
}

fn mark_sources_fd(fd: RawFd, sources: &[SourceRoot], mask: u64) -> Result<()> {
    for source in sources {
        mark_source_fd(fd, source, mask)
            .with_context(|| format!("failed to mark source {}", source.root.display()))?;
        info!(source = source.id, root = %source.root.display(), "fanotify fd path mark registered");
    }
    Ok(())
}

fn mark_source_fd(fd: RawFd, source: &SourceRoot, mask: u64) -> Result<()> {
    let root = &source.root;
    if try_mark(fd, root, FAN_MARK_ADD | FAN_MARK_FILESYSTEM, mask).is_ok() {
        return Ok(());
    }
    let fs_err = std::io::Error::last_os_error();
    warn!(
        path = %root.display(),
        error = %fs_err,
        "fanotify filesystem mark failed; trying mount mark"
    );

    if try_mark(fd, root, FAN_MARK_ADD | FAN_MARK_MOUNT, mask).is_ok() {
        return Ok(());
    }
    let mount_err = std::io::Error::last_os_error();
    if source.is_file {
        warn!(
            path = %root.display(),
            error = %mount_err,
            "fanotify mount mark failed; trying file mark"
        );
        return try_mark(fd, root, FAN_MARK_ADD, mask);
    }

    warn!(
        path = %root.display(),
        error = %mount_err,
        "fanotify mount mark failed; trying recursive directory marks"
    );
    mark_directory_tree(fd, root, mask | FAN_EVENT_ON_CHILD)
}

fn try_mark(fd: RawFd, root: &Path, flags: u32, mask: u64) -> Result<()> {
    let c_path = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| anyhow!("source path contains nul byte: {}", root.display()))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_fanotify_mark,
            fd,
            flags as libc::c_uint,
            mask,
            libc::AT_FDCWD,
            c_path.as_ptr(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("fanotify_mark failed");
    }
    Ok(())
}

fn mark_directory_tree(fd: RawFd, root: &Path, mask: u64) -> Result<()> {
    let mut count = 0_usize;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_dir() {
            continue;
        }
        try_mark(fd, entry.path(), FAN_MARK_ADD | FAN_MARK_ONLYDIR, mask)
            .with_context(|| format!("failed to mark directory {}", entry.path().display()))?;
        count += 1;
    }
    info!(root = %root.display(), directories = count, "fanotify recursive directory marks registered");
    Ok(())
}

fn parse_events(
    state: &State,
    sources: &mut [SourceRoot],
    fanotify_fd: RawFd,
    mode: FanotifyMode,
    mark_mask: u64,
    mut bytes: &[u8],
) -> Result<()> {
    let min_len = mem::size_of::<FanotifyEventMetadata>();
    while bytes.len() >= min_len {
        let meta = unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<FanotifyEventMetadata>()) };
        if meta.vers != FANOTIFY_METADATA_VERSION {
            // Cannot trust event_len to resync mid-buffer; drop the rest of this
            // read. The next read() returns fresh, aligned events.
            warn!(
                version = meta.vers,
                "unsupported fanotify metadata version; dropping rest of buffer"
            );
            break;
        }
        if meta.event_len == 0 || meta.event_len as usize > bytes.len() {
            warn!(
                event_len = meta.event_len,
                "invalid fanotify event length; dropping rest of buffer"
            );
            break;
        }

        // Persist failures (e.g. a transient SQLite lock) must not kill the
        // watcher thread; log and continue so realtime watching survives.
        let result = if meta.mask & FAN_Q_OVERFLOW != 0 {
            warn!("fanotify queue overflow; recording realtime source events");
            (|| -> Result<()> {
                for source in &mut *sources {
                    state.record_event(&source.id, meta.mask, "queue_overflow", None, true)?;
                }
                Ok(())
            })()
        } else {
            let event = &bytes[..meta.event_len as usize];
            match mode {
                FanotifyMode::FidName => {
                    persist_fid_name_event(state, sources, fanotify_fd, mark_mask, &meta, event)
                }
                FanotifyMode::FdPath => persist_fd_path_event(state, sources, &meta),
            }
        };
        if let Err(err) = result {
            warn!(error = %err, "failed to persist fanotify event; skipping");
        }

        bytes = &bytes[meta.event_len as usize..];
    }
    Ok(())
}

fn persist_fd_path_event(
    state: &State,
    sources: &[SourceRoot],
    meta: &FanotifyEventMetadata,
) -> Result<()> {
    let Some(path) = event_path(meta.fd) else {
        for source in sources {
            state.record_event(&source.id, meta.mask, mask_to_kind(meta.mask), None, true)?;
        }
        return Ok(());
    };

    for source in sources {
        if let Some(rel) = source_relative_event_path(source, &path) {
            state.record_event(
                &source.id,
                meta.mask,
                mask_to_kind(meta.mask),
                Some(rel.as_str()),
                false,
            )?;
        }
    }
    Ok(())
}

fn persist_fid_name_event(
    state: &State,
    sources: &mut [SourceRoot],
    fanotify_fd: RawFd,
    mark_mask: u64,
    meta: &FanotifyEventMetadata,
    event: &[u8],
) -> Result<()> {
    let records = fid_records(event)?;
    // Deletes/moves-away invalidate the cached handle→path entry for the gone
    // path (and its subtree); keeping them would leak memory and, on inode
    // reuse, resolve a future event to the wrong path.
    let is_removal =
        meta.mask & (FAN_DELETE | FAN_MOVED_FROM | FAN_DELETE_SELF | FAN_MOVE_SELF) != 0;
    let mut recorded = false;
    for source in &mut *sources {
        for record in &records {
            let Some(path) = fid_record_path(source, record)? else {
                continue;
            };
            let Some(rel) = source_relative_event_path(source, &path) else {
                continue;
            };
            state.record_event(
                &source.id,
                meta.mask,
                mask_to_kind(meta.mask),
                Some(rel.as_str()),
                false,
            )?;
            recorded = true;
            if is_removal {
                remove_handle_paths_under(source, &path);
            } else {
                track_new_path_and_mark_directory(source, fanotify_fd, mark_mask, &path);
            }
        }
    }
    if !recorded {
        debug!(
            mask = meta.mask,
            kind = mask_to_kind(meta.mask),
            "fanotify FID/name event did not resolve to a source path"
        );
    }
    Ok(())
}

#[derive(Debug)]
struct FidRecord {
    handle: Vec<u8>,
    name: Option<OsString>,
}

fn fid_records(event: &[u8]) -> Result<Vec<FidRecord>> {
    let meta_len = mem::size_of::<FanotifyEventMetadata>();
    if event.len() < meta_len {
        bail!("fanotify event shorter than metadata");
    }
    let meta = unsafe { ptr::read_unaligned(event.as_ptr().cast::<FanotifyEventMetadata>()) };
    let mut offset = meta.metadata_len as usize;
    let mut records = Vec::new();
    while offset + mem::size_of::<FanotifyEventInfoHeader>() <= event.len() {
        let header = unsafe {
            ptr::read_unaligned(event[offset..].as_ptr().cast::<FanotifyEventInfoHeader>())
        };
        let len = header.len as usize;
        if len < mem::size_of::<FanotifyEventInfoHeader>() || offset + len > event.len() {
            bail!("invalid fanotify info record length {}", len);
        }
        if matches!(
            header.info_type,
            FAN_EVENT_INFO_TYPE_FID
                | FAN_EVENT_INFO_TYPE_DFID
                | FAN_EVENT_INFO_TYPE_DFID_NAME
                | FAN_EVENT_INFO_TYPE_OLD_DFID_NAME
                | FAN_EVENT_INFO_TYPE_NEW_DFID_NAME
        ) {
            if let Some(record) = parse_fid_record(header.info_type, &event[offset..offset + len])?
            {
                records.push(record);
            }
        }
        offset += len;
    }
    Ok(records)
}

fn parse_fid_record(info_type: u8, bytes: &[u8]) -> Result<Option<FidRecord>> {
    let base = mem::size_of::<FanotifyEventInfoHeader>() + 8;
    if bytes.len() < base + mem::size_of::<FileHandleHeader>() {
        return Ok(None);
    }
    let handle_start = base;
    let handle_header =
        unsafe { ptr::read_unaligned(bytes[handle_start..].as_ptr().cast::<FileHandleHeader>()) };
    let handle_bytes = handle_header.handle_bytes as usize;
    if handle_bytes > MAX_HANDLE_SZ {
        bail!("fanotify file handle too large: {}", handle_bytes);
    }
    let handle_len = mem::size_of::<FileHandleHeader>() + handle_bytes;
    if handle_start + handle_len > bytes.len() {
        bail!("truncated fanotify file handle");
    }
    let handle = bytes[handle_start..handle_start + handle_len].to_vec();
    let name = if matches!(
        info_type,
        FAN_EVENT_INFO_TYPE_DFID_NAME
            | FAN_EVENT_INFO_TYPE_OLD_DFID_NAME
            | FAN_EVENT_INFO_TYPE_NEW_DFID_NAME
    ) {
        parse_fid_name(&bytes[handle_start + handle_len..])
    } else {
        None
    };
    Ok(Some(FidRecord { handle, name }))
}

fn parse_fid_name(bytes: &[u8]) -> Option<OsString> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if end == 0 {
        None
    } else {
        Some(OsString::from_vec(bytes[..end].to_vec()))
    }
}

fn fid_record_path(source: &SourceRoot, record: &FidRecord) -> Result<Option<PathBuf>> {
    let Some(base) = source.handle_paths.get(&record.handle) else {
        return Ok(None);
    };
    Ok(Some(match &record.name {
        Some(name) => base.join(name),
        None => base.clone(),
    }))
}

fn track_new_path_and_mark_directory(
    source: &mut SourceRoot,
    fanotify_fd: RawFd,
    mark_mask: u64,
    path: &Path,
) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let newly_tracked = match handle_key_from_path(path) {
        Ok(handle) => source
            .handle_paths
            .insert(handle, path.to_path_buf())
            .is_none(),
        Err(_) => true,
    };
    if !metadata.is_dir() {
        return;
    }
    if !newly_tracked {
        return;
    }
    if let Err(err) = mark_directory_tree(fanotify_fd, path, mark_mask | FAN_EVENT_ON_CHILD) {
        warn!(path = %path.display(), error = %err, "failed to mark new fanotify directory");
    }
}

fn event_path(fd: i32) -> Option<PathBuf> {
    if fd == FAN_NOFD {
        return None;
    }
    let proc_path = PathBuf::from(format!("/proc/self/fd/{fd}"));
    let path = std::fs::read_link(proc_path).ok();
    close_event_fd(fd);
    path
}

fn close_event_fd(fd: i32) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

fn source_roots(cfg: &AppConfig) -> Result<Vec<SourceRoot>> {
    cfg.source_groups
        .iter()
        .filter(|s| source_needs_fanotify(s))
        .map(source_root)
        .collect()
}

fn source_root(source: &SourceGroupConfig) -> Result<SourceRoot> {
    let root = source
        .src
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source {}", source.src.display()))?;
    let metadata =
        std::fs::metadata(&root).with_context(|| format!("failed to stat {}", root.display()))?;
    if !metadata.is_dir() && !metadata.is_file() {
        bail!("source is neither file nor directory: {}", root.display());
    }
    let handle_paths = build_handle_path_map(&root, metadata.is_file())?;
    Ok(SourceRoot {
        id: source.id.clone(),
        root,
        is_file: metadata.is_file(),
        handle_paths,
    })
}

fn build_handle_path_map(root: &Path, is_file: bool) -> Result<HashMap<Vec<u8>, PathBuf>> {
    let mut handles = HashMap::new();
    if is_file {
        if let Some(parent) = root.parent() {
            insert_handle_path(&mut handles, parent);
        }
        insert_handle_path(&mut handles, root);
        return Ok(handles);
    }

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        insert_handle_path(&mut handles, entry.path());
    }
    Ok(handles)
}

/// Drop cached handle→path entries for `path` and anything beneath it (a
/// directory removal takes its whole subtree with it). Keyed by value because
/// the inode is gone, so its handle can no longer be computed from the path.
fn remove_handle_paths_under(source: &mut SourceRoot, path: &Path) {
    source
        .handle_paths
        .retain(|_, cached| cached != path && !cached.starts_with(path));
}

fn insert_handle_path(handles: &mut HashMap<Vec<u8>, PathBuf>, path: &Path) {
    match handle_key_from_path(path) {
        Ok(handle) => {
            handles.insert(handle, path.to_path_buf());
        }
        Err(err) => {
            warn!(path = %path.display(), error = %err, "failed to resolve file handle");
        }
    }
}

fn handle_key_from_path(path: &Path) -> Result<Vec<u8>> {
    #[repr(C)]
    struct HandleBuf {
        header: FileHandleHeader,
        bytes: [u8; MAX_HANDLE_SZ],
    }

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("path contains nul byte: {}", path.display()))?;
    let mut mount_id: libc::c_int = 0;
    let mut buf = HandleBuf {
        header: FileHandleHeader {
            handle_bytes: MAX_HANDLE_SZ as u32,
            handle_type: 0,
        },
        bytes: [0; MAX_HANDLE_SZ],
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            libc::AT_FDCWD,
            c_path.as_ptr(),
            (&mut buf.header as *mut FileHandleHeader).cast::<libc::c_void>(),
            &mut mount_id,
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("name_to_handle_at failed");
    }
    let handle_bytes = buf.header.handle_bytes as usize;
    if handle_bytes > MAX_HANDLE_SZ {
        bail!("name_to_handle_at returned oversized file handle: {handle_bytes}");
    }

    let mut key = Vec::with_capacity(mem::size_of::<FileHandleHeader>() + handle_bytes);
    key.extend_from_slice(&buf.header.handle_bytes.to_ne_bytes());
    key.extend_from_slice(&buf.header.handle_type.to_ne_bytes());
    key.extend_from_slice(&buf.bytes[..handle_bytes]);
    Ok(key)
}

fn source_relative_event_path(source: &SourceRoot, path: &Path) -> Option<String> {
    if source.is_file {
        if path == source.root {
            return source
                .root
                .file_name()
                .map(|name| name.to_string_lossy().to_string());
        }
        return None;
    }

    let rel = path.strip_prefix(&source.root).ok()?;
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel.to_string_lossy().to_string())
    }
}

fn mask_to_kind(mask: u64) -> &'static str {
    if mask & FAN_CLOSE_WRITE != 0 {
        "close_write"
    } else if mask & FAN_CREATE != 0 {
        "create"
    } else if mask & FAN_MODIFY != 0 {
        "modify"
    } else if mask & FAN_DELETE != 0 {
        "delete"
    } else if mask & FAN_MOVED_FROM != 0 {
        "move_from"
    } else if mask & FAN_MOVED_TO != 0 {
        "move_to"
    } else if mask & FAN_DELETE_SELF != 0 {
        "delete_self"
    } else if mask & FAN_MOVE_SELF != 0 {
        "move_self"
    } else {
        "other"
    }
}

struct FdGuard(RawFd);

impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}
