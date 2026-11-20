use std::fmt;

/// Every failure the backend can report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport, parsing or protocol level failure from the client crate.
    #[error("redis client: {0}")]
    Client(#[from] redis::RedisError),

    /// The server answered a command with an error reply.
    #[error("server replied {code}: {detail}")]
    Server { code: String, detail: String },

    /// The ACL on the server denies this command (NOPERM).
    #[error("command denied by server ACL: {command}")]
    Denied { command: String },

    /// The index database failed.
    #[error("index: {0}")]
    Index(#[from] rusqlite::Error),

    /// The server returned a reply with an unexpected shape.
    #[error("unexpected reply to {command}: {detail}")]
    Reply { command: String, detail: String },

    /// A profile or configuration value is unusable.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The scan engine was asked to do something it cannot do in its state.
    #[error("scan: {0}")]
    Scan(String),

    /// The SSH tunnel of a profile could not be opened or used.
    #[error("{0}")]
    Tunnel(String),
    /// The bastion presented a host key that is not on record (or differs
    /// from the one that is).
    #[error("{}", host_key_text(.0))]
    HostKey(Box<crate::tunnel::HostKey>),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn reply(command: &str, detail: impl fmt::Display) -> Self {
        Error::Reply {
            command: command.to_string(),
            detail: detail.to_string(),
        }
    }

    /// Build the error for a server error reply, classifying ACL denials.
    pub fn from_server(command: &str, code: &str, detail: Option<&str>) -> Self {
        if code == "NOPERM" {
            return Error::Denied {
                command: command.to_string(),
            };
        }
        Error::Server {
            code: code.to_string(),
            detail: detail.unwrap_or("").to_string(),
        }
    }

    pub fn is_denied(&self) -> bool {
        matches!(self, Error::Denied { .. })
    }

    /// A cluster redirect (`MOVED` or `ASK`): the slot and the node that
    /// serves it now.
    pub fn redirect(&self) -> Option<Redirect> {
        let Error::Server { code, detail } = self else { return None };
        let ask = match code.as_str() {
            "MOVED" => false,
            "ASK" => true,
            _ => return None,
        };
        let mut parts = detail.split_whitespace();
        let slot: u16 = parts.next()?.parse().ok()?;
        let (host, port) = parts.next()?.rsplit_once(':')?;
        let port: u16 = port.parse().ok()?;
        Some(Redirect {
            ask,
            slot,
            host: host.to_string(),
            port,
        })
    }
}

/// Where a cluster node sent a command instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// `ASK`: a one-off redirect during slot migration; `MOVED` otherwise.
    pub ask: bool,
    pub slot: u16,
    pub host: String,
    pub port: u16,
}

fn host_key_text(k: &crate::tunnel::HostKey) -> String {
    if k.changed {
        format!(
            "the SSH host key of {}:{} has changed (now {}); this can mean someone is in between, so the connection was refused",
            k.host, k.port, k.fingerprint
        )
    } else {
        format!("the SSH host key of {}:{} is not known yet: {}", k.host, k.port, k.fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_are_parsed() {
        let e = Error::from_server("GET", "MOVED", Some("3999 127.0.0.1:6381"));
        let r = e.redirect().unwrap();
        assert!(!r.ask);
        assert_eq!(r.slot, 3999);
        assert_eq!((r.host.as_str(), r.port), ("127.0.0.1", 6381));
        let e = Error::from_server("GET", "ASK", Some("12 [::1]:7000"));
        let r = e.redirect().unwrap();
        assert!(r.ask);
        assert_eq!((r.host.as_str(), r.port), ("[::1]", 7000));
        assert!(Error::from_server("GET", "ERR", Some("nope")).redirect().is_none());
        assert!(Error::from_server("GET", "MOVED", Some("garbage")).redirect().is_none());
    }
}
