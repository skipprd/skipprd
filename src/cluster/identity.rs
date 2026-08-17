use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use skippr_lease::{ClusterId, HostId, NodeId, PathError};
use uuid::Uuid;

use crate::helpers::wal_storage::{ConfigError, WalStorage};

#[derive(Clone, Debug)]
pub struct ProcessQueryBind {
    pub identity: ClusterIdentity,
    pub flight: SocketAddr,
}

static PROCESS_QUERY: OnceLock<Mutex<Option<ProcessQueryBind>>> = OnceLock::new();

pub fn install_process_query_bind(bind: ProcessQueryBind) {
    *PROCESS_QUERY
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("process query bind") = Some(bind);
}

pub fn process_query_bind() -> Option<ProcessQueryBind> {
    PROCESS_QUERY.get()?.lock().ok()?.clone()
}

pub fn clear_process_query_bind() {
    if let Some(lock) = PROCESS_QUERY.get() {
        *lock.lock().expect("process query bind") = None;
    }
}

#[derive(Clone, Debug)]
pub struct ClusterConfig {
    pub storage: WalStorage,
    pub table: String,
    pub cluster_id: ClusterId,
    pub gossip_hmac_key: Vec<u8>,
    pub node_id: NodeId,
    pub host_id: HostId,
    pub data_root: PathBuf,
    pub advertised_ip: IpAddr,
}

/// Cluster membership identity for replica RPC and gossip. Not a tenant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClusterIdentity {
    pub cluster_id: ClusterId,
    pub node: NodeId,
}

impl ClusterIdentity {
    pub fn new(cluster_id: ClusterId, node: NodeId) -> Self {
        Self { cluster_id, node }
    }

    pub fn hash(&self) -> String {
        self.cluster_id.hash_hex()
    }

    pub fn node_label(&self) -> String {
        self.node.to_string()
    }
}

/// Logical tenant for Flight SQL sessions. Independent of [`ClusterIdentity`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantScope {
    pub tenant: String,
    pub workspace: String,
}

impl TenantScope {
    pub fn new(tenant: impl Into<String>, workspace: impl Into<String>) -> Result<Self, PathError> {
        let tenant = tenant.into();
        let workspace = workspace.into();
        skippr_lease::validate_path_component(&tenant)?;
        skippr_lease::validate_path_component(&workspace)?;
        Ok(Self { tenant, workspace })
    }

    pub fn matches_pipeline(&self, key: &skippr_lease::PipelineKey) -> bool {
        key.tenant() == self.tenant && key.workspace() == self.workspace
    }

    pub fn basic_authorization(&self) -> String {
        use base64::Engine;
        let user = format!("{}/{}", self.tenant, self.workspace);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(user.as_bytes())
        )
    }
}

pub fn query_tenant_scope_from_env() -> Result<TenantScope, PathError> {
    let tenant = std::env::var("SKIPPR_QUERY_TENANT").unwrap_or_default();
    let workspace = std::env::var("SKIPPR_QUERY_WORKSPACE").unwrap_or_default();
    if tenant.trim().is_empty() || workspace.trim().is_empty() {
        return Err(PathError::InvalidComponent(
            "SKIPPR_QUERY_TENANT and SKIPPR_QUERY_WORKSPACE are required".into(),
        ));
    }
    TenantScope::new(tenant.trim(), workspace.trim())
}

impl ClusterConfig {
    pub fn identity(&self) -> ClusterIdentity {
        ClusterIdentity::new(self.cluster_id.clone(), self.node_id)
    }

    pub fn advertised_addr(&self, bind: SocketAddr) -> SocketAddr {
        advertised_addr(bind, self.advertised_ip)
    }
}

pub fn derive_host_id() -> Result<HostId, ConfigError> {
    if let Ok(node) = std::env::var("KUBERNETES_NODE_NAME") {
        if !node.trim().is_empty() {
            return HostId::new(node).map_err(map_path);
        }
    }
    if std::env::var("ECS_CONTAINER_METADATA_URI_V4").is_ok()
        || std::env::var("ECS_CONTAINER_METADATA_URI").is_ok()
    {
        if let Some(id) = read_ec2_instance_id() {
            return HostId::new(id).map_err(map_path);
        }
    }
    if let Some(id) = read_ec2_instance_id() {
        return HostId::new(id).map_err(map_path);
    }
    if let Ok(machine) = std::fs::read_to_string("/etc/machine-id") {
        let trimmed = machine.trim();
        if !trimmed.is_empty() {
            return HostId::new(trimmed).map_err(map_path);
        }
    }
    let hostname = hostname().map_err(|err| {
        ConfigError::InvalidIdentity(format!("failed to resolve hostname: {err}"))
    })?;
    HostId::new(hostname).map_err(map_path)
}

fn read_ec2_instance_id() -> Option<String> {
    let body = std::fs::read_to_string("/var/lib/cloud/data/instance-id").ok()?;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn hostname() -> Result<String, std::io::Error> {
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if let Ok(s) = std::env::var("HOSTNAME") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..len]) {
                if !name.is_empty() {
                    return Ok(name.to_string());
                }
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "hostname is not available",
    ))
}

fn map_path(err: PathError) -> ConfigError {
    ConfigError::InvalidIdentity(err.to_string())
}

/// Advertised private address is the local route used to reach DynamoDB.
/// Loopback is allowed only when that route is loopback (DynamoDB Local).
pub fn derive_advertised_ip(dynamodb_endpoint: &str) -> Result<IpAddr, ConfigError> {
    let host = dynamodb_endpoint
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(dynamodb_endpoint)
        .split(':')
        .next()
        .unwrap_or("dynamodb.us-east-1.amazonaws.com");
    let port = dynamodb_endpoint
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(443);
    let probe = format!("{host}:{port}");
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|err| {
        ConfigError::InvalidAdvertisedAddress(format!("bind probe failed: {err}"))
    })?;
    socket.connect(&probe).map_err(|err| {
        ConfigError::InvalidAdvertisedAddress(format!(
            "could not route to DynamoDB endpoint {probe}: {err}"
        ))
    })?;
    let ip = socket
        .local_addr()
        .map_err(|err| ConfigError::InvalidAdvertisedAddress(err.to_string()))?
        .ip();
    if ip.is_unspecified() {
        return Err(ConfigError::InvalidAdvertisedAddress(format!(
            "advertised address {ip} is unspecified"
        )));
    }
    Ok(ip)
}

pub fn dynamodb_route_endpoint() -> String {
    dynamodb_route_endpoint_from(
        std::env::var("AWS_ENDPOINT_URL_DYNAMODB").ok(),
        std::env::var("AWS_ENDPOINT_URL").ok(),
        std::env::var("AWS_REGION").ok(),
    )
}

fn dynamodb_route_endpoint_from(
    dynamodb: Option<String>,
    generic: Option<String>,
    region: Option<String>,
) -> String {
    if let Some(url) = dynamodb.filter(|url| !url.trim().is_empty()) {
        return url;
    }
    if let Some(url) = generic.filter(|url| !url.trim().is_empty()) {
        return url;
    }
    let region = region.unwrap_or_else(|| "us-east-1".into());
    format!("https://dynamodb.{region}.amazonaws.com")
}

pub fn advertised_addr(bind: SocketAddr, ip: IpAddr) -> SocketAddr {
    SocketAddr::new(ip, bind.port())
}

pub fn bind_ephemeral(ip: IpAddr) -> Result<SocketAddr, ConfigError> {
    let socket = UdpSocket::bind((ip, 0)).or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)));
    socket
        .and_then(|s| s.local_addr())
        .map_err(|err| ConfigError::InvalidAdvertisedAddress(err.to_string()))
}

pub fn process_generation() -> NodeId {
    NodeId::from_uuid(Uuid::new_v4())
}

pub fn exclusive_data_dir_lock(data_root: &Path) -> Result<std::fs::File, ConfigError> {
    std::fs::create_dir_all(data_root).map_err(|err| ConfigError::ClusteredDataDirLock {
        path: data_root.display().to_string(),
        detail: err.to_string(),
    })?;
    let path = data_root.join("clustered.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| ConfigError::ClusteredDataDirLock {
            path: path.display().to_string(),
            detail: err.to_string(),
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(ConfigError::ClusteredDataDirLock {
                path: path.display().to_string(),
                detail: "another clustered skipprd process already owns this DATA_DIR".into(),
            });
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_identity_hash_is_cluster_id_not_tenant() {
        let node = NodeId::generate();
        let lake_a = ClusterId::new("lake-a").unwrap();
        let lake_b = ClusterId::new("lake-b").unwrap();
        let id = ClusterIdentity::new(lake_a.clone(), node);
        assert_eq!(id.hash(), lake_a.hash_hex());
        assert_ne!(id.hash(), lake_b.hash_hex());
    }

    #[test]
    fn process_generation_is_unique() {
        assert_ne!(process_generation(), process_generation());
    }

    #[test]
    fn host_id_falls_back_to_hostname() {
        let id = derive_host_id().expect("host id");
        assert!(!id.as_str().is_empty());
    }

    #[test]
    fn loopback_and_unspecified_are_not_advertisable() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let unspecified: IpAddr = "0.0.0.0".parse().unwrap();
        assert!(loopback.is_loopback());
        assert!(unspecified.is_unspecified());
        let bind: SocketAddr = "0.0.0.0:1234".parse().unwrap();
        assert_eq!(
            advertised_addr(bind, loopback).to_string(),
            "127.0.0.1:1234"
        );
        assert!(!advertised_addr(bind, loopback).ip().is_unspecified());
    }

    #[test]
    fn dynamodb_local_endpoint_prefers_service_specific_url() {
        assert_eq!(
            dynamodb_route_endpoint_from(
                Some("http://127.0.0.1:8000".into()),
                Some("http://127.0.0.1:9000".into()),
                None,
            ),
            "http://127.0.0.1:8000"
        );
        assert_eq!(
            dynamodb_route_endpoint_from(None, Some("http://127.0.0.1:8000".into()), None),
            "http://127.0.0.1:8000"
        );
        assert_eq!(
            dynamodb_route_endpoint_from(None, None, Some("eu-west-1".into())),
            "https://dynamodb.eu-west-1.amazonaws.com"
        );
    }
}
