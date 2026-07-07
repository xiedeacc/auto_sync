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
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::core::config::{CollectorConfig, CollectorHost};

/// Marker inserted between a split file's name and its part index, e.g.
/// `big.bin.autosplit.000`. Files carrying it are skipped by later splits.
const AUTOSPLIT_MARKER: &str = ".autosplit.";

/// Per-host permission cache written under the host's local root at collect
/// time and replayed by `deploy` (Windows can't preserve Unix modes).
const PERMS_FILE: &str = ".auto_sync_perms";

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
    let mut pulled = Vec::new();
    for path in &host.paths {
        let remote = path.trim();
        if remote.is_empty() {
            continue;
        }
        match pull_path(host, &conn, remote, state) {
            Ok(()) => {
                log(state, format!("  ok {remote}"));
                pulled.push(remote.to_string());
            }
            Err(err) => {
                failures += 1;
                log(state, format!("  FAILED {remote}: {err:#}"));
            }
        }
    }
    // Windows can't preserve Unix modes on the pulled files, so capture them
    // from the remote into a per-host cache that `deploy` replays.
    if !pulled.is_empty() {
        if let Err(err) = record_host_perms(host, &conn, &pulled, state) {
            log(state, format!("  perms: could not record: {err:#}"));
        }
    }
    failures
}

/// Capture the Unix mode of every file/dir under each pulled path and write a
/// per-host cache `<root>/.auto_sync_perms` of `mode path` lines. `deploy`
/// replays these because Windows can't hold the modes on the local copies.
/// Prefers `stat -c '%a %n'` (gives the octal mode directly); falls back to
/// parsing `ls -ldn` on hosts where `stat` is not installed yet.
fn record_host_perms(
    host: &CollectorHost,
    conn: &SshConn,
    paths: &[String],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    let mut lines: Vec<String> = Vec::new();
    for path in paths {
        let q = shell_quote(path);
        let cmd = format!(
            "if command -v stat >/dev/null 2>&1; then \
find {q} \\( -type f -o -type d \\) -exec stat -c '%a %n' {{}} \\; 2>/dev/null; \
else \
find {q} \\( -type f -o -type d \\) -exec ls -ldn {{}} \\; 2>/dev/null; \
fi"
        );
        let out = match ssh_capture(conn, &cmd) {
            Ok(out) => out,
            Err(err) => {
                log(state, format!("  perms: {path}: {err:#}"));
                continue;
            }
        };
        for line in out.lines() {
            if let Some((mode, entry_path)) = parse_perm_line(line) {
                lines.push(format!("{mode} {entry_path}"));
            }
        }
    }
    lines.sort();
    lines.dedup();
    let file = host.root.join(PERMS_FILE);
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(&file, body).with_context(|| format!("writing {}", file.display()))?;
    log(state, format!("  perms: recorded {} entries", lines.len()));
    Ok(())
}

/// Parse one permission line into (octal_mode, absolute_path). Accepts both
/// `stat -c '%a %n'` output (`755 /path` — mode already octal) and `ls -ldn`
/// output (`drwxr-xr-x … /path` — symbolic, may carry setuid/setgid/sticky).
fn parse_perm_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    // stat format: an octal mode token, then the path (rest of the line).
    if let Some((first, rest)) = line.split_once(char::is_whitespace) {
        let rest = rest.trim();
        if !first.is_empty()
            && first.len() <= 5
            && first.bytes().all(|b| b.is_ascii_digit())
            && rest.starts_with('/')
        {
            return Some((first.to_string(), rest.to_string()));
        }
    }
    // ls -ldn fallback: symbolic perms first, absolute path last.
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }
    let sym = parts[0].as_bytes();
    if sym.len() < 10 {
        return None;
    }
    let path = *parts.last()?;
    if !path.starts_with('/') {
        return None;
    }
    let p = &sym[1..10];
    let mut mode = 0u32;
    if p[0] == b'r' { mode |= 0o400; }
    if p[1] == b'w' { mode |= 0o200; }
    match p[2] { b'x' => mode |= 0o100, b's' => mode |= 0o4100, b'S' => mode |= 0o4000, _ => {} }
    if p[3] == b'r' { mode |= 0o40; }
    if p[4] == b'w' { mode |= 0o20; }
    match p[5] { b'x' => mode |= 0o10, b's' => mode |= 0o2010, b'S' => mode |= 0o2000, _ => {} }
    if p[6] == b'r' { mode |= 0o4; }
    if p[7] == b'w' { mode |= 0o2; }
    match p[8] { b'x' => mode |= 0o1, b't' => mode |= 0o1001, b'T' => mode |= 0o1000, _ => {} }
    Some((format!("{mode:o}"), path.to_string()))
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

// --- deploy (run the host's deploy script on Windows) -------------------------

/// Deploy a single host by running its deploy script **on this machine**
/// (Windows), not on the remote host. The script drives the bundled `ssh`/`scp`
/// itself — it decides what to push, how to fix permissions, how to restart
/// services. The engine only hands it a ready-made environment:
///   AS_SSH / AS_SCP        the ssh/scp binaries to use (bundled, else PATH)
///   AS_HOSTNAME / AS_USER / AS_DEST / AS_PORT / AS_KEY   this host's connection
///   AS_ROOT                the host's local collected root (e.g. D:\share\openwrt)
///   AS_HOST_<NAME>         every collector host's hostname (so a script can, e.g.,
///                          read AS_HOST_AWS to substitute a server address)
pub fn deploy(host: CollectorHost, all_hosts: Vec<CollectorHost>, state: Arc<Mutex<CollectorRunState>>) {
    let ok = match deploy_inner(&host, &all_hosts, &state) {
        Ok(0) => {
            log(&state, "Deploy finished.");
            true
        }
        Ok(n) => {
            log(&state, format!("Deploy finished with {n} error(s)."));
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

fn deploy_inner(
    host: &CollectorHost,
    all_hosts: &[CollectorHost],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<usize> {
    if host.hostname.trim().is_empty() {
        bail!("host has no hostname");
    }
    if host.root.as_os_str().is_empty() {
        bail!("host has no local root");
    }
    let conn = SshConn::from_host(host);
    let label = if host.name.trim().is_empty() {
        host.hostname.clone()
    } else {
        host.name.clone()
    };
    log(state, format!("=== deploy {label} ({}) — running on this machine ===", conn.dest()));

    if host.deploy_script.trim().is_empty() {
        log(state, "no deploy script for this host — nothing to do");
        return Ok(0);
    }

    match run_local_deploy_script(host, &conn, all_hosts, &host.deploy_script, state) {
        Ok(true) => {
            log(state, "deploy script ok");
            Ok(0)
        }
        Ok(false) => {
            log(state, "deploy script exited non-zero");
            Ok(1)
        }
        Err(err) => {
            log(state, format!("deploy script error: {err:#}"));
            Ok(1)
        }
    }
}

/// True if `s` looks like a dotted IPv4 literal (four 0-255 octets).
fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.trim().split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().map(|n| n <= 255).unwrap_or(false)
        })
}

/// Rewrite a shadowsocks-rust client config so the `server` field of the entry
/// matching `new_server`'s address family is set to `new_server`: an IPv4
/// hostname updates the IPv4 server entry, an IPv6 hostname the IPv6 one. The
/// file is parsed as JSON and re-serialized, so no brittle text munging is
/// involved. Returns the new JSON text; errs if the family has no server entry.
fn substitute_ss_server(conf_text: &str, new_server: &str) -> Result<String> {
    let new_server = new_server.trim();
    let want_v4 = is_ipv4(new_server);
    let want_v6 = !want_v4 && new_server.contains(':');
    if !want_v4 && !want_v6 {
        bail!("aws hostname '{new_server}' is neither IPv4 nor IPv6; not substituting");
    }
    let mut root: serde_json::Value =
        serde_json::from_str(conf_text).context("parsing shadowsocks client json")?;
    let mut changed = 0usize;
    if let Some(servers) = root.get_mut("servers").and_then(|v| v.as_array_mut()) {
        for entry in servers.iter_mut() {
            let Some(cur) = entry.get("server").and_then(|v| v.as_str()) else {
                continue;
            };
            let entry_v4 = is_ipv4(cur);
            let entry_v6 = !entry_v4 && cur.contains(':');
            let matches = (want_v4 && entry_v4) || (want_v6 && entry_v6);
            if matches && cur != new_server {
                entry["server"] = serde_json::Value::String(new_server.to_string());
                changed += 1;
            } else if matches {
                changed += 1; // already correct, but the family entry exists
            }
        }
    }
    if changed == 0 {
        bail!(
            "no {} server entry found in the shadowsocks config",
            if want_v4 { "IPv4" } else { "IPv6" }
        );
    }
    serde_json::to_string_pretty(&root).context("serializing shadowsocks client json")
}

/// Prefer a bundled OpenSSH binary shipped next to the running executable
/// (`<exe dir>\windows\openssh\OpenSSH-Win64\<tool>.exe`), falling back to the
/// system tool on PATH.
fn resolve_ssh_tool(tool: &str) -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let exe_name = format!("{tool}.exe");
            let candidates = [
                dir.join("windows").join("openssh").join("OpenSSH-Win64").join(&exe_name),
                dir.join("openssh").join("OpenSSH-Win64").join(&exe_name),
                dir.join(&exe_name),
            ];
            for cand in candidates {
                if cand.exists() {
                    return cand.to_string_lossy().into_owned();
                }
            }
        }
    }
    tool.to_string()
}

/// Turn a host name into an env-var-safe suffix: `AS_HOST_<UPPER>` where every
/// non-alphanumeric character becomes `_`.
fn env_host_suffix(name: &str) -> String {
    let mut s: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// Write the deploy script to a temp file and run it locally (PowerShell on
/// Windows), streaming its output into the run log.
fn run_local_deploy_script(
    host: &CollectorHost,
    conn: &SshConn,
    all_hosts: &[CollectorHost],
    script: &str,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<bool> {
    let file_tag = env_host_suffix(&host.name).to_lowercase();
    let script_path = std::env::temp_dir().join(format!("auto_sync_deploy_{file_tag}.ps1"));
    fs::write(&script_path, script)
        .with_context(|| format!("writing {}", script_path.display()))?;

    #[cfg(windows)]
    let mut cmd = {
        let mut c = command("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = command("pwsh");
        c.args(["-NoProfile", "-NonInteractive", "-File"]);
        c
    };
    cmd.arg(&script_path);

    cmd.env("AS_SSH", resolve_ssh_tool("ssh"));
    cmd.env("AS_SCP", resolve_ssh_tool("scp"));
    cmd.env("AS_HOSTNAME", conn.hostname);
    cmd.env("AS_USER", conn.user);
    cmd.env("AS_DEST", conn.dest());
    cmd.env("AS_PORT", if conn.port == 0 { String::new() } else { conn.port.to_string() });
    cmd.env("AS_KEY", if conn.identity_file.is_empty() { String::new() } else { expand_tilde(conn.identity_file) });
    cmd.env("AS_ROOT", host.root.as_os_str());
    for other in all_hosts {
        cmd.env(format!("AS_HOST_{}", env_host_suffix(&other.name)), other.hostname.trim());
    }

    // Hand the script the per-host permission cache captured at collect time so
    // it can restore modes Windows dropped.
    let perms_file = host.root.join(PERMS_FILE);
    if perms_file.exists() {
        cmd.env("AS_PERMS_FILE", perms_file.as_os_str());
    }

    // If this host collected a shadowsocks client config, rewrite its server
    // address (family-matched to the `aws` host's hostname) here in Rust and
    // hand the script the substituted file via AS_SS_CLIENT_CONF — the JSON
    // rewrite is far safer done here than with string munging in the script.
    let ss_conf_temp = prepare_ss_client_conf(host, all_hosts, &file_tag, state);
    if let Some(ref temp) = ss_conf_temp {
        cmd.env("AS_SS_CLIENT_CONF", temp.as_os_str());
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = cmd.output().context("running deploy script (powershell)")?;
    for line in String::from_utf8_lossy(&output.stdout).lines().take(400) {
        log(state, format!("  | {line}"));
    }
    for line in String::from_utf8_lossy(&output.stderr).lines().take(200) {
        log(state, format!("  ! {line}"));
    }
    let _ = fs::remove_file(&script_path);
    if let Some(temp) = ss_conf_temp {
        let _ = fs::remove_file(&temp);
    }
    Ok(output.status.success())
}

/// If `host` collected `usr/local/shadowsocks/conf/shadowsocks-client.json` and
/// a host named `aws` exists, write a server-substituted copy to a temp file and
/// return its path (for the deploy script to scp up). Returns None (and logs)
/// when nothing needs doing; a substitution error is logged and yields None.
fn prepare_ss_client_conf(
    host: &CollectorHost,
    all_hosts: &[CollectorHost],
    file_tag: &str,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Option<PathBuf> {
    let conf_local = host
        .root
        .join("usr")
        .join("local")
        .join("shadowsocks")
        .join("conf")
        .join("shadowsocks-client.json");
    if !conf_local.exists() {
        return None;
    }
    let Some(aws) = all_hosts
        .iter()
        .find(|h| h.name.trim().eq_ignore_ascii_case("aws"))
        .map(|h| h.hostname.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        log(state, "shadowsocks conf present but no 'aws' host — leaving server address unchanged");
        return None;
    };
    let text = match fs::read_to_string(&conf_local) {
        Ok(text) => text,
        Err(err) => {
            log(state, format!("could not read {}: {err}", conf_local.display()));
            return None;
        }
    };
    match substitute_ss_server(&text, &aws) {
        Ok(new_text) => {
            let temp = std::env::temp_dir().join(format!("auto_sync_ss_client_{file_tag}.json"));
            if let Err(err) = fs::write(&temp, new_text) {
                log(state, format!("could not write substituted conf: {err}"));
                return None;
            }
            let family = if is_ipv4(&aws) { "IPv4" } else { "IPv6" };
            log(state, format!("shadowsocks server address -> {aws} ({family})"));
            Some(temp)
        }
        Err(err) => {
            log(state, format!("shadowsocks server substitution skipped: {err:#}"));
            None
        }
    }
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
    // List entries and mark directories with a trailing `/`, using a `[ -d ]`
    // test (which follows symlinks, so a symlink-to-dir is flagged correctly).
    // We iterate `ls -A1` output rather than a shell glob because an unmatched
    // glob is a fatal error under zsh (`no matches found`) whereas it stays
    // literal in sh — the `ls | while read` form behaves the same in zsh, bash,
    // ash and busybox.
    let remote_cmd = format!(
        "cd -- {} 2>/dev/null || exit 0; \
ls -A1 2>/dev/null | while IFS= read -r f; do \
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
    fn parse_perm_line_handles_stat_and_ls() {
        // stat -c '%a %n' (mode already octal)
        assert_eq!(
            parse_perm_line("755 /usr/local/shadowsocks/bin"),
            Some(("755".to_string(), "/usr/local/shadowsocks/bin".to_string()))
        );
        assert_eq!(
            parse_perm_line("644 /etc/config/dhcp"),
            Some(("644".to_string(), "/etc/config/dhcp".to_string()))
        );
        assert_eq!(parse_perm_line("4755 /bin/su").unwrap().0, "4755");
        // ls -ldn fallback (symbolic)
        let dir = "drwxr-xr-x    2 0        0             4096 Jun 30 21:40 /usr/local/shadowsocks/bin";
        assert_eq!(parse_perm_line(dir), Some(("755".to_string(), "/usr/local/shadowsocks/bin".to_string())));
        let key = "-rw-------    1 0 0 100 Jan 1 00:00 /root/.ssh/id_ed25519";
        assert_eq!(parse_perm_line(key), Some(("600".to_string(), "/root/.ssh/id_ed25519".to_string())));
        let suid = "-rwsr-xr-x 1 0 0 1 Jan 1 00:00 /bin/su";
        assert_eq!(parse_perm_line(suid).unwrap().0, "4755");
        let sticky = "drwxrwxrwt 2 0 0 1 Jan 1 00:00 /tmp";
        assert_eq!(parse_perm_line(sticky).unwrap().0, "1777");
        assert_eq!(parse_perm_line("garbage"), None);
    }

    #[test]
    fn ss_server_substitution_is_family_matched() {
        let conf = r#"{
  "servers": [
    { "server": "2406:da18:c4a:2281::1", "server_port": 443, "disabled": true },
    { "server": "54.179.191.126", "server_port": 443, "disabled": false }
  ]
}"#;
        // IPv4 aws -> only the IPv4 entry changes.
        let out = substitute_ss_server(conf, "203.0.113.9").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["servers"][0]["server"], "2406:da18:c4a:2281::1");
        assert_eq!(v["servers"][1]["server"], "203.0.113.9");

        // IPv6 aws -> only the IPv6 entry changes.
        let out6 = substitute_ss_server(conf, "2001:db8::2").unwrap();
        let v6: serde_json::Value = serde_json::from_str(&out6).unwrap();
        assert_eq!(v6["servers"][0]["server"], "2001:db8::2");
        assert_eq!(v6["servers"][1]["server"], "54.179.191.126");

        assert!(is_ipv4("54.179.191.126"));
        assert!(!is_ipv4("2001:db8::2"));
        assert!(!is_ipv4("256.1.1.1"));
        assert!(!is_ipv4("example.com"));
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
