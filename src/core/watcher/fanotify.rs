use std::ffi::CString;
use std::mem;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{error, info, warn};
use walkdir::WalkDir;

use crate::core::config::{AppConfig, ScheduleMode, SourceGroupConfig};
use crate::core::state::State;

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
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
const FAN_NOFD: i32 = -1;
const FANOTIFY_METADATA_VERSION: u8 = 3;

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

#[derive(Debug, Clone)]
struct SourceRoot {
    id: String,
    root: PathBuf,
    is_file: bool,
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
                if let Err(err) = run_fanotify_loop(source_cfg, db_path, shutdown) {
                    error!(error = %err, "fanotify source watcher stopped");
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

fn source_needs_fanotify(source: &SourceGroupConfig) -> bool {
    source.enabled
        && source
            .destinations
            .iter()
            .any(|dst| dst.enabled && dst.schedule.mode == ScheduleMode::Realtime)
}

fn run_fanotify_loop(cfg: AppConfig, db_path: PathBuf, shutdown: Arc<AtomicBool>) -> Result<()> {
    let sources = source_roots(&cfg)?;
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

    let fd = fanotify_init()?;
    let _guard = FdGuard(fd);
    let mask = FAN_MODIFY
        | FAN_CLOSE_WRITE
        | FAN_CREATE
        | FAN_DELETE
        | FAN_MOVED_FROM
        | FAN_MOVED_TO
        | FAN_DELETE_SELF
        | FAN_MOVE_SELF;

    for source in &sources {
        mark_source(fd, source, mask)
            .with_context(|| format!("failed to mark source {}", source.root.display()))?;
        info!(source = source.id, root = %source.root.display(), "fanotify mark registered");
    }

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
        parse_events(&state, &sources, &buf[..n as usize])?;
    }
    Ok(())
}

fn fanotify_init() -> Result<RawFd> {
    let flags = FAN_CLOEXEC | FAN_NONBLOCK | FAN_CLASS_NOTIF;
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

fn mark_source(fd: RawFd, source: &SourceRoot, mask: u64) -> Result<()> {
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
    mark_directory_tree(fd, root, mask)
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

fn parse_events(state: &State, sources: &[SourceRoot], mut bytes: &[u8]) -> Result<()> {
    let min_len = mem::size_of::<FanotifyEventMetadata>();
    while bytes.len() >= min_len {
        let meta = unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<FanotifyEventMetadata>()) };
        if meta.vers != FANOTIFY_METADATA_VERSION {
            bail!("unsupported fanotify metadata version {}", meta.vers);
        }
        if meta.event_len == 0 || meta.event_len as usize > bytes.len() {
            bail!("invalid fanotify event length {}", meta.event_len);
        }

        if meta.mask & FAN_Q_OVERFLOW != 0 {
            warn!("fanotify queue overflow; marking all sources for full rescan");
            for source in sources {
                state.record_event(&source.id, meta.mask, "queue_overflow", None, true)?;
            }
        } else {
            persist_event(state, sources, &meta)?;
        }

        bytes = &bytes[meta.event_len as usize..];
    }
    Ok(())
}

fn persist_event(
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
            let rescan_required = meta.mask & (FAN_DELETE | FAN_MOVED_FROM | FAN_DELETE_SELF) != 0;
            state.record_event(
                &source.id,
                meta.mask,
                mask_to_kind(meta.mask),
                Some(rel.as_str()),
                rescan_required,
            )?;
        }
    }
    Ok(())
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
    Ok(SourceRoot {
        id: source.id.clone(),
        root,
        is_file: metadata.is_file(),
    })
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
