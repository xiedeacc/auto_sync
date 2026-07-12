use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsString};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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
use crate::core::state::{State, WatcherEvent};

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
const FAN_REPORT_FID: u32 = 0x0000_0200;
const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
const FAN_REPORT_NAME: u32 = 0x0000_0800;
const FAN_MARK_ADD: u32 = 0x0000_0001;
const FAN_MARK_ONLYDIR: u32 = 0x0000_0008;
const FAN_MARK_MOUNT: u32 = 0x0000_0010;
const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;

const FAN_MODIFY: u64 = 0x0000_0002;
const FAN_ATTRIB: u64 = 0x0000_0004;
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
/// Cap on the LAZY handle→path cache (directories only). Past it the map is
/// dropped wholesale and refills on demand — cheap, and correctness does not
/// depend on the cache. Eager (pre-built) maps are exempt: without
/// `open_by_handle_at` they are the only way to resolve events.
const MAX_LAZY_HANDLE_CACHE: usize = 131_072;

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
    /// handle→path cache, DIRECTORIES ONLY: DFID_NAME records carry the
    /// parent directory's handle plus the child name, so directory handles
    /// repeat for every child event while a file handle is looked up at most
    /// once. With lazy resolution (see `mount_fd`) it starts nearly empty,
    /// fills on demand, and is dropped wholesale past a size cap (refills
    /// lazily); without lazy resolution it is pre-built by walking the tree's
    /// directories at startup (uncapped — it must stay complete).
    handle_paths: HashMap<Vec<u8>, PathBuf>,
    /// An O_PATH fd of the source root for `open_by_handle_at`, present when
    /// the kernel/caps allow resolving file handles directly. This replaces
    /// the startup walk of the whole tree (minutes on an HDD with hundreds of
    /// thousands of entries) with an on-demand syscall per unseen handle.
    mount_fd: Option<Arc<OwnedFd>>,
    /// True when the filesystem-wide mark failed and the source is watched via
    /// per-directory marks: newly created directories must then be marked too.
    /// With a filesystem mark this is false and new directories need nothing.
    recursive_marks: bool,
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
            crate::core::signal::mark_watcher_armed();
            while !shutdown.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
            }
            return;
        }

        // The startup change scan waits for every source's marks to be live
        // (see signal::wait_watcher_armed): count down as each arms.
        let pending_arms = Arc::new(std::sync::atomic::AtomicUsize::new(sources.len()));
        let mut handles = Vec::new();
        for source in sources {
            let mut source_cfg = cfg.clone();
            source_cfg.source_groups = vec![source];
            let db_path = db_path.clone();
            let shutdown = shutdown.clone();
            let pending_arms = pending_arms.clone();
            handles.push(thread::spawn(move || {
                let mut armed = false;
                let mut note_armed = move || {
                    if !armed {
                        armed = true;
                        if pending_arms.fetch_sub(1, Ordering::SeqCst) == 1 {
                            crate::core::signal::mark_watcher_armed();
                        }
                    }
                };
                // Supervise: a watcher that errors out (read error, mark/setup
                // failure) is restarted after a short backoff instead of dying
                // silently for the lifetime of the process.
                while !shutdown.load(Ordering::SeqCst) {
                    match run_fanotify_loop(
                        source_cfg.clone(),
                        db_path.clone(),
                        shutdown.clone(),
                        &mut note_armed,
                    ) {
                        Ok(()) => break,
                        Err(err) => {
                            // A watcher that cannot set up must not block the
                            // startup scan forever.
                            note_armed();
                            error!(error = %err, "fanotify source watcher stopped; restarting after backoff");
                            // The kernel queue died with the fd: events between
                            // now and the re-arm are unobservable. Mark the gap
                            // so the next pass reconciles instead of trusting
                            // an incomplete event stream.
                            if let Ok(state) = State::open(&db_path) {
                                for source in &source_cfg.source_groups {
                                    state
                                        .record_event(
                                            &source.id,
                                            0,
                                            "watcher_restart_gap",
                                            None,
                                            true,
                                        )
                                        .ok();
                                }
                            }
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
        && source.destinations.iter().any(|dst| dst.enabled)
}

fn run_fanotify_loop(
    cfg: AppConfig,
    db_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    on_armed: &mut dyn FnMut(),
) -> Result<()> {
    let mut sources = source_roots(&cfg)?;
    if sources.is_empty() {
        info!("fanotify watcher has no enabled sources");
        on_armed();
        while !shutdown.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
        }
        return Ok(());
    }

    let state = State::open(&db_path)?;
    state.ensure_config(&cfg)?;
    state.ensure_open_cycles(&cfg)?;

    // FAN_ATTRIB (kernel 5.1+, FID mode only): chmod/touch produce events so
    // metadata-only changes propagate; the Windows USN watcher listens to the
    // equivalent BASIC_INFO/SECURITY reasons already.
    let fid_mask = FAN_MODIFY
        | FAN_ATTRIB
        | FAN_CLOSE_WRITE
        | FAN_CREATE
        | FAN_DELETE
        | FAN_MOVED_FROM
        | FAN_MOVED_TO
        | FAN_DELETE_SELF
        | FAN_MOVE_SELF
        | FAN_ONDIR;
    let (fd, mode, mask) = match setup_fid_name_fanotify(&mut sources, fid_mask) {
        Ok(fd) => {
            info!("fanotify FID/name watcher enabled");
            (fd, FanotifyMode::FidName, fid_mask)
        }
        Err(err) => {
            // fd-path mode sees only MODIFY/CLOSE_WRITE: deletes, renames and
            // mkdirs produce NO events, so mirror syncing from the event
            // stream alone would silently diverge. Make the degradation loud
            // and force a reconcile (re-recorded on every watcher start).
            error!(
                error = %err,
                "fanotify degraded to fd-path mode: deletes/renames are invisible \
                 to the event stream; forcing a reconcile"
            );
            for source in &sources {
                state
                    .record_event(&source.id, 0, "watcher_degraded", None, true)
                    .ok();
            }
            let fd = setup_fd_path_fanotify(&sources)?;
            (fd, FanotifyMode::FdPath, FAN_MODIFY | FAN_CLOSE_WRITE)
        }
    };
    let _guard = FdGuard(fd);
    // Marks are live from here: events flow, so the startup change scan can
    // safely begin (anything it misses from now on, the watcher records).
    on_armed();

    let mut ctx = WatchCtx {
        sticky_rescan: false,
        last_eager_rebuild: None,
    };
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
        parse_events(
            &state,
            &mut sources,
            fd,
            mode,
            mask,
            &buf[..n as usize],
            &mut ctx,
        )?;
    }
    Ok(())
}

/// Per-loop state threaded through event parsing.
struct WatchCtx {
    /// A previous batch failed to persist (events lost): the next successful
    /// write is prefixed with a rescan_required marker so the loss triggers a
    /// reconcile instead of silently vanishing — including when the lost event
    /// was the queue_overflow marker itself.
    sticky_rescan: bool,
    /// Throttle for eager-mode handle-map rebuilds after an overflow.
    last_eager_rebuild: Option<std::time::Instant>,
}

fn setup_fid_name_fanotify(sources: &mut [SourceRoot], mask: u64) -> Result<RawFd> {
    // Tiered init. FAN_REPORT_TARGET_FID is deliberately absent: the parser
    // never needed it (DIR_FID+NAME resolves dirent events, FID resolves
    // modifies), it doubled the info records per event (two event_log rows),
    // and it required kernel 5.17+ — pushing 5.9-5.16 kernels (Ubuntu 22.04,
    // Debian 11, RHEL 9) into the crippled fd-path fallback for nothing.
    // The FID-only tier (5.1+) resolves dirent events to the parent
    // directory: coarser (the sync recurses that directory) but complete.
    let tiers: [(&str, u32); 2] = [
        (
            "dir_fid+name+fid",
            FAN_REPORT_DIR_FID | FAN_REPORT_NAME | FAN_REPORT_FID,
        ),
        ("fid", FAN_REPORT_FID),
    ];
    let mut last_err: Option<anyhow::Error> = None;
    for (index, (label, flags)) in tiers.iter().enumerate() {
        match fanotify_init(*flags) {
            Ok(fd) => {
                if let Err(err) = mark_sources_fid(fd, sources, mask) {
                    close_event_fd(fd);
                    return Err(err);
                }
                if index > 0 {
                    warn!(
                        tier = label,
                        "fanotify running in a reduced FID tier (older kernel); \
                         dirent events resolve to their parent directory"
                    );
                }
                return Ok(fd);
            }
            Err(err) => {
                warn!(tier = label, error = %err, "fanotify FID tier unavailable");
                last_err = Some(err);
            }
        }
    }
    Err(last_err.expect("at least one tier attempted")).context("fanotify_init FID modes failed")
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

fn mark_sources_fid(fd: RawFd, sources: &mut [SourceRoot], mask: u64) -> Result<()> {
    for source in sources {
        mark_source_fid(fd, source, mask)
            .with_context(|| format!("failed to mark source {}", source.root.display()))?;
        info!(source = source.id, root = %source.root.display(), "fanotify FID/name mark registered");
    }
    Ok(())
}

fn mark_source_fid(fd: RawFd, source: &mut SourceRoot, mask: u64) -> Result<()> {
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
    source.recursive_marks = true;
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

#[allow(clippy::too_many_arguments)]
fn parse_events(
    state: &State,
    sources: &mut [SourceRoot],
    fanotify_fd: RawFd,
    mode: FanotifyMode,
    mark_mask: u64,
    mut bytes: &[u8],
    ctx: &mut WatchCtx,
) -> Result<()> {
    let mut batch: Vec<WatcherEvent> = Vec::new();
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

        if meta.mask & FAN_Q_OVERFLOW != 0 {
            warn!("fanotify queue overflow; recording realtime source events");
            for source in &mut *sources {
                batch.push(pending_event(
                    &source.id,
                    meta.mask,
                    "queue_overflow",
                    None,
                    true,
                ));
                // Eager handle maps (no open_by_handle_at) go permanently
                // blind for directories whose create events the overflow ate:
                // rebuild the map (throttled — a walk of a large tree).
                if source.mount_fd.is_none() && !source.is_file {
                    let rebuild_due = ctx
                        .last_eager_rebuild
                        .is_none_or(|at| at.elapsed() > Duration::from_secs(600));
                    if rebuild_due {
                        ctx.last_eager_rebuild = Some(std::time::Instant::now());
                        match build_handle_path_map(&source.root, source.is_file) {
                            Ok(map) => source.handle_paths = map,
                            Err(err) => warn!(
                                source = source.id,
                                error = %err,
                                "failed to rebuild handle map after overflow"
                            ),
                        }
                    }
                }
            }
        } else {
            let event = &bytes[..meta.event_len as usize];
            let collected = match mode {
                FanotifyMode::FidName => collect_fid_name_event(
                    sources,
                    fanotify_fd,
                    mark_mask,
                    &meta,
                    event,
                    &mut batch,
                ),
                FanotifyMode::FdPath => collect_fd_path_event(sources, &meta, &mut batch),
            };
            if let Err(err) = collected {
                warn!(error = %err, "failed to parse fanotify event; skipping");
            }
        }

        bytes = &bytes[meta.event_len as usize..];
    }

    // Same (source, kind, path) rows within one read() batch are redundant.
    let mut seen: HashSet<(String, String, Option<String>)> = HashSet::new();
    batch.retain(|event| {
        seen.insert((
            event.source_id.clone(),
            event.event_kind.clone(),
            event.rel_path.clone(),
        ))
    });
    if ctx.sticky_rescan && !batch.is_empty() {
        // A previous batch was lost (persist failure): mark the gap before the
        // new events so the loss forces a reconcile.
        for source in &*sources {
            batch.insert(
                0,
                pending_event(&source.id, 0, "event_persist_gap", None, true),
            );
        }
    }
    // One transaction (one fsync) for the whole batch. Persist failures must
    // not kill the watcher thread; the sticky flag keeps the loss visible.
    match state.record_events_batch(&batch) {
        Ok(()) => {
            if !batch.is_empty() {
                ctx.sticky_rescan = false;
            }
        }
        Err(err) => {
            warn!(error = %err, "failed to persist fanotify event batch");
            ctx.sticky_rescan = true;
        }
    }
    Ok(())
}

fn pending_event(
    source_id: &str,
    raw_mask: u64,
    kind: &str,
    rel_path: Option<String>,
    rescan_required: bool,
) -> WatcherEvent {
    WatcherEvent {
        source_id: source_id.to_string(),
        raw_mask,
        event_kind: kind.to_string(),
        rel_path,
        rescan_required,
    }
}

fn collect_fd_path_event(
    sources: &[SourceRoot],
    meta: &FanotifyEventMetadata,
    batch: &mut Vec<WatcherEvent>,
) -> Result<()> {
    let Some(path) = event_path(meta.fd) else {
        for source in sources {
            batch.push(pending_event(
                &source.id,
                meta.mask,
                mask_to_kind(meta.mask),
                None,
                true,
            ));
        }
        return Ok(());
    };

    for source in sources {
        if let Some(rel) = source_relative_event_path(source, &path) {
            batch.push(pending_event(
                &source.id,
                meta.mask,
                mask_to_kind(meta.mask),
                Some(rel),
                false,
            ));
        }
    }
    Ok(())
}

fn collect_fid_name_event(
    sources: &mut [SourceRoot],
    fanotify_fd: RawFd,
    mark_mask: u64,
    meta: &FanotifyEventMetadata,
    event: &[u8],
    batch: &mut Vec<WatcherEvent>,
) -> Result<()> {
    let records = fid_records(event)?;
    // Deletes/moves-away invalidate the cached handle→path entry for the gone
    // path (and its subtree); keeping them would leak memory and, on inode
    // reuse, resolve a future event to the wrong path.
    let is_removal =
        meta.mask & (FAN_DELETE | FAN_MOVED_FROM | FAN_DELETE_SELF | FAN_MOVE_SELF) != 0;
    let mut resolved = false;
    for source in &mut *sources {
        for record in &records {
            let Some(path) = fid_record_path(source, record)? else {
                continue;
            };
            let Some(rel) = source_relative_event_path(source, &path) else {
                continue;
            };
            batch.push(pending_event(
                &source.id,
                meta.mask,
                mask_to_kind(meta.mask),
                Some(rel),
                false,
            ));
            resolved = true;
            if is_removal {
                remove_handle_paths_under(source, &path);
            } else if !track_new_path_and_mark_directory(source, fanotify_fd, mark_mask, &path) {
                // The new subtree could not be marked (per-directory fallback
                // hitting max_user_marks): it is permanently unwatched, which
                // must trigger a reconcile instead of a silent blind spot.
                batch.push(pending_event(
                    &source.id,
                    0,
                    "watch_mark_failed",
                    None,
                    true,
                ));
            }
        }
    }
    if !resolved {
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

fn fid_record_path(source: &mut SourceRoot, record: &FidRecord) -> Result<Option<PathBuf>> {
    let base = match source.handle_paths.get(&record.handle) {
        Some(base) => base.clone(),
        None => {
            // Lazy resolution: ask the kernel for the handle's current path.
            // ESTALE (inode already gone — e.g. DELETE_SELF of the removed
            // dir itself) stays unresolved; the accompanying parent-based
            // DELETE event still records the removal.
            let Some(mount_fd) = source.mount_fd.as_ref() else {
                return Ok(None);
            };
            let Some(path) = path_from_handle(mount_fd.as_raw_fd(), &record.handle) else {
                return Ok(None);
            };
            // Only paths inside (or equal to) the root belong to this source;
            // a filesystem-wide mark also reports sibling trees on the same
            // fs. A file source additionally needs its parent directory (the
            // DFID base of its own events).
            let file_parent = source.is_file && Some(path.as_path()) == source.root.parent();
            if !path.starts_with(&source.root) && !file_parent {
                return Ok(None);
            }
            // Cache directory handles only: a record WITH a name is a
            // DFID_NAME record whose base is a directory by protocol. Bare
            // FID records (the object itself, possibly a file) resolve at
            // most once per event and are not worth a cache slot.
            if record.name.is_some() {
                if source.handle_paths.len() >= MAX_LAZY_HANDLE_CACHE {
                    // Cheap overflow policy: drop everything and refill on
                    // demand. Correctness is unaffected — resolution is lazy.
                    source.handle_paths.clear();
                }
                source
                    .handle_paths
                    .insert(record.handle.clone(), path.clone());
            }
            path
        }
    };
    Ok(Some(match &record.name {
        Some(name) => base.join(name),
        None => base,
    }))
}

/// Returns `false` when a required watch mark could not be registered (the
/// caller records a rescan marker: an unmarked new subtree is a permanent
/// blind spot in per-directory fallback mode).
fn track_new_path_and_mark_directory(
    source: &mut SourceRoot,
    fanotify_fd: RawFd,
    mark_mask: u64,
    path: &Path,
) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return true;
    };
    // The cache holds directories only (files resolve at most once, and the
    // map must stay small enough for removal's prefix sweep).
    if !metadata.is_dir() {
        return true;
    }
    // With lazy handle resolution the cache fills on demand; pre-inserting
    // here just saves the first open_by_handle_at for the new path.
    let newly_tracked = match handle_key_from_path(path) {
        Ok(handle) => source
            .handle_paths
            .insert(handle, path.to_path_buf())
            .is_none(),
        Err(_) => true,
    };
    if !newly_tracked {
        return true;
    }
    // A filesystem-wide mark already covers new directories; only the
    // per-directory fallback must mark each created subtree explicitly.
    if !source.recursive_marks {
        return true;
    }
    if let Err(err) = mark_directory_tree(fanotify_fd, path, mark_mask | FAN_EVENT_ON_CHILD) {
        warn!(path = %path.display(), error = %err, "failed to mark new fanotify directory");
        return false;
    }
    true
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
    // Prefer lazy handle resolution: probe open_by_handle_at with the root's
    // own handle. When it works (needs CAP_DAC_READ_SEARCH), unseen handles
    // resolve on demand and the multi-minute startup walk of a large tree is
    // skipped entirely. Otherwise fall back to pre-building the full map.
    let mount_fd = probe_lazy_handle_resolution(&root);
    let handle_paths = if mount_fd.is_some() {
        let mut handles = HashMap::new();
        if let Some(parent) = root.parent() {
            insert_handle_path(&mut handles, parent);
        }
        insert_handle_path(&mut handles, &root);
        handles
    } else {
        warn!(
            root = %root.display(),
            "open_by_handle_at unavailable; pre-building fanotify handle map (slow on large trees)"
        );
        build_handle_path_map(&root, metadata.is_file())?
    };
    Ok(SourceRoot {
        id: source.id.clone(),
        root,
        is_file: metadata.is_file(),
        handle_paths,
        mount_fd,
        recursive_marks: false,
    })
}

/// Opens the source root as the mount fd for `open_by_handle_at` and verifies
/// the call actually works here (kernel support + CAP_DAC_READ_SEARCH) by
/// resolving the root's own handle. Returns None when unusable.
fn probe_lazy_handle_resolution(root: &Path) -> Option<Arc<OwnedFd>> {
    let c_path = CString::new(root.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        warn!(
            root = %root.display(),
            error = %std::io::Error::last_os_error(),
            "failed to open source root for handle resolution"
        );
        return None;
    }
    let mount_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let root_handle = match handle_key_from_path(root) {
        Ok(handle) => handle,
        Err(err) => {
            warn!(root = %root.display(), error = %err, "failed to compute root file handle");
            return None;
        }
    };
    match path_from_handle(mount_fd.as_raw_fd(), &root_handle) {
        Some(path) if path == root => Some(Arc::new(mount_fd)),
        Some(path) => {
            warn!(
                root = %root.display(),
                resolved = %path.display(),
                "open_by_handle_at resolved the root to an unexpected path; using eager handle map"
            );
            None
        }
        None => None,
    }
}

/// Resolves a stored handle key back to a path via `open_by_handle_at` +
/// /proc/self/fd readlink. Returns None for gone inodes (ESTALE) or when the
/// call is not permitted.
fn path_from_handle(mount_fd: RawFd, handle_key: &[u8]) -> Option<PathBuf> {
    #[repr(C)]
    struct HandleBuf {
        header: FileHandleHeader,
        bytes: [u8; MAX_HANDLE_SZ],
    }

    let header_len = mem::size_of::<FileHandleHeader>();
    if handle_key.len() < header_len || handle_key.len() > header_len + MAX_HANDLE_SZ {
        return None;
    }
    let mut buf = HandleBuf {
        header: FileHandleHeader {
            handle_bytes: (handle_key.len() - header_len) as u32,
            handle_type: i32::from_ne_bytes(handle_key[4..8].try_into().ok()?),
        },
        bytes: [0; MAX_HANDLE_SZ],
    };
    buf.bytes[..handle_key.len() - header_len].copy_from_slice(&handle_key[header_len..]);
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_by_handle_at,
            mount_fd,
            (&mut buf.header as *mut FileHandleHeader).cast::<libc::c_void>(),
            libc::O_PATH | libc::O_CLOEXEC,
        ) as RawFd
    };
    if fd < 0 {
        return None;
    }
    let path = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok();
    close_event_fd(fd);
    path
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

    // Directories only: DFID_NAME event records reference directory handles
    // (parent + child name); file handles are never looked up. This also cuts
    // the startup walk's handle syscalls by the tree's file count.
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            insert_handle_path(&mut handles, entry.path());
        }
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
    } else if rel_has_internal_component(rel) {
        None
    } else {
        Some(rel.to_string_lossy().to_string())
    }
}

fn rel_has_internal_component(rel: &Path) -> bool {
    rel.components().any(|component| {
        let text = component.as_os_str().to_string_lossy();
        matches!(
            text.as_ref(),
            ".auto_sync_tmp" | ".auto_sync_trash" | ".auto_sync_probe"
        )
    })
}

fn mask_to_kind(mask: u64) -> &'static str {
    if mask & FAN_CLOSE_WRITE != 0 {
        "close_write"
    } else if mask & FAN_CREATE != 0 {
        "create"
    } else if mask & FAN_MODIFY != 0 {
        "modify"
    } else if mask & FAN_ATTRIB != 0 {
        // Same kind the Windows USN watcher records for BASIC_INFO/SECURITY.
        "metadata"
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
