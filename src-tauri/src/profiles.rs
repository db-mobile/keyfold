//! Stored connections (CONCEPT 3.4). Profiles are a small JSON file in the
//! app data dir; each has a stable UUID that names its index folder in the
//! cache dir and its entry in the OS keychain. The file never holds a
//! password, only whether one was put into the keychain.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use keyfold_core::{IndexMode, Profile, ScanProfile, ScanTarget, Tag};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::session::{ConnectionDto, Session};
use crate::{Sessions, adopt, secrets};

const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredProfile {
    /// UUID, assigned on first save; index folder and keychain entry.
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    /// The database a connect opens; the header switches from there.
    #[serde(default)]
    pub db: i64,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub tls_insecure: bool,
    /// `dev`, `staging` or `prod`
    pub tag: String,
    /// `fast`, `normal`, `polite`; `None` follows the tag.
    #[serde(default)]
    pub scan_profile: Option<String>,
    /// `auto`, `primary` or `replica`
    #[serde(default = "default_target")]
    pub scan_target: String,
    /// `None` follows the tag (on for prod).
    #[serde(default)]
    pub ephemeral_index: Option<bool>,
    /// `full`, `onDemand` or `none`; `None` decides by the database's size.
    #[serde(default)]
    pub index_mode: Option<String>,
    /// A password for this profile is in the OS keychain.
    #[serde(default)]
    pub password_saved: bool,
    /// Reach the server through this SSH bastion (CONCEPT 8).
    #[serde(default)]
    pub ssh: Option<StoredSsh>,
    /// The bastion's password or key passphrase is in the OS keychain.
    #[serde(default)]
    pub ssh_secret_saved: bool,
}

/// The SSH bastion of a stored connection. Its password or key passphrase
/// is never part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSsh {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    /// `agent`, `key` or `password`
    pub auth: String,
    /// The private key file, for `key`.
    #[serde(default)]
    pub key_path: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}

impl StoredSsh {
    fn validated(mut self) -> Result<StoredSsh, String> {
        self.host = self.host.trim().to_string();
        self.user = self.user.trim().to_string();
        self.key_path = self.key_path.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
        if self.host.is_empty() || self.host.contains(char::is_whitespace) || self.host.contains('/') {
            return Err(format!("not an SSH host name: {:?}", self.host));
        }
        if self.user.is_empty() {
            return Err("the SSH tunnel needs a user name".into());
        }
        if self.port == 0 {
            return Err("the SSH port must be between 1 and 65535".into());
        }
        match self.auth.as_str() {
            "agent" | "password" => self.key_path = None,
            "key" => {
                if self.key_path.is_none() {
                    return Err("choose the private key file for the SSH tunnel".into());
                }
            }
            other => return Err(format!("unknown SSH login method {other:?}")),
        }
        Ok(self)
    }

    /// `~/` in a key path is the user's home directory.
    fn key_file(&self) -> Option<PathBuf> {
        let path = self.key_path.as_deref()?;
        Some(match (path.strip_prefix("~/"), std::env::home_dir()) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(path),
        })
    }
}

fn default_target() -> String {
    "auto".into()
}

impl StoredProfile {
    /// Trim the text fields and refuse what a connect could not use.
    pub fn validated(mut self) -> Result<StoredProfile, String> {
        self.name = self.name.trim().to_string();
        self.host = self.host.trim().to_string();
        self.username = self.username.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
        if self.name.is_empty() {
            return Err("the connection needs a name".into());
        }
        if self.host.is_empty() || self.host.contains(char::is_whitespace) || self.host.contains('/') {
            return Err(format!("not a host name: {:?}", self.host));
        }
        if self.port == 0 {
            return Err("the port must be between 1 and 65535".into());
        }
        if self.db < 0 {
            return Err(format!("no database {}", self.db));
        }
        Tag::parse(&self.tag).ok_or_else(|| format!("unknown tag {:?}", self.tag))?;
        if let Some(p) = &self.scan_profile {
            ScanProfile::parse(p).ok_or_else(|| format!("unknown scan profile {p:?}"))?;
        }
        ScanTarget::parse(&self.scan_target).ok_or_else(|| format!("unknown scan target {:?}", self.scan_target))?;
        if let Some(m) = &self.index_mode {
            self.index_mode = Some(
                IndexMode::parse(m)
                    .ok_or_else(|| format!("unknown index mode {m:?}"))?
                    .as_str()
                    .to_string(),
            );
        }
        if !self.tls {
            self.tls_insecure = false;
        }
        self.ssh = self.ssh.map(StoredSsh::validated).transpose()?;
        if self.ssh.as_ref().is_none_or(|s| s.auth == "agent") {
            self.ssh_secret_saved = false;
        }
        Ok(self)
    }

    /// The profile a session runs on. The password and the bastion's
    /// secret come from the keychain; `known_hosts` is the app's own file.
    pub fn to_profile_with(
        &self,
        password: Option<String>,
        ssh_secret: Option<String>,
        known_hosts: Option<PathBuf>,
    ) -> Result<Profile, String> {
        use keyfold_core::tunnel::{SshAuth, SshConfig};
        let ssh = match &self.ssh {
            None => None,
            Some(s) => Some(SshConfig {
                host: s.host.clone(),
                port: s.port,
                user: s.user.clone(),
                auth: match s.auth.as_str() {
                    "agent" => SshAuth::Agent,
                    "key" => SshAuth::KeyFile {
                        path: s.key_file().ok_or("the SSH tunnel has no key file")?,
                        passphrase: ssh_secret,
                    },
                    "password" => SshAuth::Password(ssh_secret.ok_or("the SSH tunnel has no password; edit the connection and enter it")?),
                    other => return Err(format!("unknown SSH login method {other:?}")),
                },
                known_hosts,
            }),
        };
        Ok(Profile {
            id: self.id.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            password,
            db: self.db,
            tls: self.tls,
            tls_insecure: self.tls_insecure,
            tag: Tag::parse(&self.tag).ok_or_else(|| format!("unknown tag {:?}", self.tag))?,
            ephemeral_index: self.ephemeral_index,
            substring_index: false,
            scan_profile: match &self.scan_profile {
                Some(p) => Some(ScanProfile::parse(p).ok_or_else(|| format!("unknown scan profile {p:?}"))?),
                None => None,
            },
            scan_target: ScanTarget::parse(&self.scan_target).ok_or_else(|| format!("unknown scan target {:?}", self.scan_target))?,
            live: false,
            index_mode: match &self.index_mode {
                Some(m) => Some(IndexMode::parse(m).ok_or_else(|| format!("unknown index mode {m:?}"))?),
                None => None,
            },
            ssh,
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    version: u32,
    profiles: Vec<StoredProfile>,
}

/// The profile file. Every change reads, edits and rewrites it under one
/// lock; the file is replaced by a rename so a crash leaves the old one.
pub struct ProfileStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ProfileStore {
    pub fn new(path: PathBuf) -> ProfileStore {
        ProfileStore {
            path,
            lock: Mutex::new(()),
        }
    }

    fn read(&self) -> Result<Vec<StoredProfile>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("could not read {}: {e}", self.path.display())),
        };
        let file: ProfileFile = serde_json::from_str(&text).map_err(|e| format!("{} is not a profile file: {e}", self.path.display()))?;
        if file.version > FILE_VERSION {
            return Err(format!(
                "{} was written by a newer version of the app (format {})",
                self.path.display(),
                file.version
            ));
        }
        Ok(file.profiles)
    }

    fn write(&self, profiles: Vec<StoredProfile>) -> Result<(), String> {
        let file = ProfileFile {
            version: FILE_VERSION,
            profiles,
        };
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        let fail = |e: std::io::Error| format!("could not write {}: {e}", self.path.display());
        // the app's data directory and everything in it is the user's alone
        if let Some(dir) = self.path.parent() {
            keyfold_core::private::own_dir(dir).map_err(fail)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        keyfold_core::private::create_file(&tmp).map_err(fail)?;
        std::fs::write(&tmp, text).map_err(fail)?;
        std::fs::rename(&tmp, &self.path).map_err(fail)
    }

    pub fn list(&self) -> Result<Vec<StoredProfile>, String> {
        let _guard = self.lock.lock().map_err(|_| "profile store poisoned")?;
        self.read()
    }

    pub fn get(&self, id: &str) -> Result<Option<StoredProfile>, String> {
        Ok(self.list()?.into_iter().find(|p| p.id == id))
    }

    /// Insert the profile, or replace the one with its id in place.
    pub fn upsert(&self, profile: StoredProfile) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|_| "profile store poisoned")?;
        let mut all = self.read()?;
        match all.iter_mut().find(|p| p.id == profile.id) {
            Some(slot) => *slot = profile,
            None => all.push(profile),
        }
        self.write(all)
    }

    /// Change one stored profile; `false` when the id is not stored.
    pub fn update(&self, id: &str, change: impl FnOnce(&mut StoredProfile)) -> Result<bool, String> {
        let _guard = self.lock.lock().map_err(|_| "profile store poisoned")?;
        let mut all = self.read()?;
        let Some(slot) = all.iter_mut().find(|p| p.id == id) else {
            return Ok(false);
        };
        change(slot);
        self.write(all)?;
        Ok(true)
    }

    pub fn remove(&self, id: &str) -> Result<Option<StoredProfile>, String> {
        let _guard = self.lock.lock().map_err(|_| "profile store poisoned")?;
        let mut all = self.read()?;
        let Some(at) = all.iter().position(|p| p.id == id) else {
            return Ok(None);
        };
        let removed = all.remove(at);
        self.write(all)?;
        Ok(Some(removed))
    }
}

/// Index folder of a stored profile. Only a UUID names one, so an id from
/// the webview can never point a delete anywhere else.
fn index_dir(cache_dir: &Path, id: &str) -> Result<PathBuf, String> {
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| format!("not a profile id: {id:?}"))?;
    Ok(cache_dir.join("indexes").join(uuid.hyphenated().to_string()))
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

fn remove_index(cache_dir: &Path, id: &str) -> Result<(), String> {
    let dir = index_dir(cache_dir, id)?;
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not delete {}: {e}", dir.display())),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

async fn refuse_if_open(sessions: &Sessions, id: &str, what: &str) -> Result<(), String> {
    if sessions.map.read().await.values().any(|s| s.profile.id == id) {
        return Err(format!("disconnect before you {what}"));
    }
    Ok(())
}

/// A stored profile as the connection list shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRow {
    #[serde(flatten)]
    profile: StoredProfile,
    /// Size of the profile's index folder on disk.
    index_bytes: u64,
}

#[tauri::command]
pub async fn profiles_list(app: AppHandle, store: State<'_, Arc<ProfileStore>>) -> Result<Vec<ProfileRow>, String> {
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let store = Arc::clone(&store);
    blocking(move || {
        Ok(store
            .list()?
            .into_iter()
            .map(|profile| ProfileRow {
                index_bytes: index_dir(&cache_dir, &profile.id).map(|d| dir_size(&d)).unwrap_or(0),
                profile,
            })
            .collect())
    })
    .await
}

/// What the editor sends: a profile without an id is a new one.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileInput {
    id: Option<String>,
    name: String,
    host: String,
    port: u16,
    db: i64,
    username: Option<String>,
    tls: bool,
    tls_insecure: bool,
    tag: String,
    scan_profile: Option<String>,
    scan_target: String,
    ephemeral_index: Option<bool>,
    #[serde(default)]
    index_mode: Option<String>,
    #[serde(default)]
    ssh: Option<StoredSsh>,
}

/// Create or change a stored connection. `password`: `None` keeps what
/// the keychain has, an empty string removes it, anything else replaces
/// it. The keychain is written first; when it refuses, nothing is saved.
///
/// The index folder belongs to the profile id, so a change of host or port
/// drops it: those keys came from another server and would otherwise be
/// merged with the new server's.
#[tauri::command]
pub async fn profile_save(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    store: State<'_, Arc<ProfileStore>>,
    profile: ProfileInput,
    password: Option<String>,
    ssh_secret: Option<String>,
) -> Result<StoredProfile, String> {
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let open = match &profile.id {
        Some(id) => sessions.map.read().await.values().any(|s| &s.profile.id == id),
        None => false,
    };
    let store = Arc::clone(&store);
    blocking(move || {
        let existing = match &profile.id {
            Some(id) => Some(store.get(id)?.ok_or_else(|| format!("no stored connection {id}"))?),
            None => None,
        };
        let moved = existing
            .as_ref()
            .is_some_and(|e| e.host != profile.host.trim() || e.port != profile.port);
        if moved && open {
            return Err("disconnect before you change the host or port".into());
        }
        let mut stored = StoredProfile {
            id: existing
                .as_ref()
                .map_or_else(|| uuid::Uuid::new_v4().hyphenated().to_string(), |e| e.id.clone()),
            name: profile.name,
            host: profile.host,
            port: profile.port,
            db: profile.db,
            username: profile.username,
            tls: profile.tls,
            tls_insecure: profile.tls_insecure,
            tag: profile.tag,
            scan_profile: profile.scan_profile.filter(|p| !p.is_empty()),
            scan_target: profile.scan_target,
            ephemeral_index: profile.ephemeral_index,
            index_mode: profile.index_mode.filter(|m| !m.is_empty()),
            password_saved: existing.as_ref().is_some_and(|e| e.password_saved),
            ssh: profile.ssh,
            ssh_secret_saved: existing.as_ref().is_some_and(|e| e.ssh_secret_saved),
        }
        .validated()?;
        match password.as_deref() {
            None => {}
            Some("") => {
                if stored.password_saved {
                    secrets::delete(&stored.id)?;
                }
                stored.password_saved = false;
            }
            Some(p) => {
                secrets::set(&stored.id, p)?;
                stored.password_saved = true;
            }
        }
        // the bastion's secret: the same three answers, in its own entry;
        // a tunnel that is gone or asks the agent keeps none
        let had_secret = existing.as_ref().is_some_and(|e| e.ssh_secret_saved);
        let wants_secret = stored.ssh.as_ref().is_some_and(|s| s.auth != "agent");
        let account = secrets::ssh_account(&stored.id);
        match ssh_secret.as_deref() {
            _ if !wants_secret => {
                if had_secret {
                    secrets::delete(&account)?;
                }
                stored.ssh_secret_saved = false;
            }
            None => stored.ssh_secret_saved = had_secret,
            Some("") => {
                if had_secret {
                    secrets::delete(&account)?;
                }
                stored.ssh_secret_saved = false;
            }
            Some(secret) => {
                secrets::set(&account, secret)?;
                stored.ssh_secret_saved = true;
            }
        }
        store.upsert(stored.clone())?;
        if moved {
            remove_index(&cache_dir, &stored.id)?;
        }
        tracing::info!(id = %stored.id, name = %stored.name, new = existing.is_none(), moved, "stored connection saved");
        Ok(stored)
    })
    .await
}

/// Delete a stored connection with its keychain entry and index folder.
#[tauri::command]
pub async fn profile_delete(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    store: State<'_, Arc<ProfileStore>>,
    id: String,
) -> Result<(), String> {
    refuse_if_open(&sessions, &id, "delete this connection").await?;
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let store = Arc::clone(&store);
    blocking(move || {
        let Some(removed) = store.remove(&id)? else {
            return Err(format!("no stored connection {id}"));
        };
        if removed.password_saved
            && let Err(e) = secrets::delete(&id)
        {
            tracing::warn!(%id, "keychain entry not removed: {e}");
        }
        if removed.ssh_secret_saved
            && let Err(e) = secrets::delete(&secrets::ssh_account(&id))
        {
            tracing::warn!(%id, "keychain entry of the SSH tunnel not removed: {e}");
        }
        remove_index(&cache_dir, &id)?;
        tracing::info!(%id, name = %removed.name, "stored connection deleted");
        Ok(())
    })
    .await
}

/// Drop the index folder of a stored connection; the next connect rescans.
#[tauri::command]
pub async fn profile_clear_index(app: AppHandle, sessions: State<'_, Sessions>, id: String) -> Result<(), String> {
    refuse_if_open(&sessions, &id, "clear its index").await?;
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    blocking(move || remove_index(&cache_dir, &id)).await
}

/// An index folder that belongs to no stored connection: a connect by URL
/// leaves one per server and database (`host_port_db`), and a profile
/// whose delete failed half way leaves its UUID folder.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrphanIndex {
    name: String,
    bytes: u64,
}

/// The folders under `indexes/` whose name is not in `stored`, by name.
fn orphan_indexes(cache_dir: &Path, stored: &[String]) -> Vec<OrphanIndex> {
    let Ok(entries) = std::fs::read_dir(cache_dir.join("indexes")) else {
        return Vec::new();
    };
    let mut out: Vec<OrphanIndex> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .filter(|(name, _)| !stored.contains(name))
        .map(|(name, path)| OrphanIndex {
            bytes: dir_size(&path),
            name,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Delete one orphan folder. The name has to be one the listing shows, so
/// nothing the webview sends can name a path outside `indexes/` or the
/// folder of a stored connection.
fn remove_orphan_index(cache_dir: &Path, stored: &[String], name: &str) -> Result<(), String> {
    if !orphan_indexes(cache_dir, stored).iter().any(|o| o.name == name) {
        return Err(format!("no such index folder: {name:?}"));
    }
    let dir = cache_dir.join("indexes").join(name);
    std::fs::remove_dir_all(&dir).map_err(|e| format!("could not delete {}: {e}", dir.display()))
}

#[tauri::command]
pub async fn indexes_orphans(app: AppHandle, store: State<'_, Arc<ProfileStore>>) -> Result<Vec<OrphanIndex>, String> {
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let store = Arc::clone(&store);
    blocking(move || {
        let stored: Vec<String> = store.list()?.into_iter().map(|p| p.id).collect();
        Ok(orphan_indexes(&cache_dir, &stored))
    })
    .await
}

#[tauri::command]
pub async fn index_orphan_clear(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    store: State<'_, Arc<ProfileStore>>,
    name: String,
) -> Result<(), String> {
    refuse_if_open(&sessions, &name, "clear its index").await?;
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let store = Arc::clone(&store);
    blocking(move || {
        let stored: Vec<String> = store.list()?.into_iter().map(|p| p.id).collect();
        remove_orphan_index(&cache_dir, &stored, &name)
    })
    .await
}

/// The editor's fields from a pasted `redis://` or `valkey://` URL.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlParts {
    name: String,
    host: String,
    port: u16,
    db: i64,
    username: Option<String>,
    password: Option<String>,
    tls: bool,
    tls_insecure: bool,
}

#[tauri::command]
pub fn profile_from_url(url: String) -> Result<UrlParts, String> {
    let p = Profile::from_url(url.trim()).map_err(|e| e.to_string())?;
    Ok(UrlParts {
        name: format!("{}:{}", p.host, p.port),
        host: p.host,
        port: p.port,
        db: p.db,
        username: p.username,
        password: p.password,
        tls: p.tls,
        tls_insecure: p.tls_insecure,
    })
}

/// Connect a stored profile: its password comes from the keychain, its
/// index lives in the folder named by its id.
#[tauri::command]
pub async fn connect_profile(
    app: AppHandle,
    sessions: State<'_, Sessions>,
    store: State<'_, Arc<ProfileStore>>,
    profile_id: String,
) -> Result<ConnectionDto, String> {
    let store = Arc::clone(&store);
    let wanted = profile_id.clone();
    let known_hosts = known_hosts_file(&app)?;
    let profile = blocking(move || {
        let stored = store.get(&wanted)?.ok_or_else(|| format!("no stored connection {wanted}"))?;
        let password = if stored.password_saved {
            Some(secrets::get(&stored.id)?.ok_or_else(|| {
                format!(
                    "the password of {:?} is no longer in the OS keychain; edit the connection and enter it again",
                    stored.name
                )
            })?)
        } else {
            None
        };
        let ssh_secret = if stored.ssh_secret_saved {
            Some(secrets::get(&secrets::ssh_account(&stored.id))?.ok_or_else(|| {
                format!(
                    "the SSH secret of {:?} is no longer in the OS keychain; edit the connection and enter it again",
                    stored.name
                )
            })?)
        } else {
            None
        };
        stored.to_profile_with(password, ssh_secret, Some(known_hosts))
    })
    .await?;
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let id = sessions.new_id();
    let session = Arc::new(Session::open_profile(id, profile, cache_dir, None).await?);
    adopt(&app, &sessions, session).await
}

/// SSH host keys the user accepted live in the app's own file (CONCEPT
/// 3.4); the user's `~/.ssh/known_hosts` is read, never written.
fn known_hosts_file(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join("known_hosts"))
}

/// A host key the user read and accepted, as a connect reported it
/// (`ssh-host-key-unknown`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostKeyInput {
    host: String,
    port: u16,
    fingerprint: String,
    openssh: String,
}

/// Record an SSH host key the user accepted. A key that differs from one
/// on record is never offered, so this only ever adds a first key.
#[tauri::command]
pub async fn ssh_trust(app: AppHandle, key: HostKeyInput) -> Result<(), String> {
    let file = known_hosts_file(&app)?;
    blocking(move || {
        let key = keyfold_core::tunnel::HostKey {
            host: key.host,
            port: key.port,
            fingerprint: key.fingerprint,
            openssh: key.openssh,
            changed: false,
        };
        keyfold_core::tunnel::trust_host_key(&file, &key).map_err(|e| e.to_string())?;
        tracing::info!(host = %key.host, port = key.port, fingerprint = %key.fingerprint, "ssh host key trusted");
        Ok(())
    })
    .await
}

/// Keep a header change to the scan options on the stored profile, when
/// the session runs on one.
pub async fn remember_scan_options(store: &Arc<ProfileStore>, profile_id: &str, profile: ScanProfile, target: ScanTarget) {
    let store = Arc::clone(store);
    let id = profile_id.to_string();
    let saved = blocking(move || {
        store.update(&id, |p| {
            p.scan_profile = Some(profile.as_str().to_string());
            p.scan_target = target.as_str().to_string();
        })
    })
    .await;
    if let Err(e) = saved {
        tracing::warn!("scan options not saved on the profile: {e}");
    }
}

/// Keep a change of the index mode on the stored profile, when the
/// session runs on one.
pub async fn remember_index_mode(store: &Arc<ProfileStore>, profile_id: &str, mode: Option<IndexMode>) {
    let store = Arc::clone(store);
    let id = profile_id.to_string();
    let saved = blocking(move || store.update(&id, |p| p.index_mode = mode.map(|m| m.as_str().to_string()))).await;
    if let Err(e) = saved {
        tracing::warn!("index mode not saved on the profile: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (ProfileStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("keyfold-profiles-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (ProfileStore::new(dir.join("data").join("profiles.json")), dir)
    }

    fn sample(id: &str, name: &str) -> StoredProfile {
        StoredProfile {
            id: id.into(),
            name: name.into(),
            host: "cache.example".into(),
            port: 6380,
            db: 2,
            username: Some("browser".into()),
            tls: true,
            tls_insecure: false,
            tag: "staging".into(),
            scan_profile: None,
            scan_target: "auto".into(),
            ephemeral_index: None,
            index_mode: None,
            password_saved: true,
            ssh: None,
            ssh_secret_saved: false,
        }
    }

    #[test]
    fn the_index_mode_is_optional_and_normalised() {
        let mut p = sample("a", "A");
        assert_eq!(
            p.clone().validated().unwrap().to_profile_with(None, None, None).unwrap().index_mode,
            None
        );
        p.index_mode = Some("on-demand".into());
        let v = p.validated().unwrap();
        assert_eq!(v.index_mode.as_deref(), Some("onDemand"));
        assert_eq!(v.to_profile_with(None, None, None).unwrap().index_mode, Some(IndexMode::OnDemand));
        let mut bad = sample("a", "A");
        bad.index_mode = Some("some".into());
        assert!(bad.validated().is_err());
        // a file written before the field existed
        let old: StoredProfile = serde_json::from_str(r#"{"id":"x","name":"n","host":"h","port":1,"tag":"dev"}"#).unwrap();
        assert_eq!(old.index_mode, None);
    }

    #[test]
    fn the_ssh_tunnel_of_a_profile_is_checked_and_carries_no_secret() {
        use keyfold_core::tunnel::SshAuth;
        let ssh = |auth: &str, key: Option<&str>| StoredSsh {
            host: " bastion.example ".into(),
            port: 2222,
            user: " ops ".into(),
            auth: auth.into(),
            key_path: key.map(str::to_string),
        };
        let mut p = sample("a", "A");
        p.ssh = Some(ssh("key", Some("/keys/id_ed25519")));
        p.ssh_secret_saved = true;
        let v = p.clone().validated().unwrap();
        let s = v.ssh.clone().unwrap();
        assert_eq!((s.host.as_str(), s.user.as_str(), s.port), ("bastion.example", "ops", 2222));
        // what is written to disk names the key file and never a secret
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("id_ed25519") && json.contains("\"sshSecretSaved\":true"), "{json}");
        let profile = v
            .to_profile_with(None, Some("the passphrase".into()), Some("/data/known_hosts".into()))
            .unwrap();
        let config = profile.ssh.unwrap();
        assert_eq!(
            config.auth,
            SshAuth::KeyFile {
                path: "/keys/id_ed25519".into(),
                passphrase: Some("the passphrase".into())
            }
        );
        assert_eq!(config.known_hosts.as_deref(), Some(std::path::Path::new("/data/known_hosts")));
        assert!(!format!("{config:?}").contains("the passphrase"));

        // the agent keeps no secret; a password login needs one to connect
        p.ssh = Some(ssh("agent", Some("ignored")));
        let v = p.clone().validated().unwrap();
        assert!(!v.ssh_secret_saved && v.ssh.as_ref().unwrap().key_path.is_none());
        p.ssh = Some(ssh("password", None));
        let v = p.clone().validated().unwrap();
        assert!(v.to_profile_with(None, None, None).is_err());
        assert_eq!(
            v.to_profile_with(None, Some("pw".into()), None).unwrap().ssh.unwrap().auth,
            SshAuth::Password("pw".into())
        );

        for bad in [
            ssh("key", None),
            ssh("telnet", None),
            StoredSsh {
                user: " ".into(),
                ..ssh("agent", None)
            },
            StoredSsh {
                port: 0,
                ..ssh("agent", None)
            },
            StoredSsh {
                host: "a b".into(),
                ..ssh("agent", None)
            },
        ] {
            p.ssh = Some(bad.clone());
            assert!(p.clone().validated().is_err(), "{bad:?}");
        }
        // no tunnel, no secret; and a file from before the field existed
        p.ssh = None;
        assert!(!p.clone().validated().unwrap().ssh_secret_saved);
        let old: StoredProfile = serde_json::from_str(r#"{"id":"x","name":"n","host":"h","port":1,"tag":"dev"}"#).unwrap();
        assert!(old.ssh.is_none() && !old.ssh_secret_saved);
        assert_eq!(secrets::ssh_account("6f1c"), "6f1c:ssh");
    }

    #[test]
    fn profiles_survive_a_reopen_in_order() {
        let (store, dir) = temp_store("roundtrip");
        assert!(store.list().unwrap().is_empty(), "no file yet is an empty list");
        store.upsert(sample("a", "orders")).unwrap();
        store.upsert(sample("b", "sessions")).unwrap();
        store
            .upsert(StoredProfile {
                name: "orders (eu)".into(),
                ..sample("a", "")
            })
            .unwrap();

        let reopened = ProfileStore::new(dir.join("data").join("profiles.json"));
        let names: Vec<String> = reopened.list().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["orders (eu)", "sessions"], "an update keeps its place");

        assert!(reopened.update("b", |p| p.scan_target = "replica".into()).unwrap());
        assert!(!reopened.update("missing", |_| ()).unwrap());
        assert_eq!(reopened.get("b").unwrap().unwrap().scan_target, "replica");
        assert_eq!(reopened.remove("a").unwrap().map(|p| p.id).as_deref(), Some("a"));
        assert!(reopened.remove("a").unwrap().is_none());
        assert_eq!(reopened.list().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_file_holds_no_password_and_is_private() {
        let (store, dir) = temp_store("private");
        store.upsert(sample("a", "orders")).unwrap();
        let path = dir.join("data").join("profiles.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"passwordSaved\": true"));
        assert!(!text.to_lowercase().contains("\"password\""), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_newer_or_broken_file_is_refused_not_overwritten() {
        let (store, dir) = temp_store("newer");
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let path = dir.join("data").join("profiles.json");
        std::fs::write(&path, r#"{"version": 99, "profiles": []}"#).unwrap();
        assert!(store.list().unwrap_err().contains("newer version"));
        assert!(store.upsert(sample("a", "orders")).is_err());
        assert!(std::fs::read_to_string(&path).unwrap().contains("99"));
        std::fs::write(&path, "not json").unwrap();
        assert!(store.upsert(sample("a", "orders")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validation_trims_and_refuses_unusable_fields() {
        let ok = StoredProfile {
            name: "  orders ".into(),
            host: " cache.example ".into(),
            username: Some("  ".into()),
            tls: false,
            tls_insecure: true,
            ..sample("a", "")
        }
        .validated()
        .unwrap();
        assert_eq!(
            (ok.name.as_str(), ok.host.as_str(), ok.username, ok.tls_insecure),
            ("orders", "cache.example", None, false)
        );
        for bad in [
            StoredProfile {
                name: " ".into(),
                ..sample("a", "x")
            },
            StoredProfile {
                host: "".into(),
                ..sample("a", "x")
            },
            StoredProfile {
                host: "redis://h".into(),
                ..sample("a", "x")
            },
            StoredProfile {
                port: 0,
                ..sample("a", "x")
            },
            StoredProfile {
                db: -1,
                ..sample("a", "x")
            },
            StoredProfile {
                tag: "live".into(),
                ..sample("a", "x")
            },
            StoredProfile {
                scan_profile: Some("turbo".into()),
                ..sample("a", "x")
            },
            StoredProfile {
                scan_target: "any".into(),
                ..sample("a", "x")
            },
        ] {
            assert!(bad.clone().validated().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_stored_profile_becomes_a_session_profile() {
        let stored = StoredProfile {
            tag: "prod".into(),
            scan_profile: Some("normal".into()),
            scan_target: "replica".into(),
            ..sample("6f1c0c0e-0000-4000-8000-000000000001", "orders")
        };
        let p = stored.to_profile_with(Some("secret".into()), None, None).unwrap();
        assert_eq!(
            (p.id.as_str(), p.name.as_str(), p.db, p.port),
            (stored.id.as_str(), "orders", 2, 6380)
        );
        assert_eq!(p.password.as_deref(), Some("secret"));
        assert_eq!(
            (p.tag, p.scan_profile, p.scan_target),
            (Tag::Prod, Some(ScanProfile::Normal), ScanTarget::Replica)
        );
        assert!(p.tls && p.ephemeral_index());
        // switching the database keeps the folder the index lives in
        assert_eq!(p.with_db(5).id, stored.id);
    }

    #[test]
    fn orphan_indexes_are_the_folders_no_stored_connection_owns() {
        let cache = std::env::temp_dir().join(format!("keyfold-orphans-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let stored = vec!["6f1c0c0e-0000-4000-8000-000000000001".to_string()];
        assert_eq!(orphan_indexes(&cache, &stored), Vec::new(), "no cache directory yet");
        let indexes = cache.join("indexes");
        for name in [stored[0].as_str(), "127.0.0.1_6379_0", "db.example_6380_3"] {
            std::fs::create_dir_all(indexes.join(name)).unwrap();
            std::fs::write(indexes.join(name).join("db0.sqlite"), [0u8; 10]).unwrap();
        }
        std::fs::write(indexes.join("stray-file"), b"x").unwrap();
        std::fs::write(cache.join("outside"), b"x").unwrap();
        let names = |c: &Path| {
            orphan_indexes(c, &stored)
                .into_iter()
                .map(|o| (o.name, o.bytes))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&cache),
            vec![("127.0.0.1_6379_0".to_string(), 10), ("db.example_6380_3".to_string(), 10)]
        );

        // only a listed name is deleted: no stored id, no path, no file
        for bad in [
            stored[0].as_str(),
            "",
            ".",
            "..",
            "../outside",
            "127.0.0.1_6379_0/..",
            "stray-file",
            "nope",
        ] {
            assert!(remove_orphan_index(&cache, &stored, bad).is_err(), "{bad:?}");
        }
        assert!(indexes.join(&stored[0]).join("db0.sqlite").exists());
        assert!(cache.join("outside").exists());
        remove_orphan_index(&cache, &stored, "127.0.0.1_6379_0").unwrap();
        assert_eq!(names(&cache), vec![("db.example_6380_3".to_string(), 10)]);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn only_a_uuid_names_an_index_folder() {
        let cache = Path::new("/cache");
        let id = "6f1c0c0e-0000-4000-8000-000000000001";
        assert_eq!(index_dir(cache, id).unwrap(), Path::new("/cache/indexes").join(id));
        for bad in ["", "..", "../../etc", "127.0.0.1_6379_0", "a/b"] {
            assert!(index_dir(cache, bad).is_err(), "{bad:?}");
        }
    }
}
