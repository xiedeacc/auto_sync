use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::{debug, warn};

use crate::core::config::{
    AppConfig, MachineConfig, load_config, normalized_machines, preferred_local_host,
};

pub const DISCOVERY_PORT: u16 = 18766;
const DISCOVERY_MAGIC: &[u8] = b"auto_sync_discover_v1";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineHealth {
    pub id: String,
    pub name: String,
    pub host: String,
    pub web_port: u16,
    #[serde(default)]
    pub ssh_user: String,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    pub os: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub web_port: u16,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub os: String,
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

pub fn local_health(cfg: &AppConfig, web_port: u16) -> MachineHealth {
    let machines = normalized_machines(cfg);
    let local = machines
        .iter()
        .into_iter()
        .find(|machine| machine.id == "local")
        .cloned()
        .unwrap_or_else(MachineConfig::local);
    let (ssh_user, ssh_port) = advertised_ssh_config(cfg, &machines, &local, web_port);
    let host = local.host;
    MachineHealth {
        id: discovery_machine_id(&host, web_port),
        name: local_machine_name(&local.name),
        host,
        web_port,
        ssh_user,
        ssh_port,
        os: std::env::consts::OS.to_string(),
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
            advertised_ssh_config(cfg, &normalized, machine, machine.web_port)
        } else {
            (
                machine.ssh_user.trim().to_string(),
                normalized_ssh_port(machine.ssh_port),
            )
        };
        machines.push(MachineView {
            id: machine.id.clone(),
            name: machine.name.clone(),
            host: machine.host.clone(),
            web_port: machine.web_port,
            ssh_user,
            ssh_port,
            os: machine.os.clone(),
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
    endpoint_key(&machine.host, machine.web_port)
}

fn machine_view_endpoint_key(machine: &MachineView) -> (String, u16) {
    endpoint_key(&machine.host, machine.web_port)
}

fn endpoint_key(host: &str, web_port: u16) -> (String, u16) {
    (host.trim().to_ascii_lowercase(), web_port)
}

fn default_ssh_port() -> u16 {
    22
}

fn normalized_ssh_port(port: u16) -> u16 {
    if port == 0 { default_ssh_port() } else { port }
}

fn local_machine_name(fallback: &str) -> String {
    static LOCAL_MACHINE_NAME: OnceLock<Option<String>> = OnceLock::new();
    if let Some(value) = LOCAL_MACHINE_NAME.get_or_init(detect_local_machine_name) {
        return value.clone();
    }

    fallback_machine_name(fallback)
}

fn detect_local_machine_name() -> Option<String> {
    for value in [
        std::env::var("COMPUTERNAME").ok(),
        std::env::var("HOSTNAME").ok(),
        std::fs::read_to_string("/etc/hostname").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    hostname_command_name()
}

fn fallback_machine_name(fallback: &str) -> String {
    let fallback = fallback.trim();
    if fallback.is_empty() {
        "This machine".to_string()
    } else {
        fallback.to_string()
    }
}

fn hostname_command_name() -> Option<String> {
    let mut command = Command::new("hostname");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command.output().ok().and_then(|output| {
        let value = String::from_utf8(output.stdout).ok()?;
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn has_advertised_ssh(machine: &MachineConfig) -> bool {
    !machine.ssh_user.trim().is_empty() || normalized_ssh_port(machine.ssh_port) != 22
}

fn advertised_ssh_config(
    cfg: &AppConfig,
    machines: &[MachineConfig],
    local: &MachineConfig,
    web_port: u16,
) -> (String, u16) {
    let local_host = local.host.trim();
    if let Some(machine) = machines.iter().find(|machine| {
        machine.id != "local"
            && machine.host.trim().eq_ignore_ascii_case(local_host)
            && machine.web_port == web_port
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
    cfg: &AppConfig,
    machines: &[MachineConfig],
    local: &MachineConfig,
) -> Vec<u16> {
    let mut ports = Vec::new();
    let local_port = normalized_ssh_port(local.ssh_port);
    if local_port != 22 {
        push_unique_port(&mut ports, local_port);
    }
    for target in &cfg.deploy.targets {
        if is_localish_host(&target.host, &local.host) {
            push_unique_port(&mut ports, target.port);
        }
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
    cfg.deploy
        .targets
        .iter()
        .find(|target| {
            is_localish_host(&target.host, &local.host)
                && (target.port == ssh_port || target.port == 0)
                && !target.user.trim().is_empty()
        })
        .or_else(|| {
            cfg.deploy.targets.iter().find(|target| {
                is_localish_host(&target.host, &local.host) && !target.user.trim().is_empty()
            })
        })
        .map(|target| target.user.trim().to_string())
        .unwrap_or_default()
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
            .find(|machine| machine.host == health.host && machine.web_port == health.web_port)
        {
            existing.online = true;
            merge_name_from_health(existing, &health);
            merge_ssh_from_health(existing, &health);
            if existing.id != "local" {
                existing.discovered = true;
            }
            continue;
        }

        let mut id = clean_machine_id(&health.id);
        if id.is_empty() || id == "local" || known_ids.contains(&id) {
            id = discovery_machine_id(&health.host, health.web_port);
        }
        id = unique_machine_id(id, &known_ids);
        known_ids.insert(id.clone());

        let name = discovered_machine_name(&health);

        status.machines.push(MachineView {
            id,
            name,
            host: health.host,
            web_port: health.web_port,
            ssh_user: health.ssh_user.trim().to_string(),
            ssh_port: normalized_ssh_port(health.ssh_port),
            os: health.os,
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
    let name = health.name.trim();
    if name.is_empty() || name == "local" || name == "This machine" {
        health.host.clone()
    } else {
        name.to_string()
    }
}

fn discovery_machine_id(host: &str, web_port: u16) -> String {
    let host = clean_machine_id(host);
    let path_hash = current_exe_path_hash();
    if host.is_empty() {
        format!("lan_{web_port}_{path_hash}")
    } else {
        format!("lan_{host}_{web_port}_{path_hash}")
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

pub fn spawn_discovery_responder(config_path: Arc<PathBuf>, web_port: u16) -> JoinHandle<()> {
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
            let health = local_health(&cfg, web_port);
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
                        .any(|item| item.host == health.host && item.web_port == health.web_port)
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
    endpoint_key(&health.host, health.web_port) == machine_endpoint_key(machine)
        || (!machine.id.trim().is_empty() && machine.id == health.id)
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
    let raw = http_request(
        &machine.host,
        machine.web_port,
        "GET",
        path,
        None,
        &[],
        timeout,
    )?;
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
        machine.web_port,
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
        machine.web_port,
        "POST",
        path,
        Some("application/octet-stream"),
        body,
        timeout,
    )?;
    serde_json::from_slice(&raw).context("failed to parse peer response")
}

pub fn find_machine(cfg: &AppConfig, machine_id: &str) -> Option<MachineConfig> {
    normalized_machines(cfg)
        .into_iter()
        .find(|machine| machine.id == machine_id)
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

pub fn ssh_target(machine: &MachineConfig) -> String {
    let user = if machine.ssh_user.trim().is_empty() {
        String::new()
    } else {
        format!("{}@", machine.ssh_user.trim())
    };
    format!("{user}{}", machine.host)
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
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("invalid peer address {host}:{port}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("failed to connect to peer {host}:{port}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
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
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        bail!("invalid peer HTTP response");
    };
    let header = String::from_utf8_lossy(&raw[..split]);
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        bail!(
            "peer returned non-200 response: {}",
            header.lines().next().unwrap_or("")
        );
    }
    Ok(raw[split + 4..].to_vec())
}

pub fn rsync_endpoint(machine: &MachineConfig, path: &Path) -> String {
    let path = rsync_path(machine, path);
    if machine.id == "local" {
        path
    } else {
        format!("{}:{path}", ssh_target(machine))
    }
}

pub fn rsync_path(machine: &MachineConfig, path: &Path) -> String {
    let value = path.to_string_lossy();
    if machine.os.eq_ignore_ascii_case("windows") {
        return windows_path_to_cygwin(&value);
    }
    value.to_string()
}

fn windows_path_to_cygwin(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        let rest = normalized[2..].trim_start_matches('/');
        if rest.is_empty() {
            format!("/cygdrive/{drive}")
        } else {
            format!("/cygdrive/{drive}/{rest}")
        }
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn converts_windows_drive_paths_for_rsync() {
        let mut machine = MachineConfig::local();
        machine.os = "windows".to_string();
        assert_eq!(
            rsync_path(&machine, Path::new("C:\\Users\\me\\data")),
            "/cygdrive/c/Users/me/data"
        );
        assert_eq!(rsync_path(&machine, Path::new("D:\\")), "/cygdrive/d");
    }

    #[test]
    fn merge_discovered_keeps_multiple_local_ids_by_host() {
        let cfg = AppConfig::default();
        let status = merge_discovered(
            &cfg,
            vec![
                MachineHealth {
                    id: "local".to_string(),
                    name: "This machine".to_string(),
                    host: "203.0.113.10".to_string(),
                    web_port: 18765,
                    ssh_user: "root".to_string(),
                    ssh_port: 10022,
                    os: "linux".to_string(),
                    version: "0.1.0".to_string(),
                },
                MachineHealth {
                    id: "local".to_string(),
                    name: "This machine".to_string(),
                    host: "203.0.113.11".to_string(),
                    web_port: 18765,
                    ssh_user: "Administrator".to_string(),
                    ssh_port: 2222,
                    os: "linux".to_string(),
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
            name: "windows".to_string(),
            host: "192.168.2.166".to_string(),
            web_port: 18765,
            ssh_user: "Administrator".to_string(),
            ssh_port: 2222,
            os: "windows".to_string(),
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

        assert_eq!(health.ssh_user, "");
        assert_eq!(health.ssh_port, 22);
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
            name: "nas".to_string(),
            host: local_host.clone(),
            web_port: 18765,
            ssh_user: "root".to_string(),
            ssh_port: 10022,
            os: "linux".to_string(),
            enabled: true,
            manual: true,
        });

        let status = machine_status(&cfg);
        let matches: Vec<_> = status
            .machines
            .iter()
            .filter(|machine| machine.host == local_host && machine.web_port == 18765)
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
        nas.name = "nas".to_string();
        nas.host = "203.0.113.10".to_string();
        nas.web_port = 18765;
        nas.os = "linux".to_string();
        nas.manual = false;
        cfg.machines.push(nas);

        let status = merge_discovered(
            &cfg,
            vec![MachineHealth {
                id: "local".to_string(),
                name: "This machine".to_string(),
                host: "203.0.113.10".to_string(),
                web_port: 18765,
                ssh_user: "root".to_string(),
                ssh_port: 10022,
                os: "linux".to_string(),
                version: "0.1.0".to_string(),
            }],
        );

        let matches: Vec<_> = status
            .machines
            .iter()
            .filter(|machine| machine.host == "203.0.113.10")
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "nas");
        assert!(matches[0].online);
        assert_eq!(matches[0].ssh_user, "root");
        assert_eq!(matches[0].ssh_port, 10022);
        assert!(matches[0].discovered);
    }

    #[test]
    fn merge_discovered_preserves_manual_machine_edits() {
        let mut cfg = AppConfig::default();
        cfg.machines.push(MachineConfig {
            id: "manual_peer".to_string(),
            name: "Manual Name".to_string(),
            host: "203.0.113.30".to_string(),
            web_port: 18765,
            ssh_user: "manual".to_string(),
            ssh_port: 2222,
            os: "linux".to_string(),
            enabled: true,
            manual: true,
        });

        let status = merge_discovered(
            &cfg,
            vec![MachineHealth {
                id: "lan_peer".to_string(),
                name: "Auto Name".to_string(),
                host: "203.0.113.30".to_string(),
                web_port: 18765,
                ssh_user: "root".to_string(),
                ssh_port: 10022,
                os: "linux".to_string(),
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
