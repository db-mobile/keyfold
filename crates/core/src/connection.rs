//! Connection profiles and per-node connections.
//!
//! One [`ConnectionManager`] per profile opens connections to the profile's
//! address or, for a cluster, to any node of it ([`ConnectionManager::connect_to`]).
//! Topology lives in [`crate::cluster`] and routing in [`crate::route`]; the
//! crate's cluster abstraction is deliberately not used (CONCEPT 10.1).

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, Cmd, ConnectionAddr, ConnectionInfo, ErrorKind, Pipeline, ProtocolVersion, RedisConnectionInfo, Value};

use crate::cluster::NodeAddr;
use crate::error::{Error, Result};
use crate::scan::ScanProfile;

const LIB_NAME: &str = "keyfold";
const LIB_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment tag of a connection. Prod changes defaults (polite scan,
/// ephemeral index, confirmed writes); it never changes what is possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tag {
    #[default]
    Dev,
    Staging,
    Prod,
}

impl Tag {
    pub fn as_str(self) -> &'static str {
        match self {
            Tag::Dev => "dev",
            Tag::Staging => "staging",
            Tag::Prod => "prod",
        }
    }

    pub fn parse(s: &str) -> Option<Tag> {
        match s.to_ascii_lowercase().as_str() {
            "dev" | "development" => Some(Tag::Dev),
            "staging" | "stage" => Some(Tag::Staging),
            "prod" | "production" => Some(Tag::Prod),
            _ => None,
        }
    }
}

/// Which nodes a scan reads from (CONCEPT 4, "prefer replicas").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanTarget {
    /// A replica when one is reachable, else the primary.
    #[default]
    Auto,
    Primary,
    /// Replicas only; the scan refuses to start without one.
    Replica,
}

impl ScanTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            ScanTarget::Auto => "auto",
            ScanTarget::Primary => "primary",
            ScanTarget::Replica => "replica",
        }
    }

    pub fn parse(s: &str) -> Option<ScanTarget> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(ScanTarget::Auto),
            "primary" | "master" => Some(ScanTarget::Primary),
            "replica" | "replicas" => Some(ScanTarget::Replica),
            _ => None,
        }
    }
}

/// How much of the keyspace a connection indexes (CONCEPT 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexMode {
    /// A background scan of everything, kept on disk.
    #[default]
    Full,
    /// A sample seeds the tree; a subtree is scanned when it is opened.
    OnDemand,
    /// Nothing is indexed: plain `SCAN` pages and jump-to-key.
    None,
}

impl IndexMode {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexMode::Full => "full",
            IndexMode::OnDemand => "onDemand",
            IndexMode::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<IndexMode> {
        match s.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
            "full" => Some(IndexMode::Full),
            "ondemand" => Some(IndexMode::OnDemand),
            "none" | "noindex" => Some(IndexMode::None),
            _ => None,
        }
    }
}

/// A connection profile as the user authors it. Credentials are carried in
/// memory only; persistence of the password belongs to the OS keychain.
#[derive(Debug, Clone)]
pub struct Profile {
    /// Stable identifier, used as the index folder name. Never the display name.
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub db: i64,
    pub tls: bool,
    /// Skip certificate verification. Only for explicit user choice.
    pub tls_insecure: bool,
    pub tag: Tag,
    /// `None` means "follow the tag": on for prod, off otherwise.
    pub ephemeral_index: Option<bool>,
    pub substring_index: bool,
    /// Gentleness of the scan; `None` follows the tag (polite on prod,
    /// fast otherwise).
    pub scan_profile: Option<ScanProfile>,
    pub scan_target: ScanTarget,
    /// Keep the index live from keyspace notifications (CONCEPT 7, opt-in).
    pub live: bool,
    /// `None` decides by size: full up to the auto-index threshold, on
    /// demand above it.
    pub index_mode: Option<IndexMode>,
    /// Reach the server through this SSH bastion (CONCEPT 8).
    pub ssh: Option<crate::tunnel::SshConfig>,
}

/// Id of a profile that exists only as a URL (CLI, quick connect).
fn url_id(host: &str, port: u16, db: i64) -> String {
    format!("{host}_{port}_{db}")
}

impl Profile {
    /// Build a profile from a `redis://`, `rediss://`, `valkey://` or
    /// `valkeys://` URL. The id is derived from host, port and db so that
    /// the CLI gets a stable index file per target.
    pub fn from_url(url: &str) -> Result<Profile> {
        let parsed = redis::parse_redis_url(url).ok_or_else(|| Error::Config(format!("not a server URL: {url}")))?;
        let info: ConnectionInfo = redis::IntoConnectionInfo::into_connection_info(parsed)?;
        let (host, port, tls, tls_insecure) = match info.addr() {
            ConnectionAddr::Tcp(h, p) => (h.clone(), *p, false, false),
            ConnectionAddr::TcpTls { host, port, insecure, .. } => (host.clone(), *port, true, *insecure),
            ConnectionAddr::Unix(p) => return Err(Error::Config(format!("unix sockets are not supported yet: {}", p.display()))),
            other => return Err(Error::Config(format!("unsupported address kind: {other:?}"))),
        };
        let r: &RedisConnectionInfo = info.redis_settings();
        let db = r.db();
        Ok(Profile {
            id: url_id(&host, port, db),
            name: format!("{host}:{port}/{db}"),
            host,
            port,
            username: r.username().map(str::to_string),
            password: r.password().map(str::to_string),
            db,
            tls,
            tls_insecure,
            tag: Tag::Dev,
            ephemeral_index: None,
            substring_index: false,
            scan_profile: None,
            scan_target: ScanTarget::Auto,
            live: false,
            index_mode: None,
            ssh: None,
        })
    }

    /// The same server and credentials on another logical database. A
    /// profile built from a URL carries the database in its id and name,
    /// so both follow; a stored profile keeps its id (the index folder,
    /// which holds one file per database) and its display name.
    pub fn with_db(&self, db: i64) -> Profile {
        let mut other = Profile { db, ..self.clone() };
        if self.id == url_id(&self.host, self.port, self.db) {
            other.id = url_id(&self.host, self.port, db);
            other.name = format!("{}:{}/{db}", self.host, self.port);
        }
        other
    }

    /// The scan profile in force: the explicit one, else by tag.
    pub fn scan_profile(&self) -> ScanProfile {
        self.scan_profile.unwrap_or(if self.tag == Tag::Prod {
            ScanProfile::Polite
        } else {
            ScanProfile::Fast
        })
    }

    /// The address the profile names.
    pub fn addr(&self) -> NodeAddr {
        NodeAddr {
            host: self.host.clone(),
            port: self.port,
        }
    }

    /// Whether the index for this profile lives in memory and dies with the
    /// session. Prod connections default to ephemeral.
    pub fn ephemeral_index(&self) -> bool {
        self.ephemeral_index.unwrap_or(self.tag == Tag::Prod)
    }

    fn connection_info(&self, protocol: ProtocolVersion, node: &NodeAddr, db: i64) -> ConnectionInfo {
        let addr = if self.tls {
            ConnectionAddr::TcpTls {
                host: node.host.clone(),
                port: node.port,
                insecure: self.tls_insecure,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(node.host.clone(), node.port)
        };
        let mut redis = RedisConnectionInfo::default()
            .set_db(db)
            .set_protocol(protocol)
            .set_lib_name(LIB_NAME, LIB_VERSION);
        if let Some(u) = &self.username {
            redis = redis.set_username(u);
        }
        if let Some(p) = &self.password {
            redis = redis.set_password(p);
        }
        redis::IntoConnectionInfo::into_connection_info(addr)
            .expect("address is always valid")
            .set_redis_settings(redis)
    }
}

/// Opens connections for one profile. One instance per profile; every
/// consumer that must not queue behind another (the scan engine, the value
/// service) asks for its own [`Conn`].
///
/// A profile with an SSH bastion gets one tunnel, shared by every clone of
/// the manager, and every connection goes through it (see [`crate::tunnel`]).
#[derive(Debug, Clone)]
pub struct ConnectionManager {
    profile: Profile,
    connect_timeout: Duration,
    response_timeout: Duration,
    tunnel: Option<std::sync::Arc<crate::tunnel::Tunnel>>,
    /// Shared by every connection this manager (or a clone of it) opens.
    denials: crate::capability::Denials,
}

impl ConnectionManager {
    pub fn new(profile: Profile) -> Self {
        let tunnel = profile.ssh.clone().map(crate::tunnel::Tunnel::new);
        ConnectionManager {
            profile,
            connect_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(30),
            tunnel,
            denials: Default::default(),
        }
    }

    /// Use a tunnel that is already open (one shared by the sessions of a
    /// profile) instead of the manager's own.
    pub fn with_tunnel(mut self, tunnel: std::sync::Arc<crate::tunnel::Tunnel>) -> Self {
        self.tunnel = Some(tunnel);
        self
    }

    /// The SSH tunnel every connection of this manager goes through.
    pub fn tunnel(&self) -> Option<&std::sync::Arc<crate::tunnel::Tunnel>> {
        self.tunnel.as_ref()
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Commands the ACL refused on any connection this manager opened.
    pub fn denials(&self) -> &crate::capability::Denials {
        &self.denials
    }

    pub fn with_timeouts(mut self, connect: Duration, response: Duration) -> Self {
        self.connect_timeout = connect;
        self.response_timeout = response;
        self
    }

    /// Open a new connection to the profile's address. RESP3 is attempted
    /// first; a server that does not understand `HELLO` gets RESP2.
    pub async fn connect(&self) -> Result<Conn> {
        self.connect_to(&self.profile.addr()).await
    }

    /// Open a connection to another node of the same deployment (a replica,
    /// a cluster node) with the profile's credentials, TLS setting and db.
    pub async fn connect_to(&self, node: &NodeAddr) -> Result<Conn> {
        match self.connect_with(ProtocolVersion::RESP3, node).await {
            Ok(c) => Ok(c),
            Err(Error::Client(e)) if hello_unsupported(&e) => {
                tracing::debug!("HELLO rejected ({e}); falling back to RESP2");
                self.connect_with(ProtocolVersion::RESP2, node).await
            }
            Err(e) => Err(e),
        }
    }

    async fn connect_with(&self, protocol: ProtocolVersion, node: &NodeAddr) -> Result<Conn> {
        self.connect_inner(protocol, node, None).await
    }

    /// Subscribe to a channel pattern on `node` over a RESP3 connection
    /// whose push messages arrive on the returned receiver. This is how a
    /// subscription goes through an SSH tunnel: the crate's dedicated
    /// pub/sub client does not take the tunnel's resolver, a multiplexed
    /// connection does. The subscription lasts as long as the `Conn`.
    pub async fn subscribe_pattern(
        &self,
        node: &NodeAddr,
        pattern: &str,
    ) -> Result<(Conn, tokio::sync::mpsc::UnboundedReceiver<redis::PushInfo>)> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut conn = self.connect_inner(ProtocolVersion::RESP3, node, Some(tx)).await?;
        conn.inner.psubscribe(pattern).await?;
        Ok((conn, rx))
    }

    async fn connect_inner(
        &self,
        protocol: ProtocolVersion,
        node: &NodeAddr,
        push: Option<tokio::sync::mpsc::UnboundedSender<redis::PushInfo>>,
    ) -> Result<Conn> {
        let info = self.profile.connection_info(protocol, node, self.profile.db);
        let client = redis::Client::open(info)?;
        let mut config = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(self.connect_timeout))
            .set_response_timeout(Some(self.response_timeout));
        if let Some(tunnel) = &self.tunnel {
            // log in first, so a refused host key or a wrong password is
            // reported as what it is and not as a failed connect
            tunnel.ensure().await?;
            config = config
                .set_connection_timeout(Some(self.connect_timeout.max(Duration::from_secs(15))))
                .set_dns_resolver(crate::tunnel::TunnelResolver(std::sync::Arc::clone(tunnel)));
        }
        if let Some(push) = push {
            config = config.set_push_sender(push);
        }
        let inner = client.get_multiplexed_async_connection_with_config(&config).await?;
        Ok(Conn {
            inner,
            protocol,
            label: format!("{}:{}/{}", node.host, node.port, self.profile.db),
            addr: node.clone(),
            denials: self.denials.clone(),
        })
    }

    /// A client for a subscription connection to `node` (keyspace
    /// notifications). Subscriptions use the crate's push stream, the one
    /// place a typed client API is used instead of a raw command; the
    /// commands it sends are listed in `docs/commands.md`.
    pub fn pubsub_client(&self, node: &NodeAddr) -> Result<redis::Client> {
        Ok(redis::Client::open(self.profile.connection_info(
            ProtocolVersion::RESP2,
            node,
            self.profile.db,
        ))?)
    }
}

fn hello_unsupported(e: &redis::RedisError) -> bool {
    if matches!(
        e.kind(),
        ErrorKind::Io | ErrorKind::AuthenticationFailed | ErrorKind::InvalidClientConfig
    ) {
        return false;
    }
    let text = e.to_string().to_ascii_lowercase();
    text.contains("hello") || text.contains("unknown command") || text.contains("resp3")
}

/// One server connection. Every command goes through [`Conn::raw`] or
/// [`Conn::pipeline`], which turn error replies into [`Error`] values.
pub struct Conn {
    inner: MultiplexedConnection,
    protocol: ProtocolVersion,
    label: String,
    addr: NodeAddr,
    denials: crate::capability::Denials,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn")
            .field("label", &self.label)
            .field("protocol", &self.protocol)
            .finish()
    }
}

impl Conn {
    pub fn protocol(&self) -> u8 {
        match self.protocol {
            ProtocolVersion::RESP3 => 3,
            _ => 2,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// The node this connection talks to.
    pub fn addr(&self) -> &NodeAddr {
        &self.addr
    }

    /// Commands the ACL refused on any connection of this session.
    pub fn denials(&self) -> &crate::capability::Denials {
        &self.denials
    }

    /// Every error passes here: a command the ACL refuses is remembered.
    fn noted(&self, e: Error) -> Error {
        if let Some(name) = e.denied_command() {
            self.denials.mark(&name);
        }
        e
    }

    /// Send one command and return its reply.
    pub async fn raw(&mut self, cmd: &Cmd) -> Result<Value> {
        let name = command_name(cmd);
        match self.inner.send_packed_command(cmd).await {
            Ok(Value::ServerError(e)) => Err(self.noted(Error::from_server(&name, e.code(), e.details()))),
            Ok(v) => Ok(v),
            Err(e) => Err(self.noted(map_client_error(&name, e))),
        }
    }

    /// Send a pipeline and return one reply per command, in order. Error
    /// replies for individual commands are returned in place as `Err`, so
    /// one denied command does not hide the others.
    pub async fn pipeline(&mut self, pipe: &Pipeline) -> Result<Vec<Result<Value>>> {
        let n = pipe.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let values = match self.inner.send_packed_commands(pipe, 0, n).await {
            Ok(v) => v,
            Err(e) => return Err(self.noted(map_client_error("pipeline", e))),
        };
        Ok(values
            .into_iter()
            .map(|v| match v {
                Value::ServerError(e) => Err(self.noted(Error::from_server("pipeline", e.code(), e.details()))),
                v => Ok(v),
            })
            .collect())
    }
}

fn map_client_error(command: &str, e: redis::RedisError) -> Error {
    match e.kind() {
        ErrorKind::Server(_) | ErrorKind::Extension => {
            let code = e.code().unwrap_or("ERR").to_string();
            Error::from_server(command, &code, e.detail())
        }
        _ => Error::Client(e),
    }
}

/// Uppercase name of the command, for error messages and ACL diagnostics.
pub fn command_name(cmd: &Cmd) -> String {
    match cmd.args_iter().next() {
        Some(redis::Arg::Simple(b)) => String::from_utf8_lossy(b).to_ascii_uppercase(),
        _ => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_from_url() {
        let p = Profile::from_url("redis://user:secret@db.example:6380/3").unwrap();
        assert_eq!(p.host, "db.example");
        assert_eq!(p.port, 6380);
        assert_eq!(p.db, 3);
        assert_eq!(p.username.as_deref(), Some("user"));
        assert_eq!(p.password.as_deref(), Some("secret"));
        assert!(!p.tls);
        assert_eq!(p.id, "db.example_6380_3");
        let other = p.with_db(7);
        assert_eq!(
            (other.db, other.id.as_str(), other.name.as_str()),
            (7, "db.example_6380_7", "db.example:6380/7")
        );
        assert_eq!(other.password, p.password);
        // a stored profile has its own id and name; both survive a switch
        let stored = Profile {
            id: "6f1c0c0e-0000-4000-8000-000000000001".into(),
            name: "orders".into(),
            ..p.clone()
        };
        let switched = stored.with_db(7);
        assert_eq!((switched.db, switched.id, switched.name), (7, stored.id, stored.name));

        let v = Profile::from_url("valkeys://localhost").unwrap();
        assert!(v.tls);
        assert_eq!(v.port, 6379);
        assert_eq!(v.db, 0);

        assert!(Profile::from_url("http://nope").is_err());
    }

    #[test]
    fn ephemeral_follows_tag() {
        let mut p = Profile::from_url("redis://localhost").unwrap();
        assert!(!p.ephemeral_index());
        p.tag = Tag::Prod;
        assert!(p.ephemeral_index());
        p.ephemeral_index = Some(false);
        assert!(!p.ephemeral_index());
    }

    #[test]
    fn command_name_reads_first_arg() {
        assert_eq!(command_name(&crate::commands::scan(0, None, 10, None)), "SCAN");
        assert_eq!(command_name(&crate::commands::memory_usage(b"k")), "MEMORY");
    }
}
