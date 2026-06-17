use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    let (ssh_user, ssh_port) = advertised_ssh_config(&machines, &local, web_port);
    let host = local.host;
    MachineHealth {
        id: discovery_machine_id(&host, web_port),
        name: local.name,
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
    for machine in normalized_machines(cfg).into_iter().filter(|m| m.enabled) {
        let endpoint = machine_endpoint_key(&machine);
        if let Some(existing) = machines
            .iter_mut()
            .find(|view| machine_view_endpoint_key(view) == endpoint)
        {
            merge_ssh_from_machine(existing, &machine);
            continue;
        }
        let online = machine.id == "local" || ping_machine(&machine);
        machines.push(MachineView {
            id: machine.id,
            name: machine.name,
            host: machine.host,
            web_port: machine.web_port,
            ssh_user: machine.ssh_user,
            ssh_port: machine.ssh_port,
            os: machine.os,
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

fn has_advertised_ssh(machine: &MachineConfig) -> bool {
    !machine.ssh_user.trim().is_empty() || normalized_ssh_port(machine.ssh_port) != 22
}

fn advertised_ssh_config(
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
    (
        local.ssh_user.trim().to_string(),
        normalized_ssh_port(local.ssh_port),
    )
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
            merge_ssh_from_health(existing, &health);
            continue;
        }

        let mut id = clean_machine_id(&health.id);
        if id.is_empty() || id == "local" || known_ids.contains(&id) {
            id = discovery_machine_id(&health.host, health.web_port);
        }
        id = unique_machine_id(id, &known_ids);
        known_ids.insert(id.clone());

        let name = if health.name.trim().is_empty()
            || health.name == "local"
            || health.name == "This machine"
        {
            health.host.clone()
        } else {
            health.name
        };

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
    let raw = http_get(&machine.host, machine.web_port, path, timeout)?;
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

fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<Vec<u8>> {
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("invalid peer address {host}:{port}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("failed to connect to peer {host}:{port}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
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
    fn machine_health_defaults_ssh_for_old_peers() {
        let health: MachineHealth = serde_json::from_str(
            r#"{"id":"peer","name":"peer","host":"203.0.113.20","web_port":18765,"os":"linux","version":"0.1.0"}"#,
        )
        .unwrap();

        assert_eq!(health.ssh_user, "");
        assert_eq!(health.ssh_port, 22);
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
    }
}
