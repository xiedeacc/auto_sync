use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::{debug, warn};

use crate::core::config::{
    AppConfig, DEFAULT_TCP_CONNECTION_POOL_SIZE, MachineConfig, default_machine_port, load_config,
    local_hostname, machine_matches_reference, normalized_machines, preferred_local_host,
    process_user,
};

pub const DISCOVERY_PORT: u16 = 18766;
const DISCOVERY_MAGIC: &[u8] = b"auto_sync_discover_v1";
static TCP_CONNECTION_POOL_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_TCP_CONNECTION_POOL_SIZE);
static TCP_CONNECTION_POOL: OnceLock<Mutex<TcpConnectionPool>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TcpConnectionKey {
    host: String,
    port: u16,
}

#[derive(Default)]
struct TcpConnectionPool {
    idle: HashMap<TcpConnectionKey, Vec<TcpStream>>,
    total_idle: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineHealth {
    pub id: String,
    #[serde(default)]
    pub alias_name: String,
    pub name: String,
    pub host: String,
    #[serde(default = "default_machine_port", alias = "web_port")]
    pub port: u16,
    #[serde(default)]
    pub ssh_user: String,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    pub os: String,
    #[serde(default)]
    pub install_dir: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    pub id: String,
    pub alias_name: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub os: String,
    pub install_dir: PathBuf,
    pub enabled: bool,
    pub manual: bool,
    pub online: bool,
    pub discovered: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineStatus {
    pub online: usize,
    pub total: usize,
    pub machines: Vec<MachineView>,
}

pub fn local_health(cfg: &AppConfig, port: u16) -> MachineHealth {
    let machines = normalized_machines(cfg);
    let local = machines
        .iter()
        .into_iter()
        .find(|machine| machine.id == "local")
        .cloned()
        .unwrap_or_else(MachineConfig::local);
    let (ssh_user, ssh_port) = advertised_ssh_config(cfg, &machines, &local, port);
    let host = local.host;
    MachineHealth {
        id: discovery_machine_id(&host, port),
        alias_name: local.alias_name.trim().to_string(),
        name: local_machine_name(&local.name),
        host,
        port,
        ssh_user,
        ssh_port,
        os: std::env::consts::OS.to_string(),
        install_dir: local.install_dir.to_string_lossy().into_owned(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

pub fn machine_status(cfg: &AppConfig) -> MachineStatus {
    let mut machines = Vec::new();
    let normalized = normalized_machines(cfg);
    for machine in normalized.iter().filter(|m| m.enabled) {
        let endpoint = machine_endpoint_key(machine);
        if let Some(existing) = machines
            .iter_mut()
            .find(|view| machine_view_endpoint_key(view) == endpoint)
        {
            merge_ssh_from_machine(existing, &machine);
            continue;
        }
        let online = machine.id == "local" || ping_machine(machine);
        let (ssh_user, ssh_port) = if machine.id == "local" {
            advertised_ssh_config(cfg, &normalized, machine, machine.port)
        } else {
            (
                machine.ssh_user.trim().to_string(),
                normalized_ssh_port(machine.ssh_port),
            )
        };
        machines.push(MachineView {
            id: machine.id.clone(),
            alias_name: machine.alias_name.clone(),
            name: machine.name.clone(),
            host: machine.host.clone(),
            port: machine.port,
            ssh_user,
            ssh_port,
            os: machine.os.clone(),
            install_dir: machine.install_dir.clone(),
            enabled: machine.enabled,
            manual: machine.manual,
            online,
            discovered: false,
        });
    }
    let online = machines.iter().filter(|machine| machine.online).count();
    MachineStatus {
        online,
        total: machines.len(),
        machines,
    }
}

fn machine_endpoint_key(machine: &MachineConfig) -> (String, u16) {
    endpoint_key(&machine.host, machine.port)
}

fn machine_view_endpoint_key(machine: &MachineView) -> (String, u16) {
    endpoint_key(&machine.host, machine.port)
}

fn endpoint_key(host: &str, port: u16) -> (String, u16) {
    (host.trim().to_ascii_lowercase(), port)
}

fn default_ssh_port() -> u16 {
    22
}

fn normalized_ssh_port(port: u16) -> u16 {
    if port == 0 { default_ssh_port() } else { port }
}

fn local_machine_name(fallback: &str) -> String {
    let name = local_hostname();
    if !name.trim().is_empty() && name != "This machine" {
        return name;
    }

    fallback_machine_name(fallback)
}

fn fallback_machine_name(fallback: &str) -> String {
    let fallback = fallback.trim();
    if fallback.is_empty() {
        "This machine".to_string()
    } else {
        fallback.to_string()
    }
}

fn has_advertised_ssh(machine: &MachineConfig) -> bool {
    !machine.ssh_user.trim().is_empty() || normalized_ssh_port(machine.ssh_port) != 22
}

fn advertised_ssh_config(
    cfg: &AppConfig,
    machines: &[MachineConfig],
    local: &MachineConfig,
    port: u16,
) -> (String, u16) {
    let local_host = local.host.trim();
    if let Some(machine) = machines.iter().find(|machine| {
        machine.id != "local"
            && machine.host.trim().eq_ignore_ascii_case(local_host)
            && machine.port == port
            && has_advertised_ssh(machine)
    }) {
        return (
            machine.ssh_user.trim().to_string(),
            normalized_ssh_port(machine.ssh_port),
        );
    }
    if let Some(ssh_port) = detect_local_ssh_port(cfg, machines, local) {
        return (advertised_ssh_user(cfg, local, ssh_port), ssh_port);
    }
    (
        local.ssh_user.trim().to_string(),
        normalized_ssh_port(local.ssh_port),
    )
}

fn detect_local_ssh_port(
    cfg: &AppConfig,
    machines: &[MachineConfig],
    local: &MachineConfig,
) -> Option<u16> {
    let hosts = local_ssh_probe_hosts(local);
    let ports = local_ssh_probe_ports(cfg, machines, local);
    detect_ssh_port(&hosts, &ports)
}

fn local_ssh_probe_hosts(local: &MachineConfig) -> Vec<String> {
    let mut hosts = Vec::new();
    push_unique_string(&mut hosts, "127.0.0.1".to_string());
    push_unique_string(&mut hosts, local.host.trim().to_string());
    hosts
}

fn local_ssh_probe_ports(
    _cfg: &AppConfig,
    machines: &[MachineConfig],
    local: &MachineConfig,
) -> Vec<u16> {
    let mut ports = Vec::new();
    let local_port = normalized_ssh_port(local.ssh_port);
    if local_port != 22 {
        push_unique_port(&mut ports, local_port);
    }
    for machine in machines {
        let port = normalized_ssh_port(machine.ssh_port);
        if port != 22 {
            push_unique_port(&mut ports, port);
        }
    }
    push_unique_port(&mut ports, 10022);
    push_unique_port(&mut ports, 22);
    ports
}

fn advertised_ssh_user(cfg: &AppConfig, local: &MachineConfig, ssh_port: u16) -> String {
    if !local.ssh_user.trim().is_empty() {
        return local.ssh_user.trim().to_string();
    }
    let machines = normalized_machines(cfg);
    machines
        .iter()
        .find(|machine| {
            machine.id != "local"
                && is_localish_host(&machine.host, &local.host)
                && (normalized_ssh_port(machine.ssh_port) == ssh_port || machine.ssh_port == 0)
                && !machine.ssh_user.trim().is_empty()
        })
        .or_else(|| {
            machines.iter().find(|machine| {
                machine.id != "local"
                    && is_localish_host(&machine.host, &local.host)
                    && !machine.ssh_user.trim().is_empty()
            })
        })
        .map(|machine| machine.ssh_user.trim().to_string())
        // Nothing in the config tells us the user; advertise the account this
        // process runs as, which is who a peer would SSH in with.
        .unwrap_or_else(|| process_user().unwrap_or_default())
}

fn is_localish_host(host: &str, local_host: &str) -> bool {
    let host = host.trim();
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host.eq_ignore_ascii_case(local_host.trim())
}

fn detect_ssh_port(hosts: &[String], ports: &[u16]) -> Option<u16> {
    for &port in ports {
        if port == 0 {
            continue;
        }
        if hosts.iter().any(|host| ssh_port_has_banner(host, port)) {
            return Some(port);
        }
    }
    None
}

fn ssh_port_has_banner(host: &str, port: u16) -> bool {
    let host = host.trim();
    if host.is_empty() {
        return false;
    }
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    for addr in addrs {
        let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(120)) else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(180)));
        let mut buf = [0_u8; 4];
        if stream.read_exact(&mut buf).is_ok() && &buf == b"SSH-" {
            return true;
        }
    }
    false
}

fn push_unique_port(values: &mut Vec<u16>, value: u16) {
    if value != 0 && !values.contains(&value) {
        values.push(value);
    }
}

fn push_unique_string(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.iter().any(|item| item.eq_ignore_ascii_case(&value)) {
        values.push(value);
    }
}

fn merge_ssh_from_machine(view: &mut MachineView, machine: &MachineConfig) {
    let ssh_user = machine.ssh_user.trim();
    if view.ssh_user.trim().is_empty() && !ssh_user.is_empty() {
        view.ssh_user = ssh_user.to_string();
    }
    let ssh_port = normalized_ssh_port(machine.ssh_port);
    if view.ssh_port == 0 || (view.ssh_port == 22 && ssh_port != 22) {
        view.ssh_port = ssh_port;
    }
}

fn merge_ssh_from_health(view: &mut MachineView, health: &MachineHealth) {
    if view.manual {
        return;
    }
    let ssh_user = health.ssh_user.trim();
    if view.ssh_user.trim().is_empty() && !ssh_user.is_empty() {
        view.ssh_user = ssh_user.to_string();
    }
    let ssh_port = normalized_ssh_port(health.ssh_port);
    if view.ssh_port == 0 || (view.ssh_port == 22 && ssh_port != 22) {
        view.ssh_port = ssh_port;
    }
}

pub fn merge_discovered(cfg: &AppConfig, discovered: Vec<MachineHealth>) -> MachineStatus {
    let mut status = machine_status(cfg);
    let mut known_ids: HashSet<String> = status
        .machines
        .iter()
        .map(|machine| machine.id.clone())
        .collect();
    for health in discovered {
        if let Some(existing) = status
            .machines
            .iter_mut()
            .find(|machine| machine_view_matches_health(machine, &health))
        {
            existing.online = true;
            merge_name_from_health(existing, &health);
            merge_ssh_from_health(existing, &health);
            if existing.id != "local" {
                existing.discovered = true;
            }
            continue;
        }

        let mut id = clean_machine_id(&health.alias_name);
        if id.is_empty() {
            id = clean_machine_id(&health.id);
        }
        if id.is_empty() || id == "local" || known_ids.contains(&id) {
            id = discovery_machine_id(&health.host, health.port);
        }
        id = unique_machine_id(id, &known_ids);
        known_ids.insert(id.clone());

        let name = discovered_machine_name(&health);

        status.machines.push(MachineView {
            id,
            alias_name: clean_machine_id(&health.alias_name),
            name,
            host: health.host,
            port: health.port,
            ssh_user: health.ssh_user.trim().to_string(),
            ssh_port: normalized_ssh_port(health.ssh_port),
            os: health.os,
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: false,
            online: true,
            discovered: true,
        });
    }
    status.online = status
        .machines
        .iter()
        .filter(|machine| machine.online)
        .count();
    status.total = status.machines.len();
    status
}

fn merge_name_from_health(view: &mut MachineView, health: &MachineHealth) {
    if !view.manual {
        view.name = discovered_machine_name(health);
    }
}

fn discovered_machine_name(health: &MachineHealth) -> String {
    let alias = health.alias_name.trim();
    if !alias.is_empty() {
        return alias.to_string();
    }
    let name = health.name.trim();
    if name.is_empty() || name == "local" || name == "This machine" {
        health.host.clone()
    } else {
        name.to_string()
    }
}

/// Whether `value` is a discovery id (`lan_<sanitized-host>_<port>_<hash>`,
/// see [`discovery_machine_id`]) referring to this machine entry. Used to
/// resolve `source_group.managed_by` — which stores the controller's
/// discovery id — back to a configured machine.
pub fn machine_matches_discovery_id(machine: &MachineConfig, value: &str) -> bool {
    let host = clean_machine_id(&machine.host);
    if host.is_empty() {
        return false;
    }
    value.starts_with(&format!("lan_{host}_{}_", machine.port))
}

fn discovery_machine_id(host: &str, port: u16) -> String {
    let host = clean_machine_id(host);
    let path_hash = current_exe_path_hash();
    if host.is_empty() {
        format!("lan_{port}_{path_hash}")
    } else {
        format!("lan_{host}_{port}_{path_hash}")
    }
}

fn current_exe_path_hash() -> String {
    let path = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_PKG_NAME")));
    let digest = blake3::hash(path.to_string_lossy().as_bytes());
    digest.to_hex().chars().take(8).collect()
}

fn clean_machine_id(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn unique_machine_id(base: String, known_ids: &HashSet<String>) -> String {
    let base = if base.is_empty() {
        "discovered".to_string()
    } else {
        base
    };
    if !known_ids.contains(&base) {
        return base;
    }
    for index in 2.. {
        let candidate = format!("{base}_{index}");
        if !known_ids.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

pub fn spawn_discovery_responder(config_path: Arc<PathBuf>, port: u16) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let socket = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
            Ok(socket) => socket,
            Err(err) => {
                warn!(error = %err, "failed to bind LAN discovery responder");
                return;
            }
        };
        let mut buf = [0_u8; 512];
        loop {
            let Ok((len, peer)) = socket.recv_from(&mut buf) else {
                continue;
            };
            if &buf[..len] != DISCOVERY_MAGIC {
                continue;
            }
            let Ok(cfg) = load_config(&config_path) else {
                continue;
            };
            let health = local_health(&cfg, port);
            let Ok(raw) = serde_json::to_vec(&health) else {
                continue;
            };
            if let Err(err) = socket.send_to(&raw, peer) {
                debug!(error = %err, "failed to send discovery response");
            }
        }
    })
}

pub fn discover_lan(timeout: Duration) -> Result<Vec<MachineHealth>> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).context("failed to open discovery socket")?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    for target in discovery_targets() {
        if let Err(err) = socket.send_to(DISCOVERY_MAGIC, target) {
            debug!(error = %err, %target, "failed to send LAN discovery packet");
        }
    }
    let start = Instant::now();
    let mut out: Vec<MachineHealth> = Vec::new();
    let mut buf = [0_u8; 2048];
    while start.elapsed() < timeout {
        match socket.recv_from(&mut buf) {
            Ok((len, _)) => {
                if let Ok(health) = serde_json::from_slice::<MachineHealth>(&buf[..len]) {
                    if !out
                        .iter()
                        .any(|item| item.host == health.host && item.port == health.port)
                    {
                        out.push(health);
                    }
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(out)
}

pub fn machines_share_lan(left: &MachineConfig, right: &MachineConfig, timeout: Duration) -> bool {
    if machine_endpoint_key(left) == machine_endpoint_key(right) {
        return true;
    }
    if same_private_ipv4_lan(&left.host, &right.host) {
        return true;
    }
    let Ok(discovered) = discover_lan(timeout) else {
        return false;
    };
    let left_seen =
        left.id == "local" || discovered.iter().any(|health| health_matches(left, health));
    let right_seen = right.id == "local"
        || discovered
            .iter()
            .any(|health| health_matches(right, health));
    left_seen && right_seen
}

fn health_matches(machine: &MachineConfig, health: &MachineHealth) -> bool {
    let alias = health.alias_name.trim();
    (!alias.is_empty() && machine_matches_reference(machine, alias))
        || (health.id.trim() != "local"
            && !health.id.trim().is_empty()
            && machine_matches_reference(machine, &health.id))
        || endpoint_key(&health.host, health.port) == machine_endpoint_key(machine)
}

fn same_private_ipv4_lan(left: &str, right: &str) -> bool {
    let Ok(left) = left.trim().parse::<Ipv4Addr>() else {
        return false;
    };
    let Ok(right) = right.trim().parse::<Ipv4Addr>() else {
        return false;
    };
    if !is_private_ipv4(left) || !is_private_ipv4(right) {
        return false;
    }
    let left = left.octets();
    let right = right.octets();
    left[..3] == right[..3]
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
}

fn discovery_targets() -> Vec<SocketAddr> {
    let mut targets = vec![SocketAddr::from((
        Ipv4Addr::new(255, 255, 255, 255),
        DISCOVERY_PORT,
    ))];
    if let Ok(ip) = preferred_local_host().parse::<Ipv4Addr>() {
        let [a, b, c, _] = ip.octets();
        if !ip.is_loopback() && !ip.is_unspecified() {
            targets.push(SocketAddr::from((
                Ipv4Addr::new(a, b, c, 255),
                DISCOVERY_PORT,
            )));
        }
    }
    targets.sort_unstable();
    targets.dedup();
    targets
}

pub fn remote_get_json<T: DeserializeOwned>(
    machine: &MachineConfig,
    path: &str,
    timeout: Duration,
) -> Result<T> {
    let raw = http_request(&machine.host, machine.port, "GET", path, None, &[], timeout)?;
    serde_json::from_slice(&raw).context("failed to parse peer response")
}

pub fn remote_post_json<B: Serialize, T: DeserializeOwned>(
    machine: &MachineConfig,
    path: &str,
    body: &B,
    timeout: Duration,
) -> Result<T> {
    let body = serde_json::to_vec(body).context("failed to serialize peer request")?;
    let raw = http_request(
        &machine.host,
        machine.port,
        "POST",
        path,
        Some("application/json"),
        &body,
        timeout,
    )?;
    serde_json::from_slice(&raw).context("failed to parse peer response")
}

pub fn remote_post_bytes<T: DeserializeOwned>(
    machine: &MachineConfig,
    path: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<T> {
    let raw = http_request(
        &machine.host,
        machine.port,
        "POST",
        path,
        Some("application/octet-stream"),
        body,
        timeout,
    )?;
    serde_json::from_slice(&raw).context("failed to parse peer response")
}

/// POST a request whose body is STREAMED through `write_body` with a known
/// Content-Length instead of being buffered in memory first (multi-gigabyte
/// file pushes). Always opens a FRESH connection: a streamed body cannot be
/// replayed on a stale pooled connection the way buffered requests are. The
/// connection is returned to the pool afterwards when reusable. Returns the
/// raw response body; non-200 responses use the same error wording as the
/// buffered path so callers can classify missing endpoints.
pub fn remote_post_octet_stream(
    machine: &MachineConfig,
    path: &str,
    content_length: u64,
    write_body: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let key = TcpConnectionKey {
        host: machine.host.trim().to_ascii_lowercase(),
        port: machine.port,
    };
    let mut stream = open_tcp_connection(&key, timeout)?;
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: keep-alive\r\n\
         Accept: application/json\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {content_length}\r\n",
        host = machine.host,
        port = machine.port,
    );
    let token = peer_token();
    if !token.is_empty() {
        request.push_str(&format!("{PEER_TOKEN_HEADER}: {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    write_body(&mut stream)?;
    stream.flush()?;
    let response = read_http_response(&mut stream)?;
    if !response.status_line.starts_with("HTTP/1.1 200")
        && !response.status_line.starts_with("HTTP/1.0 200")
    {
        let detail: String = String::from_utf8_lossy(&response.body)
            .trim()
            .chars()
            .take(2048)
            .collect();
        if detail.is_empty() {
            bail!(
                "peer returned non-200 response: {}",
                response.status_line.trim()
            );
        }
        bail!(
            "peer returned non-200 response: {}: {detail}",
            response.status_line.trim()
        );
    }
    if response.reusable {
        return_tcp_connection(key, stream);
    }
    Ok(response.body)
}

/// POST a JSON request and stream the NDJSON response line by line through
/// `on_line` (CR/LF stripped) instead of buffering the whole body. Whole-tree
/// snapshot responses run to ~100MB of JSON — the buffered path holds all of
/// it (plus the parsed value) in memory on the requesting side.
pub fn remote_post_ndjson<B: Serialize>(
    machine: &MachineConfig,
    path: &str,
    body: &B,
    timeout: Duration,
    on_line: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let body = serde_json::to_vec(body).context("failed to serialize peer request")?;
    let key = TcpConnectionKey {
        host: machine.host.trim().to_ascii_lowercase(),
        port: machine.port,
    };
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 0..2 {
        let (mut stream, reused) = if attempt == 0 {
            take_tcp_connection(&key, timeout)?
        } else {
            (open_tcp_connection(&key, timeout)?, false)
        };
        let mut delivered = false;
        match ndjson_request_on_stream(
            &mut stream,
            &machine.host,
            machine.port,
            path,
            &body,
            &mut delivered,
            on_line,
        ) {
            Ok(reusable) => {
                if reusable {
                    return_tcp_connection(key, stream);
                }
                return Ok(());
            }
            // A stale pooled connection fails before any body bytes arrive;
            // once lines were delivered a retry would duplicate them.
            Err(err) if reused && attempt == 0 && !delivered => {
                last_error = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("HTTP request failed")))
}

/// One streaming NDJSON request on an open connection. Returns whether the
/// connection is reusable (response fully consumed, no `Connection: close`).
fn ndjson_request_on_stream(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    path: &str,
    body: &[u8],
    delivered: &mut bool,
    on_line: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<bool> {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: keep-alive\r\nAccept: application/x-ndjson\r\nContent-Type: application/json\r\n"
    );
    let token = peer_token();
    if !token.is_empty() {
        request.push_str(&format!("{PEER_TOKEN_HEADER}: {token}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;

    // Read headers; anything past the blank line is the first body bytes.
    let mut raw = Vec::new();
    let mut buf = [0_u8; 16 * 1024];
    let split = loop {
        if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break split;
        }
        let n = stream.read(&mut buf)?;
        if n == 0 {
            bail!("peer closed connection before HTTP headers");
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > 128 * 1024 {
            bail!("peer HTTP headers are too large");
        }
    };
    let header_text = String::from_utf8_lossy(&raw[..split]).to_string();
    let pending = raw[split + 4..].to_vec();
    let mut lines = header_text.lines();
    let status_line = lines.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let connection_close = headers
        .iter()
        .any(|(name, value)| name == "connection" && value.to_ascii_lowercase().contains("close"));
    let chunked = headers.iter().any(|(name, value)| {
        name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    let content_length: Option<usize> = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok());

    if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
        // Collect a bounded error body for the same peer-error classification
        // the buffered path provides.
        let mut error_body = Vec::new();
        let collect = &mut |chunk: &[u8]| -> Result<()> {
            if error_body.len() < 64 * 1024 {
                error_body.extend_from_slice(chunk);
            }
            Ok(())
        };
        if chunked {
            stream_chunked_body(stream, pending, collect).ok();
        } else if let Some(total) = content_length {
            stream_sized_body(stream, pending, total, collect).ok();
        }
        let detail: String = String::from_utf8_lossy(&error_body)
            .trim()
            .chars()
            .take(2048)
            .collect();
        if detail.is_empty() {
            bail!("peer returned non-200 response: {}", status_line.trim());
        }
        bail!(
            "peer returned non-200 response: {}: {detail}",
            status_line.trim()
        );
    }

    let mut splitter = NdjsonLineSplitter {
        carry: Vec::new(),
        on_line,
    };
    let sink = &mut |chunk: &[u8]| -> Result<()> {
        if !chunk.is_empty() {
            *delivered = true;
        }
        splitter.feed(chunk)
    };
    if chunked {
        stream_chunked_body(stream, pending, sink)?;
    } else if let Some(total) = content_length {
        stream_sized_body(stream, pending, total, sink)?;
    } else {
        // No framing: read to EOF; the connection cannot be reused.
        sink(&pending)?;
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sink(&buf[..n])?;
        }
        splitter.finish()?;
        return Ok(false);
    }
    splitter.finish()?;
    Ok(!connection_close)
}

struct NdjsonLineSplitter<'a> {
    carry: Vec<u8>,
    on_line: &'a mut dyn FnMut(&[u8]) -> Result<()>,
}

impl NdjsonLineSplitter<'_> {
    fn feed(&mut self, chunk: &[u8]) -> Result<()> {
        let joined;
        let data: &[u8] = if self.carry.is_empty() {
            chunk
        } else {
            joined = [self.carry.as_slice(), chunk].concat();
            &joined
        };
        let mut start = 0;
        while let Some(pos) = data[start..].iter().position(|&b| b == b'\n') {
            let line = &data[start..start + pos];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            (self.on_line)(line)?;
            start += pos + 1;
        }
        self.carry = data[start..].to_vec();
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.carry.is_empty() {
            return Ok(());
        }
        let line = std::mem::take(&mut self.carry);
        let trimmed = line.strip_suffix(b"\r").unwrap_or(&line);
        (self.on_line)(trimmed)
    }
}

/// Deliver a `Content-Length` body to `sink`, starting from the bytes already
/// read past the headers.
fn stream_sized_body<R: Read>(
    stream: &mut R,
    pending: Vec<u8>,
    total: usize,
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<usize> {
    let mut consumed = 0;
    if !pending.is_empty() {
        let take = pending.len().min(total);
        sink(&pending[..take])?;
        consumed += take;
    }
    let mut buf = [0_u8; 16 * 1024];
    while consumed < total {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            bail!("peer closed connection mid-body");
        }
        let take = n.min(total - consumed);
        sink(&buf[..take])?;
        consumed += take;
    }
    Ok(consumed)
}

/// Decode a `Transfer-Encoding: chunked` body incrementally, delivering
/// payload bytes to `sink`. Consumes the terminating chunk and trailer so the
/// connection stays reusable.
fn stream_chunked_body<R: Read>(
    stream: &mut R,
    mut pending: Vec<u8>,
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut buf = [0_u8; 16 * 1024];
    let mut fill = |pending: &mut Vec<u8>, stream: &mut R| -> Result<()> {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            bail!("peer closed connection mid-chunked-body");
        }
        pending.extend_from_slice(&buf[..n]);
        Ok(())
    };
    loop {
        // Chunk-size line: hex size, optional extensions after ';'.
        let line_end = loop {
            if let Some(pos) = find_crlf(&pending) {
                break pos;
            }
            if pending.len() > 16 * 1024 {
                bail!("chunk-size line too large");
            }
            fill(&mut pending, stream)?;
        };
        let size_text = std::str::from_utf8(&pending[..line_end])
            .context("invalid chunk-size line encoding")?;
        let size_field = size_text.split(';').next().unwrap_or("").trim();
        let size =
            usize::from_str_radix(size_field, 16).context("invalid chunk size in response")?;
        pending.drain(..line_end + 2);
        if size == 0 {
            // Trailer section: zero or more header lines, then an empty line.
            loop {
                let pos = loop {
                    if let Some(pos) = find_crlf(&pending) {
                        break pos;
                    }
                    fill(&mut pending, stream)?;
                };
                let empty = pos == 0;
                pending.drain(..pos + 2);
                if empty {
                    return Ok(());
                }
            }
        }
        let mut remaining = size;
        while remaining > 0 {
            if pending.is_empty() {
                fill(&mut pending, stream)?;
            }
            let take = remaining.min(pending.len());
            sink(&pending[..take])?;
            pending.drain(..take);
            remaining -= take;
        }
        while pending.len() < 2 {
            fill(&mut pending, stream)?;
        }
        if &pending[..2] != b"\r\n" {
            bail!("malformed chunk terminator");
        }
        pending.drain(..2);
    }
}

fn find_crlf(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|window| window == b"\r\n")
}

pub fn configure_tcp_connection_pool(max_idle: usize) {
    TCP_CONNECTION_POOL_LIMIT.store(max_idle, Ordering::Relaxed);
    if let Some(pool) = TCP_CONNECTION_POOL.get() {
        pool.lock()
            .unwrap_or_else(|err| err.into_inner())
            .trim_to(max_idle);
    }
}

/// Header carrying the shared peer secret on machine-to-machine calls.
pub const PEER_TOKEN_HEADER: &str = "x-auto-sync-token";

fn peer_token_slot() -> &'static std::sync::Mutex<String> {
    static TOKEN: std::sync::OnceLock<std::sync::Mutex<String>> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| std::sync::Mutex::new(String::new()))
}

/// Shared secret for the peer/transfer APIs (`app.peer_token`). When set —
/// the SAME value in every machine's config — outgoing peer calls carry it
/// and this machine rejects transfer/delegation requests without it. Empty
/// keeps the open LAN-trust behavior.
pub fn configure_peer_token(token: &str) {
    let mut slot = peer_token_slot()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    if *slot != token {
        *slot = token.to_string();
    }
}

pub fn peer_token() -> String {
    peer_token_slot()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

pub fn find_machine(cfg: &AppConfig, machine_id: &str) -> Option<MachineConfig> {
    let machines = normalized_machines(cfg);
    resolve_machine(&machines, machine_id)
}

fn resolve_machine(machines: &[MachineConfig], machine_id: &str) -> Option<MachineConfig> {
    let machine_id = machine_id_from_path(Some(machine_id));
    if machine_id == "local" {
        return machines
            .iter()
            .find(|machine| machine.id == "local")
            .cloned();
    }
    machines
        .iter()
        .find(|machine| {
            let alias = machine.alias_name.trim();
            !alias.is_empty() && alias.eq_ignore_ascii_case(machine_id)
        })
        .or_else(|| {
            machines
                .iter()
                .find(|machine| machine.id.eq_ignore_ascii_case(machine_id))
        })
        .or_else(|| {
            machines
                .iter()
                .find(|machine| machine.host.trim().eq_ignore_ascii_case(machine_id))
        })
        .cloned()
}

/// Match a stored `MachineConfig` against a discovered health record (by alias,
/// non-local id, or host+port), mirroring `machine_view_matches_health`.
pub fn machine_matches_health(machine: &MachineConfig, health: &MachineHealth) -> bool {
    let alias = health.alias_name.trim();
    (!alias.is_empty()
        && !machine.alias_name.trim().is_empty()
        && machine.alias_name.eq_ignore_ascii_case(alias))
        || (health.id.trim() != "local"
            && !health.id.trim().is_empty()
            && machine.id.eq_ignore_ascii_case(health.id.trim()))
        || (machine.host == health.host && machine.port == health.port)
}

fn machine_view_matches_health(machine: &MachineView, health: &MachineHealth) -> bool {
    let alias = health.alias_name.trim();
    (!alias.is_empty()
        && !machine.alias_name.trim().is_empty()
        && machine.alias_name.eq_ignore_ascii_case(alias))
        || (health.id.trim() != "local"
            && !health.id.trim().is_empty()
            && machine.id.eq_ignore_ascii_case(&health.id))
        || (machine.host == health.host && machine.port == health.port)
}

pub fn machine_id_from_path(machine_id: Option<&str>) -> &str {
    machine_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("local")
}

pub fn encode_query_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn ping_machine(machine: &MachineConfig) -> bool {
    remote_get_json::<MachineHealth>(machine, "/api/health", Duration::from_millis(700)).is_ok()
}

fn http_request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>> {
    let key = TcpConnectionKey {
        host: host.trim().to_ascii_lowercase(),
        port,
    };
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 0..2 {
        let (mut stream, reused) = if attempt == 0 {
            take_tcp_connection(&key, timeout)?
        } else {
            (open_tcp_connection(&key, timeout)?, false)
        };
        match http_request_on_stream(&mut stream, host, port, method, path, content_type, body) {
            Ok(response) => {
                if response.reusable {
                    return_tcp_connection(key, stream);
                }
                return Ok(response.body);
            }
            Err(err) if reused && attempt == 0 => {
                last_error = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("HTTP request failed")))
}

fn http_request_on_stream(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<HttpResponse> {
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: keep-alive\r\nAccept: application/json\r\n"
    );
    let token = peer_token();
    if !token.is_empty() {
        request.push_str(&format!("{PEER_TOKEN_HEADER}: {token}\r\n"));
    }
    if let Some(content_type) = content_type {
        request.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if !body.is_empty() || method.eq_ignore_ascii_case("POST") {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    let response = read_http_response(stream)?;
    if !response.status_line.starts_with("HTTP/1.1 200")
        && !response.status_line.starts_with("HTTP/1.0 200")
    {
        // Include the (truncated) body: peer errors carry their reason there,
        // and callers classify some of them (e.g. "source changed while
        // copying <path>") to decide yellow-vs-red handling.
        let detail: String = String::from_utf8_lossy(&response.body)
            .trim()
            .chars()
            .take(2048)
            .collect();
        if detail.is_empty() {
            bail!(
                "peer returned non-200 response: {}",
                response.status_line.trim()
            );
        }
        bail!(
            "peer returned non-200 response: {}: {detail}",
            response.status_line.trim()
        );
    }
    Ok(response)
}

struct HttpResponse {
    status_line: String,
    body: Vec<u8>,
    reusable: bool,
}

fn read_http_response<R: Read>(stream: &mut R) -> Result<HttpResponse> {
    let mut raw = Vec::new();
    let mut buf = [0_u8; 8192];
    let split = loop {
        if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break split;
        }
        let n = stream.read(&mut buf)?;
        if n == 0 {
            bail!("peer closed connection before HTTP headers");
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > 128 * 1024 {
            bail!("peer HTTP headers are too large");
        }
    };
    let header_text = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = header_text.lines();
    let status_line = lines.next().unwrap_or("").to_string();
    if status_line.is_empty() {
        bail!("invalid peer HTTP response");
    }
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let connection_close = headers
        .iter()
        .any(|(name, value)| name == "connection" && value.to_ascii_lowercase().contains("close"));
    let chunked = headers.iter().any(|(name, value)| {
        name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let body_start = split + 4;
    let mut body = raw[body_start..].to_vec();
    let reusable = if let Some(content_length) = content_length {
        while body.len() < content_length {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                bail!("peer closed connection before response body was complete");
            }
            body.extend_from_slice(&buf[..n]);
        }
        let exact = body.len() == content_length;
        body.truncate(content_length);
        exact && !connection_close
    } else if chunked {
        body = read_chunked_body(stream, body)?;
        !connection_close
    } else {
        stream.read_to_end(&mut body)?;
        false
    };
    Ok(HttpResponse {
        status_line,
        body,
        reusable,
    })
}

fn read_chunked_body<R: Read>(stream: &mut R, mut pending: Vec<u8>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_crlf_line(stream, &mut pending)?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .with_context(|| format!("invalid chunk size: {size_text}"))?;
        if size == 0 {
            loop {
                let trailer = read_crlf_line(stream, &mut pending)?;
                if trailer.is_empty() {
                    return Ok(body);
                }
            }
        }
        ensure_pending(stream, &mut pending, size + 2)?;
        body.extend_from_slice(&pending[..size]);
        if &pending[size..size + 2] != b"\r\n" {
            bail!("invalid chunk terminator");
        }
        pending.drain(..size + 2);
    }
}

fn read_crlf_line<R: Read>(stream: &mut R, pending: &mut Vec<u8>) -> Result<String> {
    loop {
        if let Some(pos) = pending.windows(2).position(|window| window == b"\r\n") {
            let line = String::from_utf8_lossy(&pending[..pos]).to_string();
            pending.drain(..pos + 2);
            return Ok(line);
        }
        read_more(stream, pending)?;
    }
}

fn ensure_pending<R: Read>(stream: &mut R, pending: &mut Vec<u8>, needed: usize) -> Result<()> {
    while pending.len() < needed {
        read_more(stream, pending)?;
    }
    Ok(())
}

fn read_more<R: Read>(stream: &mut R, pending: &mut Vec<u8>) -> Result<()> {
    let mut buf = [0_u8; 8192];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        bail!("peer closed connection while reading response");
    }
    pending.extend_from_slice(&buf[..n]);
    Ok(())
}

fn take_tcp_connection(key: &TcpConnectionKey, timeout: Duration) -> Result<(TcpStream, bool)> {
    if TCP_CONNECTION_POOL_LIMIT.load(Ordering::Relaxed) > 0 {
        if let Some(stream) = tcp_connection_pool()
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take(key)
        {
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            return Ok((stream, true));
        }
    }
    Ok((open_tcp_connection(key, timeout)?, false))
}

fn open_tcp_connection(key: &TcpConnectionKey, timeout: Duration) -> Result<TcpStream> {
    // to_socket_addrs resolves hostnames too (SocketAddr::parse accepted only
    // IP literals, so a machine configured by name failed every peer call);
    // try each resolved address until one connects.
    let addrs: Vec<SocketAddr> = (key.host.as_str(), key.port)
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve peer address {}:{}", key.host, key.port))?
        .collect();
    let mut last_err: Option<std::io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                return Ok(stream);
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("peer address resolved to nothing")))
    .with_context(|| format!("failed to connect to peer {}:{}", key.host, key.port))
}

fn return_tcp_connection(key: TcpConnectionKey, stream: TcpStream) {
    let limit = TCP_CONNECTION_POOL_LIMIT.load(Ordering::Relaxed);
    if limit == 0 {
        return;
    }
    tcp_connection_pool()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .put(key, stream, limit);
}

fn tcp_connection_pool() -> &'static Mutex<TcpConnectionPool> {
    TCP_CONNECTION_POOL.get_or_init(|| Mutex::new(TcpConnectionPool::default()))
}

impl TcpConnectionPool {
    fn take(&mut self, key: &TcpConnectionKey) -> Option<TcpStream> {
        let stream = self.idle.get_mut(key)?.pop();
        if stream.is_some() {
            self.total_idle = self.total_idle.saturating_sub(1);
        }
        if self.idle.get(key).is_some_and(Vec::is_empty) {
            self.idle.remove(key);
        }
        stream
    }

    fn put(&mut self, key: TcpConnectionKey, stream: TcpStream, max_idle: usize) {
        if self.total_idle >= max_idle {
            return;
        }
        self.idle.entry(key).or_default().push(stream);
        self.total_idle += 1;
    }

    fn trim_to(&mut self, max_idle: usize) {
        while self.total_idle > max_idle {
            let Some(key) = self.idle.keys().next().cloned() else {
                self.total_idle = 0;
                break;
            };
            if self.take(&key).is_none() {
                self.idle.remove(&key);
            }
        }
    }
}

#[cfg(test)]
fn pooled_tcp_connection_count() -> usize {
    TCP_CONNECTION_POOL
        .get()
        .map(|pool| {
            pool.lock()
                .unwrap_or_else(|err| err.into_inner())
                .total_idle
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn http_response_reads_content_length_body_and_stays_reusable() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let response = read_http_response(&mut std::io::Cursor::new(wire.to_vec())).unwrap();
        assert_eq!(response.status_line, "HTTP/1.1 200 OK");
        assert_eq!(response.body, b"hello");
        assert!(response.reusable);
    }

    #[test]
    fn http_response_connection_close_is_not_reusable() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let response = read_http_response(&mut std::io::Cursor::new(wire.to_vec())).unwrap();
        assert_eq!(response.body, b"ok");
        assert!(!response.reusable);
    }

    #[test]
    fn http_response_decodes_chunked_transfer_encoding() {
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                     6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n";
        let response = read_http_response(&mut std::io::Cursor::new(wire.to_vec())).unwrap();
        assert_eq!(response.body, b"hello world");
        assert!(response.reusable);
        // Chunk extensions after ';' are ignored.
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                     2;ext=1\r\nhi\r\n0\r\n\r\n";
        let response = read_http_response(&mut std::io::Cursor::new(wire.to_vec())).unwrap();
        assert_eq!(response.body, b"hi");
    }

    #[test]
    fn http_response_without_framing_reads_to_eof_and_is_not_reusable() {
        let wire = b"HTTP/1.0 200 OK\r\n\r\nstream-until-close";
        let response = read_http_response(&mut std::io::Cursor::new(wire.to_vec())).unwrap();
        assert_eq!(response.body, b"stream-until-close");
        assert!(!response.reusable);
    }

    #[test]
    fn http_response_rejects_truncated_body_and_headers() {
        // Body shorter than Content-Length → error, not a silent short read.
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhel";
        assert!(read_http_response(&mut std::io::Cursor::new(wire.to_vec())).is_err());
        // Connection dropped before the header terminator.
        let wire = b"HTTP/1.1 200 OK\r\nContent-Le";
        assert!(read_http_response(&mut std::io::Cursor::new(wire.to_vec())).is_err());
    }

    #[test]
    fn chunked_body_decodes_across_read_boundaries() {
        // "hello " (6) + "world" (5) split so chunk headers and payloads
        // straddle the initial pending buffer and later reads.
        let wire = b"6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n";
        for split in 0..wire.len() {
            let pending = wire[..split].to_vec();
            let mut rest = std::io::Cursor::new(wire[split..].to_vec());
            let mut out = Vec::new();
            stream_chunked_body(&mut rest, pending, &mut |chunk| {
                out.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
            assert_eq!(out, b"hello world", "split at {split}");
        }
    }

    #[test]
    fn chunked_body_rejects_truncation() {
        let wire = b"6\r\nhel";
        let mut rest = std::io::Cursor::new(Vec::new());
        let result = stream_chunked_body(&mut rest, wire.to_vec(), &mut |_| Ok(()));
        assert!(result.is_err());
    }

    #[test]
    fn sized_body_delivers_exactly_content_length() {
        let mut rest = std::io::Cursor::new(b"body-and-then-garbage".to_vec());
        let mut out = Vec::new();
        let consumed = stream_sized_body(&mut rest, Vec::new(), 4, &mut |chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap();
        assert_eq!(consumed, 4);
        assert_eq!(out, b"body");
    }

    #[test]
    fn ndjson_lines_split_across_chunks() {
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut on_line = |line: &[u8]| {
            lines.push(line.to_vec());
            Ok(())
        };
        let mut splitter = NdjsonLineSplitter {
            carry: Vec::new(),
            on_line: &mut on_line,
        };
        splitter.feed(b"{\"a\":1}\r\n{\"b\"").unwrap();
        splitter.feed(b":2}\n").unwrap();
        splitter.feed(b"{\"c\":3}").unwrap();
        splitter.finish().unwrap();
        assert_eq!(
            lines,
            vec![
                b"{\"a\":1}".to_vec(),
                b"{\"b\":2}".to_vec(),
                b"{\"c\":3}".to_vec()
            ]
        );
    }

    #[test]
    fn merge_discovered_keeps_multiple_local_ids_by_host() {
        let cfg = AppConfig::default();
        let status = merge_discovered(
            &cfg,
            vec![
                MachineHealth {
                    id: "local".to_string(),
                    alias_name: String::new(),
                    name: "This machine".to_string(),
                    host: "203.0.113.10".to_string(),
                    port: 18765,
                    ssh_user: "root".to_string(),
                    ssh_port: 10022,
                    os: "linux".to_string(),
                    install_dir: String::new(),
                    version: "0.1.0".to_string(),
                },
                MachineHealth {
                    id: "local".to_string(),
                    alias_name: String::new(),
                    name: "This machine".to_string(),
                    host: "203.0.113.11".to_string(),
                    port: 18765,
                    ssh_user: "Administrator".to_string(),
                    ssh_port: 2222,
                    os: "linux".to_string(),
                    install_dir: String::new(),
                    version: "0.1.0".to_string(),
                },
            ],
        );

        assert!(status.machines.iter().any(|machine| {
            machine.id.starts_with("lan_203_0_113_10_18765_")
                && machine.id.len() == "lan_203_0_113_10_18765_".len() + 8
                && machine.name == "203.0.113.10"
                && machine.discovered
                && machine.ssh_user == "root"
                && machine.ssh_port == 10022
        }));
        assert!(status.machines.iter().any(|machine| {
            machine.id.starts_with("lan_203_0_113_11_18765_")
                && machine.id.len() == "lan_203_0_113_11_18765_".len() + 8
                && machine.name == "203.0.113.11"
                && machine.discovered
                && machine.ssh_user == "Administrator"
                && machine.ssh_port == 2222
        }));
    }

    #[test]
    fn local_health_uses_network_discovery_id() {
        let mut cfg = AppConfig::default();
        cfg.machines[0].host = "192.168.2.166".to_string();
        cfg.machines.push(MachineConfig {
            id: "windows".to_string(),
            alias_name: String::new(),
            name: "windows".to_string(),
            host: "192.168.2.166".to_string(),
            port: 18765,
            ssh_user: "Administrator".to_string(),
            ssh_port: 2222,
            os: "windows".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });
        let health = local_health(&cfg, 18765);

        assert!(health.id.starts_with("lan_192_168_2_166_18765_"));
        assert_eq!(health.id.len(), "lan_192_168_2_166_18765_".len() + 8);
        assert_eq!(health.host, "192.168.2.166");
        assert_eq!(health.ssh_user, "Administrator");
        assert_eq!(health.ssh_port, 2222);
    }

    #[test]
    fn detects_local_ssh_port_from_banner() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"SSH-2.0-auto_sync_test\r\n").unwrap();
        });

        let detected = detect_ssh_port(&["127.0.0.1".to_string()], &[port, 22]);

        handle.join().unwrap();
        assert_eq!(detected, Some(port));
    }

    #[test]
    fn machine_health_defaults_ssh_for_old_peers() {
        let health: MachineHealth = serde_json::from_str(
            r#"{"id":"peer","name":"peer","host":"203.0.113.20","web_port":18765,"os":"linux","version":"0.1.0"}"#,
        )
        .unwrap();

        assert_eq!(health.port, 18765);
        assert_eq!(health.ssh_user, "");
        assert_eq!(health.ssh_port, 22);
    }

    #[test]
    fn remote_get_json_reuses_pooled_tcp_connection() {
        configure_tcp_connection_pool(0);
        configure_tcp_connection_pool(1);
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for _ in 0..2 {
                read_test_http_request(&mut stream);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: keep-alive\r\n\r\n{\"ok\":true}",
                    )
                    .unwrap();
            }
        });
        let mut machine = MachineConfig::local();
        machine.host = "127.0.0.1".to_string();
        machine.port = port;

        let first: serde_json::Value =
            remote_get_json(&machine, "/first", Duration::from_secs(2)).unwrap();
        let second: serde_json::Value =
            remote_get_json(&machine, "/second", Duration::from_secs(2)).unwrap();

        handle.join().unwrap();
        assert_eq!(first["ok"], true);
        assert_eq!(second["ok"], true);
        assert_eq!(pooled_tcp_connection_count(), 1);
        configure_tcp_connection_pool(0);
        configure_tcp_connection_pool(DEFAULT_TCP_CONNECTION_POOL_SIZE);
    }

    fn read_test_http_request(stream: &mut TcpStream) {
        let mut raw = Vec::new();
        let mut buf = [0_u8; 128];
        loop {
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0);
            raw.extend_from_slice(&buf[..n]);
            if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                return;
            }
        }
    }

    #[test]
    fn same_private_ipv4_lan_uses_private_24s_only() {
        assert!(same_private_ipv4_lan("192.168.2.10", "192.168.2.247"));
        assert!(same_private_ipv4_lan("10.0.4.10", "10.0.4.11"));
        assert!(!same_private_ipv4_lan("192.168.2.10", "192.168.3.10"));
        assert!(!same_private_ipv4_lan("203.0.113.10", "203.0.113.11"));
        assert!(!same_private_ipv4_lan("nas.local", "192.168.2.247"));
    }

    #[test]
    fn machine_status_prefers_local_for_duplicate_endpoint() {
        let mut cfg = AppConfig::default();
        let local_host = MachineConfig::local().host;
        cfg.machines.push(MachineConfig {
            id: "nas".to_string(),
            alias_name: String::new(),
            name: "nas".to_string(),
            host: local_host.clone(),
            port: 18765,
            ssh_user: "root".to_string(),
            ssh_port: 10022,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });

        let status = machine_status(&cfg);
        let matches: Vec<_> = status
            .machines
            .iter()
            .filter(|machine| machine.host == local_host && machine.port == 18765)
            .collect();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "local");
        assert!(matches[0].online);
        assert_eq!(matches[0].ssh_user, "root");
        assert_eq!(matches[0].ssh_port, 10022);
    }

    #[test]
    fn merge_discovered_marks_existing_endpoint_online() {
        let mut cfg = AppConfig::default();
        let mut nas = MachineConfig::local();
        nas.id = "nas".to_string();
        nas.alias_name = "nas".to_string();
        nas.name = "nas".to_string();
        nas.host = "203.0.113.10".to_string();
        nas.port = 18765;
        nas.os = "linux".to_string();
        nas.manual = false;
        cfg.machines.push(nas);

        let status = merge_discovered(
            &cfg,
            vec![MachineHealth {
                id: "local".to_string(),
                alias_name: "nas".to_string(),
                name: "This machine".to_string(),
                host: "203.0.113.99".to_string(),
                port: 18765,
                ssh_user: "root".to_string(),
                ssh_port: 10022,
                os: "linux".to_string(),
                install_dir: String::new(),
                version: "0.1.0".to_string(),
            }],
        );

        let matches: Vec<_> = status
            .machines
            .iter()
            .filter(|machine| machine.alias_name == "nas")
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "nas");
        assert_eq!(matches[0].host, "203.0.113.10");
        assert!(matches[0].online);
        assert_eq!(matches[0].ssh_user, "root");
        assert_eq!(matches[0].ssh_port, 10022);
        assert!(matches[0].discovered);
    }

    #[test]
    fn find_machine_prefers_alias_then_id_then_host() {
        let mut cfg = AppConfig::default();
        cfg.machines.push(MachineConfig {
            id: "lan_203_0_113_10_18765_abcd1234".to_string(),
            alias_name: "nas".to_string(),
            name: "Reported Host".to_string(),
            host: "203.0.113.10".to_string(),
            port: 18765,
            ssh_user: "root".to_string(),
            ssh_port: 10022,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });

        assert_eq!(find_machine(&cfg, "nas").unwrap().host, "203.0.113.10");
        assert_eq!(
            find_machine(&cfg, "lan_203_0_113_10_18765_abcd1234")
                .unwrap()
                .alias_name,
            "nas"
        );
        assert_eq!(
            find_machine(&cfg, "203.0.113.10").unwrap().alias_name,
            "nas"
        );
    }

    #[test]
    fn merge_discovered_preserves_manual_machine_edits() {
        let mut cfg = AppConfig::default();
        cfg.machines.push(MachineConfig {
            id: "manual_peer".to_string(),
            alias_name: String::new(),
            name: "Manual Name".to_string(),
            host: "203.0.113.30".to_string(),
            port: 18765,
            ssh_user: "manual".to_string(),
            ssh_port: 2222,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });

        let status = merge_discovered(
            &cfg,
            vec![MachineHealth {
                id: "lan_peer".to_string(),
                alias_name: String::new(),
                name: "Auto Name".to_string(),
                host: "203.0.113.30".to_string(),
                port: 18765,
                ssh_user: "root".to_string(),
                ssh_port: 10022,
                os: "linux".to_string(),
                install_dir: String::new(),
                version: "0.1.0".to_string(),
            }],
        );

        let peer = status
            .machines
            .iter()
            .find(|machine| machine.id == "manual_peer")
            .unwrap();
        assert!(peer.online);
        assert_eq!(peer.name, "Manual Name");
        assert_eq!(peer.ssh_user, "manual");
        assert_eq!(peer.ssh_port, 2222);
    }
}
