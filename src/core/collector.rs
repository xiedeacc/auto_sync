//! File collector: pull files/directories from arbitrary SSH hosts into a
//! single local git repository, split oversized files so they fit git-hosting
//! limits, then commit and push.
//!
//! Everything shells out to the system `ssh`/`scp`/`git` — there is no SSH
//! client crate — so it works with the built-in OpenSSH on Windows and the
//! stock tools on Linux. Each host is described by structured fields
//! (`HostName`/`User`/`Port`/`IdentityFile`), and the engine builds the
//! `ssh`/`scp` commands with explicit `-i`/`-p` flags — no ssh config file.
//! The whole feature is UI-driven (the "Collector" button): nothing here runs
//! on a schedule.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::core::config::{CollectorConfig, CollectorHost};

/// Marker inserted between a split file's name and its part index, e.g.
/// `big.bin.autosplit.000`. Files carrying it are skipped by later splits.
const AUTOSPLIT_MARKER: &str = ".autosplit.";

/// Non-interactive ssh/scp options: never prompt for a password, auto-trust a
/// new host key (LAN/known infra), and cap the connect wait so a dead host
/// fails fast instead of hanging the run.
const SSH_OPTS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ConnectTimeout=15",
];

/// Live state of a collector run, polled by the UI.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CollectorRunState {
    pub running: bool,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// None while running; Some(true/false) once finished.
    pub ok: Option<bool>,
    pub log: Vec<String>,
}

/// One entry returned by the remote path browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectorBrowseEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// Result of browsing a remote directory over ssh.
#[derive(Debug, Clone, Serialize)]
pub struct CollectorBrowseResponse {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<CollectorBrowseEntry>,
}

/// The connection parameters a browse request carries (a subset of
/// `CollectorHost`). Sent by the UI so it can browse a host before it is saved.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CollectorBrowseRequest {
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub identity_file: String,
    #[serde(default)]
    pub path: String,
}

/// SSH connection view shared by run and browse.
struct SshConn<'a> {
    hostname: &'a str,
    user: &'a str,
    port: u16,
    identity_file: &'a str,
}

impl<'a> SshConn<'a> {
    fn from_host(host: &'a CollectorHost) -> Self {
        Self {
            hostname: host.hostname.trim(),
            user: host.user.trim(),
            port: host.port,
            identity_file: host.identity_file.trim(),
        }
    }

    fn from_request(req: &'a CollectorBrowseRequest) -> Self {
        Self {
            hostname: req.hostname.trim(),
            user: req.user.trim(),
            port: req.port,
            identity_file: req.identity_file.trim(),
        }
    }

    /// `user@host` (or just `host` when no user is set).
    fn dest(&self) -> String {
        if self.user.is_empty() {
            self.hostname.to_string()
        } else {
            format!("{}@{}", self.user, self.hostname)
        }
    }
}

fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Expand a leading `~` / `~/` to the user's home directory (ssh does this for
/// config files, but we pass identity paths as explicit `-i` args).
fn expand_tilde(path: &str) -> String {
    let path = path.trim();
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        if !home.is_empty() {
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

/// Build an `ssh` command for `conn` (port via `-p`, key via `-i`).
fn ssh_command(conn: &SshConn) -> Command {
    let mut cmd = command("ssh");
    if conn.port != 0 {
        cmd.arg("-p").arg(conn.port.to_string());
    }
    if !conn.identity_file.is_empty() {
        cmd.arg("-i").arg(expand_tilde(conn.identity_file));
    }
    cmd.args(SSH_OPTS);
    cmd
}

/// Build an `scp` command for `conn` (port via `-P`, key via `-i`).
fn scp_command(conn: &SshConn) -> Command {
    let mut cmd = command("scp");
    if conn.port != 0 {
        cmd.arg("-P").arg(conn.port.to_string());
    }
    if !conn.identity_file.is_empty() {
        cmd.arg("-i").arg(expand_tilde(conn.identity_file));
    }
    cmd.args(SSH_OPTS);
    cmd
}

fn log(state: &Arc<Mutex<CollectorRunState>>, msg: impl Into<String>) {
    let msg = msg.into();
    let line = format!("{} {}", chrono::Local::now().format("%H:%M:%S"), msg);
    tracing::info!(target: "collector", "{}", msg);
    if let Ok(mut guard) = state.lock() {
        guard.log.push(line);
    }
}

/// Entry point for a background run. Drives the whole pipeline and records the
/// final status into `state`. Never panics — every failure becomes a log line.
pub fn run(cfg: CollectorConfig, state: Arc<Mutex<CollectorRunState>>) {
    let ok = match run_inner(&cfg, &state) {
        Ok(0) => {
            log(&state, "Collector finished: all paths pulled.");
            true
        }
        Ok(failures) => {
            log(&state, format!("Collector finished with {failures} failed path(s)."));
            false
        }
        Err(err) => {
            log(&state, format!("ERROR: {err:#}"));
            false
        }
    };
    if let Ok(mut guard) = state.lock() {
        guard.running = false;
        guard.ok = Some(ok);
        guard.finished_at = Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
    }
}

/// Runs the pipeline, returning the number of remote paths that failed to
/// pull (a hard/setup error still returns `Err`).
fn run_inner(cfg: &CollectorConfig, state: &Arc<Mutex<CollectorRunState>>) -> Result<usize> {
    let git_dir = cfg.git_dir.clone();
    if git_dir.as_os_str().is_empty() {
        bail!("git repository directory is not set");
    }
    fs::create_dir_all(&git_dir)
        .with_context(|| format!("creating git dir {}", git_dir.display()))?;
    if !git_dir.join(".git").exists() {
        log(state, format!("git init {}", git_dir.display()));
        run_git(&git_dir, &["init"], state)?;
    }

    let enabled: Vec<&CollectorHost> = cfg.hosts.iter().filter(|h| h.enabled).collect();
    if enabled.is_empty() {
        bail!("no enabled hosts to pull from");
    }

    let mut failures = 0;
    for host in enabled {
        failures += pull_host(host, state);
    }

    if cfg.split_threshold_mb > 0 {
        split_large_files(&git_dir, cfg.split_threshold_mb, state)?;
    }

    if cfg.auto_commit_push {
        commit_and_push(&git_dir, state)?;
    } else {
        log(state, "auto commit/push disabled; leaving working tree as-is");
    }

    Ok(failures)
}

/// Pull every configured path for one host, returning the failure count. Host
/// setup problems degrade to logged failures rather than aborting the run.
fn pull_host(host: &CollectorHost, state: &Arc<Mutex<CollectorRunState>>) -> usize {
    let label = if host.name.trim().is_empty() {
        host.hostname.clone()
    } else {
        host.name.clone()
    };
    if host.hostname.trim().is_empty() {
        log(state, format!("host '{label}': no hostname, skipping"));
        return host.paths.len();
    }
    if host.root.as_os_str().is_empty() {
        log(state, format!("host '{label}': no local root, skipping"));
        return host.paths.len();
    }
    let conn = SshConn::from_host(host);
    log(state, format!("=== host {label} ({}) ===", conn.dest()));

    let mut failures = 0;
    for path in &host.paths {
        let remote = path.trim();
        if remote.is_empty() {
            continue;
        }
        match pull_path(host, &conn, remote, state) {
            Ok(()) => log(state, format!("  ok {remote}")),
            Err(err) => {
                failures += 1;
                log(state, format!("  FAILED {remote}: {err:#}"));
            }
        }
    }
    failures
}

/// Pull one remote path, reconstructing its directory structure under the
/// host's local root: root `D:\share\linux\aws` + remote `/usr/local/x` gives
/// `D:\share\linux\aws\usr\local\x`.
fn pull_path(
    host: &CollectorHost,
    conn: &SshConn,
    remote: &str,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    let parent_components = remote_parent_components(remote)?;
    let mut local_parent = host.root.clone();
    for component in &parent_components {
        local_parent.push(component);
    }
    fs::create_dir_all(&local_parent)
        .with_context(|| format!("creating {}", local_parent.display()))?;
    log(state, format!("  scp {}:{} -> {}", conn.dest(), remote, local_parent.display()));

    let mut cmd = scp_command(conn);
    cmd.arg("-r").arg("-p");
    cmd.arg(format!("{}:{}", conn.dest(), remote));
    cmd.arg(&local_parent);
    let output = cmd.output().context("running scp")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("scp failed: {}", stderr.trim());
    }
    Ok(())
}

/// Split every file at or above `threshold_mb` MiB under `git_dir` into
/// `<name>.autosplit.NNN` parts, git-ignore the original, and keep the parts.
fn split_large_files(
    git_dir: &Path,
    threshold_mb: u64,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    let threshold = threshold_mb.saturating_mul(1024 * 1024);
    if threshold == 0 {
        return Ok(());
    }
    let mut ignore = GitignoreEditor::load(git_dir)?;
    for entry in WalkDir::new(git_dir)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        if name.contains(AUTOSPLIT_MARKER) {
            continue; // already a part
        }
        let len = match entry.metadata() {
            Ok(meta) => meta.len(),
            Err(_) => continue,
        };
        if len < threshold {
            continue;
        }
        let parts = split_file(path, threshold)?;
        if let Ok(rel) = path.strip_prefix(git_dir) {
            ignore.add(rel);
        }
        log(
            state,
            format!("  split {} ({} MiB) into {} part(s)", name, len / 1024 / 1024, parts),
        );
    }
    ignore.save()?;
    Ok(())
}

/// Split one file into fixed-size parts, returning the part count. Any stale
/// parts from a previous run are removed first so the set stays consistent.
fn split_file(path: &Path, part_size: u64) -> Result<usize> {
    remove_existing_parts(path)?;
    let mut input = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let total = input.metadata()?.len();
    let part_count = total.div_ceil(part_size).max(1) as usize;
    let width = format!("{}", part_count.saturating_sub(1)).len().max(3);

    let mut buf = vec![0u8; 8 * 1024 * 1024];
    for index in 0..part_count {
        let mut part_name = path.as_os_str().to_owned();
        part_name.push(format!("{AUTOSPLIT_MARKER}{index:0width$}"));
        let part_path = PathBuf::from(part_name);
        let mut out =
            File::create(&part_path).with_context(|| format!("creating {}", part_path.display()))?;
        let mut written = 0u64;
        while written < part_size {
            let want = std::cmp::min(buf.len() as u64, part_size - written) as usize;
            let read = input.read(&mut buf[..want])?;
            if read == 0 {
                break;
            }
            out.write_all(&buf[..read])?;
            written += read as u64;
        }
        out.flush()?;
    }
    Ok(part_count)
}

/// Remove any `<name>.autosplit.*` siblings of `path` (stale parts).
fn remove_existing_parts(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(file_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Ok(());
    };
    let prefix = format!("{file_name}{AUTOSPLIT_MARKER}");
    if let Ok(dir) = fs::read_dir(parent) {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// `git add -A`, commit (only if something changed), then best-effort push.
fn commit_and_push(git_dir: &Path, state: &Arc<Mutex<CollectorRunState>>) -> Result<()> {
    run_git(git_dir, &["add", "-A"], state)?;
    let status = git_capture(git_dir, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        log(state, "nothing to commit");
    } else {
        let message = format!("collector: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        let mut args: Vec<String> = Vec::new();
        // Supply a fallback identity only if the repo/global config lacks one,
        // so an unconfigured git does not hard-fail the commit.
        if git_capture(git_dir, &["config", "user.email"]).unwrap_or_default().trim().is_empty() {
            args.push("-c".into());
            args.push("user.email=collector@auto_sync".into());
            args.push("-c".into());
            args.push("user.name=auto_sync collector".into());
        }
        args.push("commit".into());
        args.push("-m".into());
        args.push(message.clone());
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_git(git_dir, &refs, state)?;
        log(state, format!("committed: {message}"));
    }

    let (ok, output) = git_status(git_dir, &["push"])?;
    for line in output.lines().take(20) {
        log(state, format!("  git push | {line}"));
    }
    if ok {
        log(state, "pushed");
    } else {
        log(state, "git push returned non-zero (check remote/upstream)");
    }
    Ok(())
}

/// Browse a remote directory over ssh using the request's connection
/// parameters.
pub fn browse(req: &CollectorBrowseRequest) -> Result<CollectorBrowseResponse> {
    let conn = SshConn::from_request(req);
    if conn.hostname.is_empty() {
        bail!("no hostname to connect to");
    }
    let path = {
        let trimmed = req.path.trim();
        if trimmed.is_empty() { "/" } else { trimmed }
    };
    // List entries and mark directories with a trailing `/`. We use a `[ -d ]`
    // test rather than `ls -p` because `ls -p` only flags *real* directories —
    // a symlink pointing at a directory would otherwise show up as a file.
    // `[ -d ]` follows the symlink, so those are classified correctly. POSIX
    // sh, works under bash and busybox ash alike.
    let remote_cmd = format!(
        "cd -- {} 2>/dev/null || exit 0; \
for f in * .*; do \
[ \"$f\" = . ] && continue; \
[ \"$f\" = .. ] && continue; \
[ -e \"$f\" ] || [ -L \"$f\" ] || continue; \
if [ -d \"$f\" ]; then printf '%s/\\n' \"$f\"; else printf '%s\\n' \"$f\"; fi; \
done",
        shell_quote(path)
    );
    let out = ssh_capture(&conn, &remote_cmd)?;
    let mut entries = Vec::new();
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let is_dir = line.ends_with('/');
        let name = line.trim_end_matches('/').to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let child = join_remote(path, &name);
        entries.push(CollectorBrowseEntry { name, path: child, is_dir });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(CollectorBrowseResponse { path: path.to_string(), parent: remote_parent(path), entries })
}

// --- ssh / git command helpers ------------------------------------------------

/// Run a remote command over ssh, returning stdout (fails on non-zero exit).
fn ssh_capture(conn: &SshConn, remote_cmd: &str) -> Result<String> {
    let mut cmd = ssh_command(conn);
    cmd.arg(conn.dest()).arg(remote_cmd);
    let output = cmd.output().context("running ssh")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ssh failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git(git_dir: &Path) -> Command {
    let mut cmd = command("git");
    cmd.current_dir(git_dir);
    cmd
}

/// Run a git subcommand, logging stderr; bail on non-zero exit.
fn run_git(git_dir: &Path, args: &[&str], state: &Arc<Mutex<CollectorRunState>>) -> Result<()> {
    let output = git(git_dir).args(args).output().context("running git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines().take(20) {
            log(state, format!("  git {} | {line}", args.first().copied().unwrap_or("")));
        }
        bail!("git {} failed", args.first().copied().unwrap_or(""));
    }
    Ok(())
}

/// Run a git subcommand, returning stdout (fails on non-zero exit).
fn git_capture(git_dir: &Path, args: &[&str]) -> Result<String> {
    let output = git(git_dir).args(args).output().context("running git")?;
    if !output.status.success() {
        bail!("git {} failed", args.first().copied().unwrap_or(""));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a git subcommand, returning (success, stdout+stderr) — never bails.
fn git_status(git_dir: &Path, args: &[&str]) -> Result<(bool, String)> {
    let output = git(git_dir).args(args).output().context("running git")?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.success(), combined))
}

// --- path helpers -------------------------------------------------------------

/// The parent path components of a remote absolute path, sanitized so nothing
/// can escape the local root. `/usr/local/x` -> ["usr", "local"].
fn remote_parent_components(remote: &str) -> Result<Vec<String>> {
    let trimmed = remote.trim().trim_end_matches('/');
    let mut components: Vec<String> = trimmed
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(|c| c.to_string())
        .collect();
    if components.is_empty() {
        bail!("refusing to pull the remote root '/'");
    }
    if components.iter().any(|c| c == "..") {
        bail!("remote path must not contain '..'");
    }
    components.pop(); // drop the leaf; scp recreates it under the parent
    Ok(components)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn join_remote(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", base.trim_end_matches('/'), name)
    }
}

fn remote_parent(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) => Some("/".to_string()),
        Some((parent, _)) => Some(parent.to_string()),
        None => Some("/".to_string()),
    }
}

/// Reads, dedups, and rewrites `.gitignore` entries for split originals.
struct GitignoreEditor {
    path: PathBuf,
    lines: Vec<String>,
    dirty: bool,
}

impl GitignoreEditor {
    fn load(git_dir: &Path) -> Result<Self> {
        let path = git_dir.join(".gitignore");
        let lines = match fs::read_to_string(&path) {
            Ok(text) => text.lines().map(|l| l.to_string()).collect(),
            Err(_) => Vec::new(),
        };
        Ok(Self { path, lines, dirty: false })
    }

    /// Add a repo-relative path (anchored, forward-slashed) if not already present.
    fn add(&mut self, rel: &Path) {
        let mut entry = String::from("/");
        entry.push_str(&rel.to_string_lossy().replace('\\', "/"));
        if !self.lines.iter().any(|l| l.trim() == entry) {
            self.lines.push(entry);
            self.dirty = true;
        }
    }

    fn save(&self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let mut body = self.lines.join("\n");
        body.push('\n');
        fs::write(&self.path, body)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_parent_components_reconstructs_structure() {
        assert_eq!(remote_parent_components("/usr/local/shadowsocks").unwrap(), vec!["usr", "local"]);
        assert_eq!(remote_parent_components("/etc").unwrap(), Vec::<String>::new());
        assert_eq!(remote_parent_components("/a/b/c/").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn remote_parent_components_rejects_escapes() {
        assert!(remote_parent_components("/").is_err());
        assert!(remote_parent_components("/a/../b").is_err());
    }

    #[test]
    fn remote_navigation_helpers() {
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(join_remote("/usr/local", "bin"), "/usr/local/bin");
        assert_eq!(remote_parent("/"), None);
        assert_eq!(remote_parent("/etc"), Some("/".to_string()));
        assert_eq!(remote_parent("/usr/local/bin"), Some("/usr/local".to_string()));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/a b"), "'/a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn ssh_dest_builds_user_at_host() {
        let host = CollectorHost {
            hostname: "1.2.3.4".to_string(),
            user: "root".to_string(),
            ..Default::default()
        };
        assert_eq!(SshConn::from_host(&host).dest(), "root@1.2.3.4");
        let no_user = CollectorHost { hostname: "h".to_string(), ..Default::default() };
        assert_eq!(SshConn::from_host(&no_user).dest(), "h");
    }

    #[test]
    fn split_and_parts_roundtrip() {
        let dir = std::env::temp_dir().join(format!("collector_split_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("blob.bin");
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        fs::write(&file, &data).unwrap();
        // part size 256 => 4 parts (256*3 + 232)
        let parts = split_file(&file, 256).unwrap();
        assert_eq!(parts, 4);
        let mut rejoined = Vec::new();
        for i in 0..parts {
            let part = dir.join(format!("blob.bin.autosplit.{i:03}"));
            rejoined.extend(fs::read(&part).unwrap());
        }
        assert_eq!(rejoined, data);
        // Re-splitting removes stale parts (fewer parts with a bigger size).
        let parts2 = split_file(&file, 600).unwrap();
        assert_eq!(parts2, 2);
        assert!(!dir.join("blob.bin.autosplit.003").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gitignore_editor_anchors_and_dedups() {
        let dir = std::env::temp_dir().join(format!("collector_ignore_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let mut editor = GitignoreEditor::load(&dir).unwrap();
        editor.add(Path::new("aws/usr/local/big.bin"));
        editor.add(Path::new("aws/usr/local/big.bin")); // duplicate
        editor.save().unwrap();
        let body = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(body.matches("/aws/usr/local/big.bin").count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
