use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::mem;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use tracing::{error, info, warn};
use walkdir::WalkDir;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS,
    ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY,
    FILE_NOTIFY_CHANGE_SIZE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V1,
    USN_REASON_BASIC_INFO_CHANGE, USN_REASON_COMPRESSION_CHANGE, USN_REASON_DATA_EXTEND,
    USN_REASON_DATA_OVERWRITE, USN_REASON_DATA_TRUNCATION, USN_REASON_EA_CHANGE,
    USN_REASON_ENCRYPTION_CHANGE, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_HARD_LINK_CHANGE, USN_REASON_INDEXABLE_CHANGE, USN_REASON_NAMED_DATA_EXTEND,
    USN_REASON_NAMED_DATA_OVERWRITE, USN_REASON_NAMED_DATA_TRUNCATION, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME, USN_REASON_REPARSE_POINT_CHANGE, USN_REASON_SECURITY_CHANGE,
    USN_REASON_STREAM_CHANGE, USN_RECORD_COMMON_HEADER, USN_RECORD_V2,
};

use crate::core::config::{AppConfig, ScheduleMode, SourceGroupConfig, machine_id_or_local};
use crate::core::state::State;

const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const USN_BUFFER_SIZE: usize = 1024 * 1024;
const DIRECTORY_CHANGE_BUFFER_SIZE: usize = 256 * 1024;
const FILE_NOTIFY_INFORMATION_HEADER_SIZE: usize = 12;

pub fn first_local_realtime_usn_access_denied(cfg: &AppConfig) -> Option<String> {
    for source in cfg
        .source_groups
        .iter()
        .filter(|source| source_needs_usn(source))
    {
        if let Err(err) = check_source_usn_access(source) {
            if error_has_raw_os_error(&err, ERROR_ACCESS_DENIED as i32) {
                return Some(format!(
                    "source {} requires elevated access to query the USN Journal: {}",
                    source.id,
                    format_error_chain(&err)
                ));
            }
        }
    }
    None
}

pub fn spawn_source_watcher_thread(
    cfg: AppConfig,
    db_path: PathBuf,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let sources: Vec<_> = cfg
            .source_groups
            .iter()
            .filter(|source| source_needs_usn(source))
            .cloned()
            .collect();
        if sources.is_empty() {
            info!("Windows USN watcher has no realtime local sources");
            while !shutdown.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
            }
            return;
        }

        let mut handles = Vec::new();
        for source in sources {
            let db_path = db_path.clone();
            let shutdown = shutdown.clone();
            handles.push(thread::spawn(move || {
                run_resilient_source_watcher(source, db_path, shutdown);
            }));
        }

        while !shutdown.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
        }
        for handle in handles {
            if let Err(err) = handle.join() {
                warn!(?err, "Windows USN watcher thread join failed");
            }
        }
    })
}

fn source_needs_usn(source: &SourceGroupConfig) -> bool {
    source.enabled
        && machine_id_or_local(&source.machine_id) == "local"
        && source
            .destinations
            .iter()
            .any(|dst| dst.enabled && dst.schedule.mode == ScheduleMode::Realtime)
}

fn check_source_usn_access(source: &SourceGroupConfig) -> Result<()> {
    let root = source
        .src
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source {}", source.src.display()))?;
    let volume = volume_from_path(&root)?;
    let volume = VolumeHandle::open(&volume)?;
    query_usn_journal(volume.raw())?;
    Ok(())
}

fn run_resilient_source_watcher(
    source: SourceGroupConfig,
    db_path: PathBuf,
    shutdown: Arc<AtomicBool>,
) {
    let mut reported_unavailable = false;
    while !shutdown.load(Ordering::SeqCst) {
        match run_source_watcher_session(&source, &db_path, &shutdown) {
            Ok(()) => break,
            Err(err) => {
                warn!(
                    source = source.id,
                    error = %format_error_chain(&err),
                    "Windows USN watcher session stopped; falling back to directory watcher"
                );
                match run_directory_changes_session(&source, &db_path, &shutdown) {
                    Ok(()) => break,
                    Err(fallback_err) => {
                        warn!(
                            source = source.id,
                            error = %format_error_chain(&fallback_err),
                            "Windows directory watcher session stopped"
                        );
                        if !reported_unavailable {
                            if let Err(record_err) = record_rescan_event(
                                &db_path,
                                &source.id,
                                "windows_watcher_unavailable",
                                true,
                            ) {
                                error!(
                                    source = source.id,
                                    error = %record_err,
                                    "failed to persist Windows watcher fallback event"
                                );
                            }
                            reported_unavailable = true;
                        }
                        sleep_until_retry(&shutdown);
                    }
                }
            }
        }
    }
}

fn run_source_watcher_session(
    source: &SourceGroupConfig,
    db_path: &Path,
    shutdown: &AtomicBool,
) -> Result<()> {
    let state = State::open(db_path)?;
    state.ensure_open_cycle(&source.id, Utc::now())?;

    let mut watch = SourceWatch::new(source)?;
    let volume = VolumeHandle::open(&watch.volume)?;
    let mut journal = query_usn_journal(volume.raw())?;
    let mut next_usn = initial_next_usn(&state, &mut watch, &journal)?;

    info!(
        source = watch.id,
        volume = watch.volume,
        next_usn,
        "Windows USN watcher started"
    );

    while !shutdown.load(Ordering::SeqCst) {
        journal = query_usn_journal(volume.raw())?;
        if journal.UsnJournalID != watch.journal_id {
            state.record_event(&watch.id, 0, "usn_journal_changed", None, true)?;
            next_usn = journal.NextUsn;
            watch.journal_id = journal.UsnJournalID;
            watch.rebuild_directory_index()?;
            state.set_windows_usn_cursor(&watch.id, &watch.volume, watch.journal_id, next_usn)?;
        } else if next_usn < journal.LowestValidUsn {
            state.record_event(&watch.id, 0, "usn_gap", None, true)?;
            next_usn = journal.NextUsn;
            watch.rebuild_directory_index()?;
            state.set_windows_usn_cursor(&watch.id, &watch.volume, watch.journal_id, next_usn)?;
        }

        let mut rebuild_index = false;
        loop {
            match read_usn_batch(volume.raw(), watch.journal_id, next_usn)? {
                UsnReadOutcome::Batch(batch) => {
                    if batch.next_usn > next_usn {
                        next_usn = batch.next_usn;
                    }
                    if batch.records.is_empty() {
                        break;
                    }
                    for record in batch.records {
                        rebuild_index |= watch.persist_matching_record(&state, &record)?;
                    }
                }
                UsnReadOutcome::Gap(kind) => {
                    warn!(
                        source = watch.id,
                        volume = watch.volume,
                        kind,
                        "Windows USN watcher detected an unreadable journal range; recording realtime source event"
                    );
                    state.record_event(&watch.id, 0, kind, None, false)?;
                    journal = query_usn_journal(volume.raw())?;
                    next_usn = journal.NextUsn;
                    watch.journal_id = journal.UsnJournalID;
                    watch.rebuild_directory_index()?;
                    state.set_windows_usn_cursor(
                        &watch.id,
                        &watch.volume,
                        watch.journal_id,
                        next_usn,
                    )?;
                    break;
                }
            }
            if rebuild_index {
                watch.rebuild_directory_index()?;
                rebuild_index = false;
            }
            if next_usn >= journal.NextUsn || shutdown.load(Ordering::SeqCst) {
                break;
            }
        }

        state.set_windows_usn_cursor(&watch.id, &watch.volume, watch.journal_id, next_usn)?;
        sleep_interruptible(shutdown, WATCH_POLL_INTERVAL);
    }
    Ok(())
}

fn initial_next_usn(
    state: &State,
    watch: &mut SourceWatch,
    journal: &USN_JOURNAL_DATA_V1,
) -> Result<i64> {
    watch.journal_id = journal.UsnJournalID;
    let cursor = state.windows_usn_cursor(&watch.id, &watch.volume)?;
    let Some(cursor) = cursor else {
        state.record_event(&watch.id, 0, "usn_initial_reconcile", None, false)?;
        state.set_windows_usn_cursor(
            &watch.id,
            &watch.volume,
            journal.UsnJournalID,
            journal.NextUsn,
        )?;
        return Ok(journal.NextUsn);
    };

    if cursor.journal_id != journal.UsnJournalID || cursor.next_usn < journal.LowestValidUsn {
        state.record_event(&watch.id, 0, "usn_cursor_reconcile", None, false)?;
        state.set_windows_usn_cursor(
            &watch.id,
            &watch.volume,
            journal.UsnJournalID,
            journal.NextUsn,
        )?;
        Ok(journal.NextUsn)
    } else {
        Ok(cursor.next_usn)
    }
}

fn run_directory_changes_session(
    source: &SourceGroupConfig,
    db_path: &Path,
    shutdown: &AtomicBool,
) -> Result<()> {
    let state = State::open(db_path)?;
    state.ensure_open_cycle(&source.id, Utc::now())?;

    let root = source
        .src
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source {}", source.src.display()))?;
    let info = file_information(&root)
        .with_context(|| format!("failed to read file id for {}", root.display()))?;
    let (watch_root, file_filter) = if !info.is_dir {
        let parent = root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("file source has no parent: {}", root.display()))?
            .to_path_buf();
        let file_name = root
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("file source has no file name: {}", root.display()))?
            .to_os_string();
        (parent, Some(file_name))
    } else {
        (root, None)
    };
    let handle = DirectoryWatchHandle::open(&watch_root)
        .with_context(|| format!("failed to watch directory {}", watch_root.display()))?;
    let mut buffer = vec![0_u8; DIRECTORY_CHANGE_BUFFER_SIZE];
    info!(
        source = source.id,
        root = %watch_root.display(),
        "Windows directory watcher started"
    );

    while !shutdown.load(Ordering::SeqCst) {
        let records = read_directory_changes(handle.raw(), &mut buffer)?;
        if records.is_empty() {
            state.record_event(&source.id, 0, "directory_change_overflow", None, true)?;
            continue;
        }
        for record in records {
            if let Some(filter) = &file_filter {
                if record.rel_path.as_os_str() != filter {
                    continue;
                }
            }
            let rel_text = path_to_event_string(&record.rel_path);
            state.record_event(
                &source.id,
                record.action as u64,
                directory_action_to_kind(record.action),
                rel_text.as_deref(),
                false,
            )?;
        }
    }
    Ok(())
}

fn record_rescan_event(
    db_path: &Path,
    source_id: &str,
    kind: &str,
    rescan_required: bool,
) -> Result<()> {
    let state = State::open(db_path)?;
    state.record_event(source_id, 0, kind, None, rescan_required)?;
    Ok(())
}

#[derive(Debug)]
struct DirectoryChangeRecord {
    action: u32,
    rel_path: PathBuf,
}

#[derive(Debug)]
struct DirectoryWatchHandle(HANDLE);

impl DirectoryWatchHandle {
    fn open(path: &Path) -> Result<Self> {
        let handle = create_file(
            path.as_os_str(),
            FILE_LIST_DIRECTORY,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for DirectoryWatchHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn read_directory_changes(handle: HANDLE, buffer: &mut [u8]) -> Result<Vec<DirectoryChangeRecord>> {
    let mut bytes = 0_u32;
    let ok = unsafe {
        ReadDirectoryChangesW(
            handle,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            1,
            directory_notify_filter(),
            &mut bytes,
            ptr::null_mut(),
            None,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error()).context("ReadDirectoryChangesW failed");
    }
    if bytes == 0 {
        return Ok(Vec::new());
    }
    parse_directory_change_buffer(&buffer[..bytes as usize])
}

fn parse_directory_change_buffer(bytes: &[u8]) -> Result<Vec<DirectoryChangeRecord>> {
    let mut offset = 0_usize;
    let mut records = Vec::new();
    loop {
        if offset + FILE_NOTIFY_INFORMATION_HEADER_SIZE > bytes.len() {
            bail!("directory change record exceeds buffer");
        }
        let next_entry_offset =
            unsafe { ptr::read_unaligned(bytes[offset..].as_ptr().cast::<u32>()) };
        let action = unsafe { ptr::read_unaligned(bytes[offset + 4..].as_ptr().cast::<u32>()) };
        let name_bytes =
            unsafe { ptr::read_unaligned(bytes[offset + 8..].as_ptr().cast::<u32>()) } as usize;
        if name_bytes % 2 != 0 {
            bail!("directory change record has invalid file name length");
        }
        let name_offset = offset + FILE_NOTIFY_INFORMATION_HEADER_SIZE;
        if name_offset + name_bytes > bytes.len() {
            bail!("directory change file name exceeds buffer");
        }
        let name_ptr = unsafe { bytes.as_ptr().add(name_offset).cast::<u16>() };
        let name = std::ffi::OsString::from_wide(unsafe {
            slice::from_raw_parts(name_ptr, name_bytes / 2)
        });
        records.push(DirectoryChangeRecord {
            action,
            rel_path: PathBuf::from(name),
        });
        if next_entry_offset == 0 {
            break;
        }
        offset += next_entry_offset as usize;
    }
    Ok(records)
}

fn directory_notify_filter() -> u32 {
    FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION
        | FILE_NOTIFY_CHANGE_SECURITY
}

fn directory_action_to_kind(action: u32) -> &'static str {
    match action {
        1 => "create",
        2 => "delete",
        4 => "rename_old",
        5 => "rename_new",
        _ => "modify",
    }
}

#[derive(Debug)]
struct SourceWatch {
    id: String,
    root: PathBuf,
    volume: String,
    journal_id: u64,
    is_file: bool,
    root_file_id: u64,
    directories: HashMap<u64, PathBuf>,
}

impl SourceWatch {
    fn new(source: &SourceGroupConfig) -> Result<Self> {
        let root = source
            .src
            .canonicalize()
            .with_context(|| format!("failed to canonicalize source {}", source.src.display()))?;
        let info = file_information(&root)
            .with_context(|| format!("failed to read file id for {}", root.display()))?;
        let volume = volume_from_path(&root)?;
        let mut watch = Self {
            id: source.id.clone(),
            root,
            volume,
            journal_id: 0,
            is_file: !info.is_dir,
            root_file_id: info.file_id,
            directories: HashMap::new(),
        };
        watch.rebuild_directory_index()?;
        Ok(watch)
    }

    fn rebuild_directory_index(&mut self) -> Result<()> {
        self.directories.clear();
        if self.is_file {
            return Ok(());
        }
        for entry in WalkDir::new(&self.root).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_dir() {
                continue;
            }
            let info = file_information(entry.path()).with_context(|| {
                format!("failed to read file id for {}", entry.path().display())
            })?;
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or_else(|_| Path::new(""))
                .to_path_buf();
            self.directories.insert(info.file_id, rel);
        }
        Ok(())
    }

    fn persist_matching_record(&self, state: &State, record: &UsnRecord) -> Result<bool> {
        if self.is_file {
            if record.file_reference_number != self.root_file_id {
                return Ok(false);
            }
            let rel = self
                .root
                .file_name()
                .map(|name| name.to_string_lossy().to_string());
            let rescan = false;
            state.record_event(
                &self.id,
                record.reason as u64,
                reason_to_kind(record.reason),
                rel.as_deref(),
                rescan,
            )?;
            return Ok(rescan);
        }

        if record.file_reference_number == self.root_file_id {
            state.record_event(
                &self.id,
                record.reason as u64,
                reason_to_kind(record.reason),
                None,
                true,
            )?;
            return Ok(true);
        }

        let Some(parent_rel) = self.directories.get(&record.parent_file_reference_number) else {
            return Ok(false);
        };

        let rel = parent_rel.join(&record.file_name);
        let rel_text = path_to_event_string(&rel);
        let rescan = false;
        state.record_event(
            &self.id,
            record.reason as u64,
            reason_to_kind(record.reason),
            rel_text.as_deref(),
            rescan,
        )?;

        Ok(rescan || record.is_dir)
    }
}

#[derive(Debug, Clone, Copy)]
struct FileInformation {
    file_id: u64,
    is_dir: bool,
}

fn file_information(path: &Path) -> Result<FileInformation> {
    let handle = FileHandle::open_path(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let ok = unsafe { GetFileInformationByHandle(handle.raw(), &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("GetFileInformationByHandle failed for {}", path.display()));
    }
    Ok(FileInformation {
        file_id: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        is_dir: info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
    })
}

fn volume_from_path(path: &Path) -> Result<String> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        bail!(
            "Windows source path has no drive prefix: {}",
            path.display()
        );
    };
    let drive = match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive as char,
        _ => bail!(
            "USN Journal requires a local drive path: {}",
            path.display()
        ),
    };
    Ok(format!("{}:", drive.to_ascii_uppercase()))
}

#[derive(Debug)]
struct VolumeHandle(HANDLE);

impl VolumeHandle {
    fn open(volume: &str) -> Result<Self> {
        let path = format!(r"\\.\{volume}");
        let volume_path = OsStr::new(&path);
        let handle = create_file(volume_path, FILE_GENERIC_READ, 0)
            .with_context(|| format!("failed to open volume {path} for USN Journal access"))?;
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[derive(Debug)]
struct FileHandle(HANDLE);

impl FileHandle {
    fn open_path(path: &Path) -> Result<Self> {
        let handle = create_file(
            path.as_os_str(),
            FILE_READ_ATTRIBUTES,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for FileHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn create_file(path: &OsStr, access: u32, flags: u32) -> Result<HANDLE> {
    let wide = wide_null(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("CreateFileW failed");
    }
    Ok(handle)
}

fn query_usn_journal(handle: HANDLE) -> Result<USN_JOURNAL_DATA_V1> {
    let mut data = USN_JOURNAL_DATA_V1::default();
    let mut bytes = 0_u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            ptr::null(),
            0,
            (&mut data as *mut USN_JOURNAL_DATA_V1).cast(),
            mem::size_of::<USN_JOURNAL_DATA_V1>() as u32,
            &mut bytes,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error()).context("FSCTL_QUERY_USN_JOURNAL failed");
    }
    Ok(data)
}

#[derive(Debug)]
struct UsnBatch {
    next_usn: i64,
    records: Vec<UsnRecord>,
}

#[derive(Debug)]
enum UsnReadOutcome {
    Batch(UsnBatch),
    Gap(&'static str),
}

fn read_usn_batch(handle: HANDLE, journal_id: u64, start_usn: i64) -> Result<UsnReadOutcome> {
    let input = READ_USN_JOURNAL_DATA_V1 {
        StartUsn: start_usn,
        ReasonMask: reason_mask(),
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: journal_id,
        MinMajorVersion: 2,
        MaxMajorVersion: 2,
    };
    let mut buffer = vec![0_u8; USN_BUFFER_SIZE];
    let mut bytes = 0_u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_READ_USN_JOURNAL,
            (&input as *const READ_USN_JOURNAL_DATA_V1).cast(),
            mem::size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut bytes,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        match err.raw_os_error().map(|code| code as u32) {
            Some(ERROR_HANDLE_EOF) => {
                return Ok(UsnReadOutcome::Batch(UsnBatch {
                    next_usn: start_usn,
                    records: Vec::new(),
                }));
            }
            Some(ERROR_JOURNAL_ENTRY_DELETED) => {
                return Ok(UsnReadOutcome::Gap("usn_gap"));
            }
            Some(ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS) => {
                return Ok(UsnReadOutcome::Gap("usn_journal_unavailable"));
            }
            _ => {}
        }
        return Err(err).context("FSCTL_READ_USN_JOURNAL failed");
    }
    parse_usn_buffer(&buffer[..bytes as usize]).map(UsnReadOutcome::Batch)
}

fn parse_usn_buffer(bytes: &[u8]) -> Result<UsnBatch> {
    if bytes.len() < mem::size_of::<i64>() {
        return Ok(UsnBatch {
            next_usn: 0,
            records: Vec::new(),
        });
    }
    let next_usn = unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<i64>()) };
    let mut offset = mem::size_of::<i64>();
    let mut records = Vec::new();
    while offset + mem::size_of::<USN_RECORD_COMMON_HEADER>() <= bytes.len() {
        let header = unsafe {
            ptr::read_unaligned(bytes[offset..].as_ptr().cast::<USN_RECORD_COMMON_HEADER>())
        };
        if header.RecordLength == 0 {
            break;
        }
        let record_len = header.RecordLength as usize;
        if offset + record_len > bytes.len() {
            bail!("USN record length exceeds buffer");
        }
        if header.MajorVersion == 2 {
            records.push(parse_usn_record_v2(&bytes[offset..offset + record_len])?);
        }
        offset += record_len;
    }
    Ok(UsnBatch { next_usn, records })
}

#[derive(Debug)]
struct UsnRecord {
    file_reference_number: u64,
    parent_file_reference_number: u64,
    reason: u32,
    is_dir: bool,
    file_name: String,
}

fn parse_usn_record_v2(bytes: &[u8]) -> Result<UsnRecord> {
    if bytes.len() < mem::size_of::<USN_RECORD_V2>() {
        bail!("USN v2 record is too short");
    }
    let record = unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<USN_RECORD_V2>()) };
    let name_offset = record.FileNameOffset as usize;
    let name_bytes = record.FileNameLength as usize;
    if name_offset + name_bytes > bytes.len() || name_bytes % 2 != 0 {
        bail!("USN v2 record has invalid file name range");
    }
    let name_ptr = unsafe { bytes.as_ptr().add(name_offset).cast::<u16>() };
    let name = String::from_utf16_lossy(unsafe { slice::from_raw_parts(name_ptr, name_bytes / 2) });
    Ok(UsnRecord {
        file_reference_number: record.FileReferenceNumber,
        parent_file_reference_number: record.ParentFileReferenceNumber,
        reason: record.Reason,
        is_dir: record.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        file_name: name,
    })
}

fn reason_mask() -> u32 {
    USN_REASON_DATA_OVERWRITE
        | USN_REASON_DATA_EXTEND
        | USN_REASON_DATA_TRUNCATION
        | USN_REASON_NAMED_DATA_OVERWRITE
        | USN_REASON_NAMED_DATA_EXTEND
        | USN_REASON_NAMED_DATA_TRUNCATION
        | USN_REASON_FILE_CREATE
        | USN_REASON_FILE_DELETE
        | USN_REASON_RENAME_OLD_NAME
        | USN_REASON_RENAME_NEW_NAME
        | USN_REASON_BASIC_INFO_CHANGE
        | USN_REASON_SECURITY_CHANGE
        | USN_REASON_EA_CHANGE
        | USN_REASON_COMPRESSION_CHANGE
        | USN_REASON_ENCRYPTION_CHANGE
        | USN_REASON_REPARSE_POINT_CHANGE
        | USN_REASON_STREAM_CHANGE
        | USN_REASON_HARD_LINK_CHANGE
        | USN_REASON_INDEXABLE_CHANGE
}

fn reason_to_kind(reason: u32) -> &'static str {
    if reason & USN_REASON_FILE_DELETE != 0 {
        "delete"
    } else if reason & USN_REASON_RENAME_OLD_NAME != 0 {
        "rename_old"
    } else if reason & USN_REASON_RENAME_NEW_NAME != 0 {
        "rename_new"
    } else if reason & USN_REASON_FILE_CREATE != 0 {
        "create"
    } else if reason
        & (USN_REASON_DATA_OVERWRITE
            | USN_REASON_DATA_EXTEND
            | USN_REASON_DATA_TRUNCATION
            | USN_REASON_NAMED_DATA_OVERWRITE
            | USN_REASON_NAMED_DATA_EXTEND
            | USN_REASON_NAMED_DATA_TRUNCATION)
        != 0
    {
        "modify"
    } else {
        "metadata"
    }
}

fn path_to_event_string(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path.to_string_lossy().to_string())
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn sleep_until_retry(shutdown: &AtomicBool) {
    sleep_interruptible(shutdown, RETRY_INTERVAL);
}

fn sleep_interruptible(shutdown: &AtomicBool, duration: Duration) {
    let mut slept = Duration::ZERO;
    while slept < duration && !shutdown.load(Ordering::SeqCst) {
        let step = Duration::from_millis(200).min(duration - slept);
        thread::sleep(step);
        slept += step;
    }
}

fn format_error_chain(err: &anyhow::Error) -> String {
    err.chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

fn error_has_raw_os_error(err: &anyhow::Error, code: i32) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .and_then(io::Error::raw_os_error)
            == Some(code)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_drive_volume_from_normal_path() {
        assert_eq!(volume_from_path(Path::new(r"C:\data\src")).unwrap(), "C:");
    }

    #[test]
    fn maps_usn_reason_to_kind() {
        assert_eq!(reason_to_kind(USN_REASON_FILE_DELETE), "delete");
        assert_eq!(reason_to_kind(USN_REASON_RENAME_OLD_NAME), "rename_old");
        assert_eq!(reason_to_kind(USN_REASON_RENAME_NEW_NAME), "rename_new");
        assert_eq!(reason_to_kind(USN_REASON_DATA_EXTEND), "modify");
    }

    #[test]
    fn parses_directory_change_records() {
        let mut bytes = directory_change_record_bytes(1, "a.txt", 24);
        bytes.extend(directory_change_record_bytes(2, "nested\\b.txt", 0));

        let records = parse_directory_change_buffer(&bytes).unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(directory_action_to_kind(records[0].action), "create");
        assert_eq!(records[0].rel_path, PathBuf::from("a.txt"));
        assert_eq!(directory_action_to_kind(records[1].action), "delete");
        assert_eq!(records[1].rel_path, PathBuf::from("nested\\b.txt"));
    }

    fn directory_change_record_bytes(action: u32, name: &str, next_offset: u32) -> Vec<u8> {
        let wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().collect();
        let mut bytes = Vec::new();
        bytes.extend(next_offset.to_le_bytes());
        bytes.extend(action.to_le_bytes());
        bytes.extend(((wide.len() * 2) as u32).to_le_bytes());
        for unit in wide {
            bytes.extend(unit.to_le_bytes());
        }
        if next_offset > 0 {
            bytes.resize(next_offset as usize, 0);
        }
        bytes
    }
}
