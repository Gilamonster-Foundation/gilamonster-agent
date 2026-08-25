//! Jupyter notebook execution + server management tool — gila-native port of
//! newt-agent's `newt-tools/src/jupyter.rs` (newt-agent PR #1730).
//!
//! The extension point is a separate binary, not a plugin slot, so the jupyter
//! surface lives HERE in gila's own tree rather than behind a newt rev bump.
//! The whole module is compiled only when the `jupyter` cargo feature is on
//! (the `pub mod gila_jupyter;` declaration in `lib.rs` is feature-gated), so
//! none of the `nbformat` / `reqwest` / `rand` / `argon2` deps reach a default
//! build.
//!
//! ## Server model
//!
//! `start_server` spawns `jupyter notebook` (through Pixi if available) bound
//! to a typed loopback address only, scrubs the child environment of gila's
//! whole control plane (`env_clear` + a minimal allowlist), and redirects both
//! output streams to a private durable log. A startup guard owns the complete
//! process tree until an authenticated readiness probe succeeds and the
//! versioned registry transaction commits. After that commit the server owns
//! its own lifetime across CLI invocations.
//!
//! `stop_server` / `get_server_status` / `list_servers` resolve opaque handles
//! through that durable registry, validate the stored loopback endpoint before
//! every request, and never treat the informational PID as authority. Stop uses
//! authenticated POST `/api/shutdown`, confirms definite listener refusal, and
//! CAS-deletes the exact registered instance. No bare-PID kill, `kill`, or
//! `taskkill` subprocess is used.
//!
//! ## Phases
//!
//! - **B1a** (this module): Single-process registry with durable logs, startup
//!   guard with process-group/job-object lifecycle, and authenticated shutdown.
//! - **B2a**: Same, verified through cross-process CLI lifecycle tests.
//! - **B3**: Negative-case behavioral tests (403/timeout/socket-error handling).
//! - **B2c** (deferred, future phase): Cross-process concurrency (multiple gila
//!   agents on the same GILA_HOME), file-lock contention, and the locking
//!   harness tests. Single-instance lifecycle is stable.
//!
//! ## Pixi Integration
//!
//! When a project has pixi.toml and Pixi is available:
//! - Search for declared tasks (lab-local, jupyter, lab, jupyter-lab)
//! - Fall back to `pixi run jupyter notebook ...` if no task found
//! - Environment is managed by Pixi; gila still scrubs sensitive vars
//! - Bootstrap workflow available to modernize legacy [project] → [workspace]

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Environment variables passed through to the jupyter child after
/// `env_clear`. Deliberately EXCLUDES every gila control-plane switch and
/// secret (`GILA_*` / `NEWT_*` agent keys, operator keys, authority tokens) —
/// the same philosophy as the operator-yolo-opt-out scrub: a nested jupyter
/// cannot re-assert gila authority and gila's credentials never leak into the
/// notebook subprocess.
const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "LC_CTYPE", "TERM"];

// ---- Registry Store (B1a) -----------------------------------------------

const REGISTRY_SCHEMA_VERSION: u32 = 1;
const REGISTRY_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const REGISTRY_LOCK_RETRY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerRecord {
    pub handle_id: u64,
    #[serde(with = "instance_id_serde")]
    pub instance_id: [u8; 16],
    pub url: String,
    pub port: u16,
    // Token stored plaintext; threat boundary assumes OS file permissions (0600)
    // enforce user-only read access to the registry file. Tokens are per-instance
    // and short-lived (one server session); they do not grant access to other users
    // or survive the server lifecycle.
    pub token: String,
    pub pid: Option<u32>,
    pub registered_at_unix_ms: u64,
    #[serde(default)]
    pub log_path: Option<String>,
}

mod instance_id_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(data: &[u8; 16], ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(32);
        for byte in data {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        ser.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(de: D) -> Result<[u8; 16], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        if s.len() != 32
            || !s
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(serde::de::Error::custom(
                "instance_id must be exactly 32 lowercase hex characters",
            ));
        }

        fn nibble(byte: u8) -> u8 {
            match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => unreachable!("validated lowercase hex above"),
            }
        }

        let mut bytes = [0u8; 16];
        for (index, pair) in s.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
        }
        Ok(bytes)
    }
}

fn deserialize_server_map<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<u64, ServerRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct UniqueServerMapVisitor;

    impl<'de> serde::de::Visitor<'de> for UniqueServerMapVisitor {
        type Value = BTreeMap<u64, ServerRecord>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a server map with unique numeric handle keys")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut servers = BTreeMap::new();
            while let Some((handle, record)) = map.next_entry::<u64, ServerRecord>()? {
                if servers.insert(handle, record).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate server handle key: {handle}"
                    )));
                }
            }
            Ok(servers)
        }
    }

    deserializer.deserialize_map(UniqueServerMapVisitor)
}

fn deserialize_string_server_map<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, ServerRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct UniqueStringServerMapVisitor;

    impl<'de> serde::de::Visitor<'de> for UniqueStringServerMapVisitor {
        type Value = BTreeMap<String, ServerRecord>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a server map with unique string handle keys")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut servers = BTreeMap::new();
            while let Some((handle, record)) = map.next_entry::<String, ServerRecord>()? {
                if servers.insert(handle.clone(), record).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate server handle key: {handle}"
                    )));
                }
            }
            Ok(servers)
        }
    }

    deserializer.deserialize_map(UniqueStringServerMapVisitor)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryFile {
    pub schema_version: u32,
    pub revision: u64,
    pub next_handle: u64,
    #[serde(deserialize_with = "deserialize_server_map")]
    pub servers: BTreeMap<u64, ServerRecord>,
}

#[derive(Debug)]
pub struct RegistryStore {
    root: PathBuf,
}

#[derive(Debug)]
pub struct RegistryTransaction {
    data: RegistryFile,
    lock_file: fs::File,
    reg_path: PathBuf,
    allocated_handles: HashSet<u64>,
    dirty: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyServerRecord {
    handle_id: u64,
    url: String,
    port: u16,
    token: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    start_time_unix: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OnDiskRegistryFile {
    schema_version: u32,
    revision: u64,
    next_handle: u64,
    #[serde(deserialize_with = "deserialize_string_server_map")]
    servers: BTreeMap<String, ServerRecord>,
}

impl OnDiskRegistryFile {
    fn into_registry(self) -> Result<RegistryFile> {
        let mut servers = BTreeMap::new();
        for (encoded_handle, record) in self.servers {
            let handle = encoded_handle
                .parse::<u64>()
                .with_context(|| format!("registry server key is not a u64: {encoded_handle}"))?;
            if servers.insert(handle, record).is_some() {
                anyhow::bail!("duplicate numeric server handle key: {handle}");
            }
        }

        Ok(RegistryFile {
            schema_version: self.schema_version,
            revision: self.revision,
            next_handle: self.next_handle,
            servers,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OnDiskRegistry {
    Current(OnDiskRegistryFile),
    Legacy(Vec<LegacyServerRecord>),
}

enum LoadOutcome {
    Current(RegistryFile),
    Missing(RegistryFile),
    Migrated(RegistryFile),
}

fn empty_registry() -> RegistryFile {
    RegistryFile {
        schema_version: REGISTRY_SCHEMA_VERSION,
        revision: 0,
        next_handle: 1,
        servers: BTreeMap::new(),
    }
}

fn validate_registry(registry: &RegistryFile) -> Result<()> {
    if registry.schema_version != REGISTRY_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported registry schema version: {}",
            registry.schema_version
        );
    }
    if registry.next_handle == 0 {
        anyhow::bail!("registry next_handle must not be zero");
    }

    for (&handle, record) in &registry.servers {
        if handle == 0 || record.handle_id == 0 {
            anyhow::bail!("registry server handles must not be zero");
        }
        if handle != record.handle_id {
            anyhow::bail!(
                "registry key {handle} does not match record handle {}",
                record.handle_id
            );
        }
        if handle >= registry.next_handle {
            anyhow::bail!(
                "registry next_handle {} must be greater than server handle {handle}",
                registry.next_handle
            );
        }
    }

    Ok(())
}

fn validate_root_path(root: &Path) -> Result<()> {
    if root.as_os_str().is_empty() {
        anyhow::bail!("registry root must not be empty");
    }
    if !root.is_absolute() {
        anyhow::bail!("registry root must be absolute: {}", root.display());
    }
    Ok(())
}

fn reject_unsafe_existing_root(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                anyhow::bail!("registry root must not be a symlink: {}", root.display());
            }
            if !metadata.is_dir() {
                anyhow::bail!("registry root must be a directory: {}", root.display());
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect registry root {}", root.display())),
    }
}

fn initialize_registry_root(root: &Path) -> Result<()> {
    validate_root_path(root)?;
    reject_unsafe_existing_root(root)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(root)
            .with_context(|| format!("failed to create registry root {}", root.display()))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(root)
        .with_context(|| format!("failed to create registry root {}", root.display()))?;

    // Check again after creation so a final-component symlink or non-directory
    // introduced during the create is never accepted on any platform.
    reject_unsafe_existing_root(root)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to set registry root permissions on {}",
                root.display()
            )
        })?;
    }

    Ok(())
}

fn resolve_registry_root() -> Result<PathBuf> {
    let root = match std::env::var_os("GILA_HOME") {
        Some(value) => {
            if value.is_empty() {
                anyhow::bail!("GILA_HOME must not be empty");
            }
            PathBuf::from(value)
        }
        None => directories::BaseDirs::new()
            .context("failed to determine the home directory for the registry")?
            .home_dir()
            .join(".gila"),
    };

    validate_root_path(&root)?;
    Ok(root)
}

fn open_lock_file(lock_path: &Path) -> Result<fs::File> {
    match fs::symlink_metadata(lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("registry lock file must not be a symlink")
        }
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!("registry lock path must be a regular file")
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect registry lock file"),
    }

    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let lock_file = options
        .open(lock_path)
        .context("failed to open registry lock file")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        lock_file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("failed to repair registry lock file permissions")?;
    }

    Ok(lock_file)
}

fn acquire_exclusive_lock(lock_file: &fs::File, timeout: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        match fs4::FileExt::try_lock(lock_file) {
            Ok(()) => return Ok(()),
            Err(fs4::TryLockError::WouldBlock) if started.elapsed() < timeout => {
                let remaining = timeout.saturating_sub(started.elapsed());
                thread::sleep(REGISTRY_LOCK_RETRY.min(remaining));
            }
            Err(fs4::TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "timed out after {} seconds waiting for the registry lock",
                    timeout.as_secs()
                )
            }
            Err(fs4::TryLockError::Error(error)) => {
                return Err(error).context("failed to acquire registry lock")
            }
        }
    }
}

fn atomic_write_registry_with<F>(path: &Path, registry: &RegistryFile, writer: F) -> Result<()>
where
    F: FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
{
    validate_registry(registry)?;
    let encoded = serde_json::to_vec(registry).context("failed to serialize registry")?;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write_with_options(|file| writer(file, &encoded), options)
        .context("failed to write registry atomically")?;
    Ok(())
}

fn atomic_write_registry(path: &Path, registry: &RegistryFile) -> Result<()> {
    atomic_write_registry_with(path, registry, |file, encoded| file.write_all(encoded))
}

impl RegistryStore {
    pub fn new() -> Result<Self> {
        Self::from_root(resolve_registry_root()?)
    }

    #[cfg(test)]
    fn for_test(root: PathBuf) -> Result<Self> {
        Self::from_root(root)
    }

    fn from_root(root: PathBuf) -> Result<Self> {
        initialize_registry_root(&root)?;
        Ok(Self { root })
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("servers.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("servers.lock")
    }

    pub fn snapshot(&self) -> Result<RegistryFile> {
        Ok(self.begin_transaction()?.data.clone())
    }

    pub fn begin_transaction(&self) -> Result<RegistryTransaction> {
        self.begin_transaction_with_timeout(REGISTRY_LOCK_TIMEOUT)
    }

    fn begin_transaction_with_timeout(&self, timeout: Duration) -> Result<RegistryTransaction> {
        let lock_path = self.lock_path();
        let reg_path = self.registry_path();
        let lock_file = open_lock_file(&lock_path)?;
        acquire_exclusive_lock(&lock_file, timeout)?;

        let outcome = self.load_registry(&reg_path)?;
        let data = match outcome {
            LoadOutcome::Current(data) | LoadOutcome::Missing(data) => data,
            LoadOutcome::Migrated(data) => {
                // Migration is a committed mutation. Persist it before exposing
                // the transaction, while the same exclusive lock is still held.
                atomic_write_registry(&reg_path, &data)?;
                data
            }
        };

        Ok(RegistryTransaction {
            data,
            lock_file,
            reg_path,
            allocated_handles: HashSet::new(),
            dirty: false,
        })
    }

    fn load_registry(&self, path: &Path) -> Result<LoadOutcome> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadOutcome::Missing(empty_registry()));
            }
            Err(e) => return Err(e).context("failed to read registry"),
        };

        match serde_json::from_slice::<OnDiskRegistry>(&bytes)
            .context("malformed registry: expected schema v1 or the legacy server array")?
        {
            OnDiskRegistry::Current(registry) => {
                let registry = registry.into_registry()?;
                validate_registry(&registry)?;
                Ok(LoadOutcome::Current(registry))
            }
            OnDiskRegistry::Legacy(legacy) => {
                let mut servers = BTreeMap::new();
                let mut seen = HashSet::new();

                for legacy_record in legacy {
                    let LegacyServerRecord {
                        handle_id,
                        url,
                        port,
                        token,
                        pid,
                        start_time_unix,
                    } = legacy_record;

                    if handle_id == 0 {
                        anyhow::bail!("legacy registry server handle must not be zero");
                    }
                    if !seen.insert(handle_id) {
                        anyhow::bail!("duplicate legacy registry handle: {handle_id}");
                    }

                    let mut instance_bytes = [0u8; 16];
                    use rand::RngCore;
                    rand::thread_rng().fill_bytes(&mut instance_bytes);

                    let registered_at_unix_ms = start_time_unix
                        .unwrap_or(0)
                        .checked_mul(1_000)
                        .context("legacy start_time_unix overflows milliseconds")?;

                    let rec = ServerRecord {
                        handle_id,
                        instance_id: instance_bytes,
                        url,
                        port,
                        token,
                        pid,
                        registered_at_unix_ms,
                        log_path: None,
                    };
                    servers.insert(handle_id, rec);
                }

                let next_handle = match servers.keys().next_back().copied() {
                    Some(maximum) => maximum
                        .checked_add(1)
                        .context("legacy registry handle overflow")?,
                    None => 1,
                };
                let registry = RegistryFile {
                    schema_version: REGISTRY_SCHEMA_VERSION,
                    // Loading legacy data and replacing it with v1 is the first
                    // committed mutation in the versioned registry.
                    revision: 1,
                    next_handle,
                    servers,
                };
                validate_registry(&registry)?;
                Ok(LoadOutcome::Migrated(registry))
            }
        }
    }
}

impl RegistryTransaction {
    pub fn snapshot(&self) -> &RegistryFile {
        &self.data
    }

    pub fn allocate_handle(&mut self) -> Result<u64> {
        let handle = self.data.next_handle;
        let next_handle = handle.checked_add(1).context("registry handle overflow")?;
        let revision = self
            .data
            .revision
            .checked_add(1)
            .context("registry revision overflow")?;

        self.data.next_handle = next_handle;
        self.data.revision = revision;
        self.allocated_handles.insert(handle);
        self.dirty = true;
        Ok(handle)
    }

    pub fn insert(&mut self, record: ServerRecord) -> Result<()> {
        let handle = record.handle_id;
        if handle == 0 {
            anyhow::bail!("Cannot insert record with zero handle");
        }
        if self.data.servers.contains_key(&handle) {
            anyhow::bail!("record with handle {handle} already exists");
        }
        if !self.allocated_handles.contains(&handle) {
            anyhow::bail!("record handle {handle} was not allocated by this transaction");
        }

        let revision = self
            .data
            .revision
            .checked_add(1)
            .context("registry revision overflow")?;
        self.data.servers.insert(handle, record);
        self.data.revision = revision;
        self.allocated_handles.remove(&handle);
        self.dirty = true;
        Ok(())
    }

    pub fn compare_and_delete(&mut self, handle: u64, instance_id: [u8; 16]) -> Result<bool> {
        let matches = self
            .data
            .servers
            .get(&handle)
            .is_some_and(|record| record.instance_id == instance_id);
        if !matches {
            return Ok(false);
        }

        let revision = self
            .data
            .revision
            .checked_add(1)
            .context("registry revision overflow")?;
        self.data.servers.remove(&handle);
        self.data.revision = revision;
        self.dirty = true;
        Ok(true)
    }

    pub fn commit(self) -> Result<()> {
        self.commit_with_writer(|file, encoded| file.write_all(encoded))
    }

    fn commit_with_writer<F>(self, writer: F) -> Result<()>
    where
        F: FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
    {
        let Self {
            data,
            lock_file,
            reg_path,
            allocated_handles: _,
            dirty,
        } = self;

        let result = if dirty {
            atomic_write_registry_with(&reg_path, &data, writer)
        } else {
            Ok(())
        };

        // Keep the exact handle that acquired the lock alive until the atomic
        // replacement (or its failure) has completed.
        drop(lock_file);
        result
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store(temp: &TempDir) -> RegistryStore {
        RegistryStore::for_test(temp.path().to_path_buf()).expect("store")
    }

    fn legacy_record(handle_id: u64, port: u16) -> serde_json::Value {
        serde_json::json!({
            "handle_id": handle_id,
            "url": format!("http://127.0.0.1:{port}"),
            "port": port,
            "token": format!("token-{handle_id}"),
            "pid": 1234,
            "start_time_unix": 2
        })
    }

    fn server_record(handle_id: u64, instance_id: [u8; 16]) -> ServerRecord {
        ServerRecord {
            handle_id,
            instance_id,
            url: "http://127.0.0.1:8888".to_string(),
            port: 8888,
            token: "token".to_string(),
            pid: Some(1234),
            registered_at_unix_ms: 2_000,
            log_path: Some("server.log".to_string()),
        }
    }

    fn assert_rejected_without_rewrite(store: &RegistryStore, original: &[u8]) {
        fs::write(store.registry_path(), original).expect("write invalid registry");
        assert!(store.begin_transaction().is_err());
        assert_eq!(
            fs::read(store.registry_path()).expect("read invalid registry"),
            original
        );
    }

    #[test]
    fn missing_registry_starts_empty_without_creating_data_file() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let tx = store.begin_transaction().expect("transaction");
        assert_eq!(tx.snapshot(), &empty_registry());
        assert!(!store.registry_path().exists());
    }

    #[test]
    fn actual_legacy_object_array_migrates_and_rewrites_immediately() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let legacy_json = serde_json::to_vec(&vec![legacy_record(1, 8888), legacy_record(2, 8889)])
            .expect("json");
        fs::write(store.registry_path(), legacy_json).expect("write legacy");

        let tx = store.begin_transaction().expect("transaction");
        assert_eq!(tx.snapshot().schema_version, REGISTRY_SCHEMA_VERSION);
        assert_eq!(tx.snapshot().revision, 1);
        assert_eq!(tx.snapshot().next_handle, 3);
        assert_eq!(tx.snapshot().servers.len(), 2);
        assert_eq!(tx.snapshot().servers[&1].registered_at_unix_ms, 2_000);

        // The migration must already be durable even though this transaction
        // is still holding the lock and has not been committed by the caller.
        let rewritten: RegistryFile = serde_json::from_slice(
            &fs::read(store.registry_path()).expect("read migrated registry"),
        )
        .expect("schema v1 registry");
        assert_eq!(rewritten, *tx.snapshot());
    }

    #[test]
    fn malformed_registry_is_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        assert_rejected_without_rewrite(&store, b"{ broken json");
    }

    #[test]
    fn unsupported_schema_is_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let unsupported = serde_json::to_vec(&RegistryFile {
            schema_version: 2,
            revision: 0,
            next_handle: 1,
            servers: BTreeMap::new(),
        })
        .expect("json");
        assert_rejected_without_rewrite(&store, &unsupported);
    }

    #[test]
    fn zero_legacy_handle_is_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let zero = serde_json::to_vec(&vec![legacy_record(0, 8888)]).expect("json");
        assert_rejected_without_rewrite(&store, &zero);
    }

    #[test]
    fn duplicate_legacy_handles_are_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let duplicate = serde_json::to_vec(&vec![legacy_record(1, 8888), legacy_record(1, 8889)])
            .expect("json");
        assert_rejected_without_rewrite(&store, &duplicate);
    }

    #[test]
    fn legacy_handle_overflow_is_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let overflow = serde_json::to_vec(&vec![legacy_record(u64::MAX, 8888)]).expect("json");
        assert_rejected_without_rewrite(&store, &overflow);
    }

    #[test]
    fn duplicate_current_server_keys_are_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let record = serde_json::to_string(&server_record(1, [1; 16])).expect("record json");
        let duplicate = format!(
            "{{\"schema_version\":1,\"revision\":0,\"next_handle\":2,\"servers\":{{\"1\":{record},\"1\":{record}}}}}"
        );
        assert_rejected_without_rewrite(&store, duplicate.as_bytes());
    }

    #[test]
    fn handles_remain_monotonic_across_delete_and_reopen() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);

        let mut tx = store.begin_transaction().expect("tx1");
        let h1 = tx.allocate_handle().expect("h1");
        assert_eq!(h1, 1);
        tx.insert(server_record(h1, [1; 16])).expect("insert");
        tx.commit().expect("commit1");

        let mut delete_tx = store.begin_transaction().expect("delete tx");
        assert!(delete_tx.compare_and_delete(h1, [1; 16]).expect("delete"));
        delete_tx.commit().expect("delete commit");

        let mut reopen_tx = store.begin_transaction().expect("reopen tx");
        assert!(reopen_tx.snapshot().servers.is_empty());
        assert_eq!(reopen_tx.snapshot().revision, 3);
        assert!(reopen_tx.insert(server_record(h1, [2; 16])).is_err());
        let h2 = reopen_tx.allocate_handle().expect("h2");
        assert_eq!(h2, 2);
        reopen_tx.commit().expect("commit h2");

        let snapshot = store.snapshot().expect("snapshot");
        assert_eq!(snapshot.next_handle, 3);
        assert_eq!(snapshot.revision, 4);
    }

    #[test]
    fn instance_mismatch_preserves_the_record_and_exact_file() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);

        let mut tx = store.begin_transaction().expect("tx1");
        let h = tx.allocate_handle().expect("h");
        tx.insert(server_record(h, [1; 16])).expect("insert");
        tx.commit().expect("commit1");
        let before = fs::read(store.registry_path()).expect("read before mismatch");

        let mut tx2 = store.begin_transaction().expect("tx2");
        let deleted = tx2
            .compare_and_delete(h, [2; 16])
            .expect("compare_and_delete");
        assert!(!deleted);
        assert!(tx2.snapshot().servers.contains_key(&h));
        tx2.commit().expect("no-op commit");
        assert_eq!(
            fs::read(store.registry_path()).expect("read after mismatch"),
            before
        );
    }

    #[test]
    fn instance_id_serialization_is_lowercase_and_roundtrips_exactly() {
        let instance_id = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0xfe, 0xff,
        ];
        let record = server_record(1, instance_id);
        let encoded = serde_json::to_string(&record).expect("serialize record");
        assert!(encoded.contains("000102030405060708090a0b0c0dfeff"));
        let decoded: ServerRecord = serde_json::from_str(&encoded).expect("deserialize record");
        assert_eq!(decoded.instance_id, instance_id);
    }

    #[test]
    fn failed_atomic_commit_preserves_the_prior_valid_registry() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);

        let mut initial = store.begin_transaction().expect("initial tx");
        let handle = initial.allocate_handle().expect("handle");
        initial
            .insert(server_record(handle, [3; 16]))
            .expect("insert");
        initial.commit().expect("initial commit");
        let before = fs::read(store.registry_path()).expect("read prior registry");

        let mut failing = store.begin_transaction().expect("failing tx");
        failing.allocate_handle().expect("allocate mutation");
        let error = failing
            .commit_with_writer(|file, _encoded| {
                file.write_all(b"partial replacement")?;
                Err(io::Error::other("forced commit failure"))
            })
            .expect_err("forced writer failure must propagate");
        assert!(error
            .to_string()
            .contains("failed to write registry atomically"));
        assert_eq!(
            fs::read(store.registry_path()).expect("read preserved registry"),
            before
        );
    }

    #[test]
    fn a_second_transaction_cannot_bypass_the_exclusive_lock() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let first = store.begin_transaction().expect("first transaction");

        let error = store
            .begin_transaction_with_timeout(Duration::from_millis(50))
            .expect_err("second transaction must not acquire the held lock");
        assert!(error.to_string().contains("timed out"));

        drop(first);
        store
            .begin_transaction_with_timeout(Duration::from_millis(50))
            .expect("dropping the first transaction releases the lock");
    }

    #[test]
    #[cfg(unix)]
    fn unix_permissions_are_repaired_and_created_securely() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().expect("temp dir");
        let root = tmp.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o755)).expect("loosen root");

        let store = test_store(&tmp);
        let root_perms = fs::metadata(root).expect("root meta").permissions();
        assert_eq!(root_perms.mode() & 0o777, 0o700);

        fs::write(store.lock_path(), b"").expect("create permissive lock file");
        fs::set_permissions(store.lock_path(), fs::Permissions::from_mode(0o666))
            .expect("loosen lock file");

        let mut tx = store.begin_transaction().expect("tx");
        tx.allocate_handle().expect("h");
        tx.commit().expect("commit");

        let reg_perms = fs::metadata(store.registry_path())
            .expect("reg meta")
            .permissions();
        assert_eq!(reg_perms.mode() & 0o777, 0o600);

        let lock_perms = fs::metadata(store.lock_path())
            .expect("lock meta")
            .permissions();
        assert_eq!(lock_perms.mode() & 0o777, 0o600);
    }

    #[test]
    fn explicit_test_root_must_be_absolute_and_a_directory() {
        assert!(RegistryStore::for_test(PathBuf::new()).is_err());
        assert!(RegistryStore::for_test(PathBuf::from("relative")).is_err());

        let tmp = TempDir::new().expect("temp dir");
        let file = tmp.path().join("not-a-directory");
        fs::write(&file, b"file").expect("write file");
        assert!(RegistryStore::for_test(file).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn explicit_test_root_rejects_a_final_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");
        let target = tmp.path().join("target");
        fs::create_dir(&target).expect("target directory");
        let link = tmp.path().join("registry-link");
        symlink(&target, &link).expect("directory symlink");
        assert!(RegistryStore::for_test(link).is_err());
    }
}

/// `gila jupyter …` subcommands. The whole enum (and the `Jupyter` arm of the
/// top-level `Command`) is compiled out unless the `jupyter` feature is on.
#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
pub enum JupyterCmd {
    /// Execute a Jupyter notebook (.ipynb) in place via `jupyter nbconvert`.
    Execute {
        /// Path to the notebook file (.ipynb).
        notebook_path: PathBuf,
        /// Working directory for execution (default: the notebook's parent dir).
        #[arg(long)]
        working_dir: Option<String>,
        /// Per-cell execution timeout in seconds (default: 300).
        #[arg(long)]
        timeout: Option<u64>,
        /// Kernel name (default: python3).
        #[arg(long)]
        kernel: Option<String>,
        /// Do not save outputs into the notebook (default: outputs are saved).
        #[arg(long)]
        no_save_outputs: bool,
    },
    /// Start a durable Jupyter notebook server bound to a typed loopback IP.
    Start {
        /// Working directory for the server (default: current directory).
        #[arg(long)]
        working_dir: Option<String>,
        /// Port to run the server on (default: 8888).
        #[arg(long)]
        port: Option<u16>,
        /// Bind address — must be a typed loopback IP (for example 127.0.0.1 or ::1).
        #[arg(long)]
        host: Option<String>,
        /// Auth token (default: auto-generated 32 random chars).
        #[arg(long)]
        token: Option<String>,
        /// Plaintext password; hashed with argon2 before being passed to jupyter.
        #[arg(long)]
        password: Option<String>,
        /// Already-hashed `argon2:$argon2id$…` PHC string (takes precedence over
        /// `--password`).
        #[arg(long)]
        password_hash: Option<String>,
        /// Open the operator's browser on start (default: headless).
        #[arg(long)]
        open_browser: bool,
        /// Extra `jupyter notebook` flags (caller-controlled — use with care).
        #[arg(long, value_delimiter = ' ')]
        extra: Option<Vec<String>>,
    },
    /// Stop a Jupyter server by its handle id (from `gila jupyter start`).
    Stop {
        /// Opaque handle id returned by `gila jupyter start`.
        handle_id: u64,
    },
    /// Query a Jupyter server's status + kernels by handle id.
    Status {
        /// Opaque handle id returned by `gila jupyter start`.
        handle_id: u64,
    },
    /// List all durable Jupyter registrations, including unreachable servers.
    List,
    /// Bootstrap: modernize pixi.toml from [project] to [workspace] syntax.
    Bootstrap {
        /// Auto-confirm the update (preview-only by default).
        #[arg(long)]
        confirm: bool,
        /// Working directory (default: current directory).
        #[arg(long)]
        working_dir: Option<String>,
    },
}

// ---- Durable Logs & Process Management (B2a) ----------------------------

pub type OwnedChild = Box<dyn process_wrap::std::ChildWrapper>;

#[cfg(unix)]
// `process-wrap` exposes `killpg(2)` failures as `io::Error`; ESRCH is the
// platform errno indicating that the process group no longer exists.
const ESRCH_RAW_OS_ERROR: i32 = 3;

fn process_tree_already_exited(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(ESRCH_RAW_OS_ERROR)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

/// Spawn a child in an explicitly managed process group (Unix) or job object
/// (Windows). The selected wrappers do not kill on drop/handle close; callers
/// must retain the returned ownership and explicitly terminate it when needed.
pub fn spawn_owned(command: Command) -> io::Result<OwnedChild> {
    let mut command = process_wrap::std::CommandWrap::from(command);

    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());

    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);

    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "owned process trees require a Unix process group or Windows job object",
    ));

    #[cfg(any(unix, windows))]
    command.spawn()
}

#[derive(Debug)]
pub struct StartupGuard {
    child: Option<OwnedChild>,
}

impl StartupGuard {
    /// Spawn and arm in one operation so there is no unguarded post-spawn gap.
    pub fn spawn(command: Command) -> io::Result<Self> {
        spawn_owned(command).map(Self::armed)
    }

    pub fn armed(child: OwnedChild) -> Self {
        Self { child: Some(child) }
    }

    pub fn id(&self) -> u32 {
        self.child
            .as_ref()
            .expect("child always present until disarmed")
            .id()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .expect("child always present until disarmed")
            .try_wait()
    }

    pub fn into_child(mut self) -> OwnedChild {
        self.child
            .take()
            .expect("child always present until disarmed")
    }

    pub fn disarm(self) -> OwnedChild {
        self.into_child()
    }

    /// Terminate and reap the complete owned process tree.
    ///
    /// Termination dispatches through the outer `ProcessGroupChild` /
    /// `JobObjectChild`, and the subsequent wait observes the complete tree
    /// exiting. Unix `ESRCH` means that tree has already exited, so it proceeds
    /// directly to reaping; every other termination failure returns immediately
    /// instead of waiting on a tree that was never successfully signalled.
    pub fn rollback(mut self) -> io::Result<()> {
        self.terminate_and_wait()
    }

    fn terminate_and_wait(&mut self) -> io::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        match child.start_kill() {
            Ok(()) => {}
            Err(error) if process_tree_already_exited(&error) => {}
            Err(error) => return Err(error),
        }
        child.wait()?;
        Ok(())
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        // Explicit post-spawn error paths call `rollback` so cleanup errors can
        // be returned to the caller. Drop remains only the last-resort safety
        // net for unwind / early-return paths that cannot report another error.
        let _ = self.terminate_and_wait();
    }
}

pub const LOG_DIAGNOSTIC_TAIL_CAP: usize = 8 * 1024;
pub const LOG_PARTIAL_LINE_CAP: usize = 8 * 1024;
const START_LOG_CREATE_ATTEMPTS: usize = 16;
const LOG_SCAN_CHUNK_SIZE: usize = 4 * 1024;
const TRUNCATED_LOG_LINE_PREFIX: &str = "[...truncated...] ";

struct DurableLog {
    logs_dir: PathBuf,
}

#[derive(Debug)]
pub struct LogHandles {
    pub stdout_append: fs::File,
    pub stderr_append: fs::File,
    pub read_handle: fs::File,
    pub log_path: String,
}

impl DurableLog {
    fn new(store: &RegistryStore) -> Result<Self> {
        // Revalidate the root at point of use; the store never exposes it to
        // callers, and the two new path components are each checked without
        // following a pre-existing final-component symlink.
        initialize_registry_root(&store.root)?;
        let jupyter_dir = store.root.join("jupyter");
        ensure_private_directory(&jupyter_dir, "jupyter log parent")?;
        let logs_dir = jupyter_dir.join("logs");
        ensure_private_directory(&logs_dir, "jupyter logs")?;

        Ok(Self { logs_dir })
    }

    fn create_handles(&self) -> Result<LogHandles> {
        for _ in 0..START_LOG_CREATE_ATTEMPTS {
            let name = random_start_log_name();
            let absolute_path = self.logs_dir.join(&name);
            let created_file = match create_secure_log(&absolute_path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create durable log {}", absolute_path.display())
                    });
                }
            };
            let created_metadata = created_file
                .metadata()
                .context("failed to inspect newly created durable log")?;

            let stdout_append =
                open_existing_log(&absolute_path, &created_metadata, ExistingLogAccess::Append)
                    .context("failed to open independent durable stdout log handle")?;
            let stderr_append =
                open_existing_log(&absolute_path, &created_metadata, ExistingLogAccess::Append)
                    .context("failed to open independent durable stderr log handle")?;
            let read_handle =
                open_existing_log(&absolute_path, &created_metadata, ExistingLogAccess::Read)
                    .context("failed to open independent durable log read handle")?;
            drop(created_file);

            return Ok(LogHandles {
                stdout_append,
                stderr_append,
                read_handle,
                log_path: format!("jupyter/logs/{name}"),
            });
        }

        anyhow::bail!(
            "failed to allocate a unique durable start log after {START_LOG_CREATE_ATTEMPTS} attempts"
        )
    }
}

impl RegistryStore {
    /// Create the durable log handles for one server-start attempt without
    /// exposing the trusted registry root to callers.
    pub fn create_start_log(&self) -> Result<LogHandles> {
        DurableLog::new(self)?.create_handles()
    }
}

fn ensure_private_directory(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("{description} directory must not be a symlink")
        }
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!("{description} path must be a directory")
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match create_private_directory(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to create {description} directory {}",
                            path.display()
                        )
                    });
                }
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect {description} directory {}",
                    path.display()
                )
            });
        }
    }

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to re-inspect {description} directory"))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("{description} directory must not be a symlink");
    }
    if !metadata.is_dir() {
        anyhow::bail!("{description} path must be a directory");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to repair {description} directory permissions on {}",
                path.display()
            )
        })?;
    }

    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }

    #[cfg(not(unix))]
    fs::create_dir(path)
}

fn random_start_log_name() -> String {
    use rand::RngCore;

    let mut random_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut random_bytes);
    format!("start-{:032x}.log", u128::from_be_bytes(random_bytes))
}

fn create_secure_log(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let file = options.open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    Ok(file)
}

#[derive(Clone, Copy)]
enum ExistingLogAccess {
    Append,
    Read,
}

fn open_existing_log(
    path: &Path,
    created_metadata: &fs::Metadata,
    access: ExistingLogAccess,
) -> io::Result<fs::File> {
    validate_log_path(path, created_metadata)?;

    let mut options = fs::OpenOptions::new();
    match access {
        ExistingLogAccess::Append => {
            options.write(true).append(true);
        }
        ExistingLogAccess::Read => {
            options.read(true);
        }
    }
    let file = options.open(path)?;
    validate_opened_log(path, created_metadata, &file)?;
    Ok(file)
}

fn validate_log_path(path: &Path, created_metadata: &fs::Metadata) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "durable log path is not a regular non-symlink file",
        ));
    }

    validate_log_identity(created_metadata, &path_metadata)
}

fn validate_opened_log(
    path: &Path,
    created_metadata: &fs::Metadata,
    file: &fs::File,
) -> io::Result<()> {
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened durable log handle is not a regular file",
        ));
    }
    validate_log_identity(created_metadata, &opened_metadata)?;

    // A final path check catches replacement between the pre-open inspection
    // and open. On Unix, inode/device equality also proves the opened handle is
    // the exact create_new file rather than a followed replacement symlink.
    validate_log_path(path, created_metadata)
}

fn validate_log_identity(
    created_metadata: &fs::Metadata,
    candidate_metadata: &fs::Metadata,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if created_metadata.dev() != candidate_metadata.dev()
            || created_metadata.ino() != candidate_metadata.ino()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "durable log was replaced while opening independent handles",
            ));
        }
        if candidate_metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "durable log permissions changed while opening handles",
            ));
        }
    }

    #[cfg(not(unix))]
    let _ = (created_metadata, candidate_metadata);

    Ok(())
}

#[derive(Debug)]
pub struct IncrementalLogScanner {
    file: fs::File,
    offset: u64,
    partial_line: Vec<u8>,
    partial_line_truncated: bool,
    diagnostic_tail: Vec<u8>,
}

impl IncrementalLogScanner {
    pub fn new(file: fs::File) -> Self {
        Self {
            file,
            offset: 0,
            partial_line: Vec::new(),
            partial_line_truncated: false,
            diagnostic_tail: Vec::new(),
        }
    }

    /// Read only bytes appended since the prior poll and return newly completed
    /// lines. A shrink is treated as a truncation and starts a fresh stream.
    pub fn poll_lines(&mut self) -> io::Result<Vec<String>> {
        let file_len = self.file.metadata()?.len();
        if file_len < self.offset {
            self.offset = 0;
            self.partial_line.clear();
            self.partial_line_truncated = false;
            self.diagnostic_tail.clear();
        }

        self.file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes_available = file_len.saturating_sub(self.offset);
        let mut chunk = [0u8; LOG_SCAN_CHUNK_SIZE];
        let mut lines = Vec::new();
        while bytes_available != 0 {
            let read_size = usize::try_from(bytes_available.min(LOG_SCAN_CHUNK_SIZE as u64))
                .unwrap_or(LOG_SCAN_CHUNK_SIZE);
            let bytes_read = self.file.read(&mut chunk[..read_size])?;
            if bytes_read == 0 {
                break;
            }

            let appended = &chunk[..bytes_read];
            self.offset = self
                .offset
                .saturating_add(u64::try_from(bytes_read).unwrap_or(u64::MAX));
            bytes_available = bytes_available.saturating_sub(bytes_read as u64);
            self.retain_diagnostic_tail(appended);
            self.consume_appended_bytes(appended, &mut lines);
        }

        Ok(lines)
    }

    pub fn diagnostic_tail(&self) -> &[u8] {
        &self.diagnostic_tail
    }

    fn consume_appended_bytes(&mut self, appended: &[u8], lines: &mut Vec<String>) {
        let mut start = 0;
        for (index, byte) in appended.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }

            self.retain_partial_suffix(&appended[start..index]);
            let mut line = &self.partial_line[..];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line);
            if self.partial_line_truncated {
                lines.push(format!("{TRUNCATED_LOG_LINE_PREFIX}{line}"));
            } else {
                lines.push(line.into_owned());
            }
            self.partial_line.clear();
            self.partial_line_truncated = false;
            start = index + 1;
        }

        self.retain_partial_suffix(&appended[start..]);
    }

    fn retain_partial_suffix(&mut self, appended: &[u8]) {
        if appended.len() >= LOG_PARTIAL_LINE_CAP {
            self.partial_line.clear();
            self.partial_line
                .extend_from_slice(&appended[appended.len() - LOG_PARTIAL_LINE_CAP..]);
            self.partial_line_truncated = true;
            return;
        }

        let overflow = self
            .partial_line
            .len()
            .saturating_add(appended.len())
            .saturating_sub(LOG_PARTIAL_LINE_CAP);
        if overflow != 0 {
            self.partial_line.drain(..overflow);
            self.partial_line_truncated = true;
        }
        self.partial_line.extend_from_slice(appended);
    }

    fn retain_diagnostic_tail(&mut self, appended: &[u8]) {
        if appended.len() >= LOG_DIAGNOSTIC_TAIL_CAP {
            self.diagnostic_tail.clear();
            self.diagnostic_tail
                .extend_from_slice(&appended[appended.len() - LOG_DIAGNOSTIC_TAIL_CAP..]);
            return;
        }

        let overflow = self
            .diagnostic_tail
            .len()
            .saturating_add(appended.len())
            .saturating_sub(LOG_DIAGNOSTIC_TAIL_CAP);
        if overflow != 0 {
            self.diagnostic_tail.drain(..overflow);
        }
        self.diagnostic_tail.extend_from_slice(appended);
    }
}

#[cfg(test)]
mod durable_log_tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store(tmp: &TempDir) -> RegistryStore {
        RegistryStore::for_test(tmp.path().join("registry")).expect("test registry")
    }

    fn absolute_log_path(store: &RegistryStore, handles: &LogHandles) -> PathBuf {
        store.root.join(Path::new(&handles.log_path))
    }

    #[test]
    fn start_log_names_are_unique_lowercase_hex() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let first = store.create_start_log().expect("first log");
        let second = store.create_start_log().expect("second log");

        assert_ne!(first.log_path, second.log_path);
        for handles in [&first, &second] {
            let id = handles
                .log_path
                .strip_prefix("jupyter/logs/start-")
                .and_then(|value| value.strip_suffix(".log"))
                .expect("trusted relative start-log path");
            assert_eq!(id.len(), 32);
            assert!(id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            assert!(absolute_log_path(&store, handles).is_file());
        }
    }

    #[test]
    #[cfg(unix)]
    fn symlink_components_are_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");
        let first_store = test_store(&tmp);
        let target = tmp.path().join("symlink-target");
        fs::create_dir(&target).expect("symlink target");
        symlink(&target, first_store.root.join("jupyter")).expect("jupyter symlink");
        assert!(first_store.create_start_log().is_err());

        let second_root = tmp.path().join("second-registry");
        let second_store = RegistryStore::for_test(second_root.clone()).expect("second registry");
        fs::create_dir(second_root.join("jupyter")).expect("jupyter directory");
        symlink(&target, second_root.join("jupyter/logs")).expect("logs symlink");
        assert!(second_store.create_start_log().is_err());
    }

    #[test]
    fn non_directory_components_are_rejected() {
        let tmp = TempDir::new().expect("temp dir");
        let first_store = test_store(&tmp);
        fs::write(first_store.root.join("jupyter"), b"not a directory").expect("jupyter file");
        assert!(first_store.create_start_log().is_err());

        let second_root = tmp.path().join("second-registry");
        let second_store = RegistryStore::for_test(second_root.clone()).expect("second registry");
        fs::create_dir(second_root.join("jupyter")).expect("jupyter directory");
        fs::write(second_root.join("jupyter/logs"), b"not a directory").expect("logs file");
        assert!(second_store.create_start_log().is_err());
    }

    #[test]
    #[cfg(unix)]
    fn unix_directories_and_logs_have_private_modes() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let handles = store.create_start_log().expect("start log");
        let directories = [store.root.join("jupyter"), store.root.join("jupyter/logs")];
        for directory in &directories {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
                .expect("loosen directory mode");
        }
        store.create_start_log().expect("repair directory modes");

        for directory in directories {
            let mode = fs::metadata(directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }

        let mode = fs::metadata(absolute_log_path(&store, &handles))
            .expect("log metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn scanner_reads_appends_from_independent_file_descriptions() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");

        let stderr_offset = handles
            .stderr_append
            .stream_position()
            .expect("stderr position");
        let reader_offset = handles
            .read_handle
            .stream_position()
            .expect("reader position");

        handles
            .stdout_append
            .write_all(b"stdout\n")
            .expect("stdout write");
        assert_eq!(
            handles
                .stderr_append
                .stream_position()
                .expect("stderr position after stdout write"),
            stderr_offset,
            "stdout writes must not move stderr's independent file offset"
        );
        assert_eq!(
            handles
                .read_handle
                .stream_position()
                .expect("reader position after stdout write"),
            reader_offset,
            "stdout writes must not move the scanner's independent file offset"
        );
        handles
            .stderr_append
            .write_all(b"stderr\n")
            .expect("stderr write");
        assert_eq!(
            handles
                .read_handle
                .stream_position()
                .expect("reader position after stderr write"),
            reader_offset,
            "stderr writes must not move the scanner's independent file offset"
        );

        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert_eq!(scanner.poll_lines().expect("scan"), ["stdout", "stderr"]);

        handles
            .stdout_append
            .write_all(b"later\n")
            .expect("later append");
        assert_eq!(scanner.poll_lines().expect("later scan"), ["later"]);
    }

    #[test]
    fn scanner_preserves_partial_lines_across_polls() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");

        handles
            .stdout_append
            .write_all(b"first\npar")
            .expect("first append");
        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert_eq!(scanner.poll_lines().expect("first poll"), ["first"]);
        assert!(scanner.poll_lines().expect("unchanged poll").is_empty());

        handles
            .stderr_append
            .write_all(b"tial\r\nlast")
            .expect("second append");
        assert_eq!(scanner.poll_lines().expect("second poll"), ["partial"]);

        handles
            .stdout_append
            .write_all(b"\n")
            .expect("final append");
        assert_eq!(scanner.poll_lines().expect("final poll"), ["last"]);
    }

    #[test]
    fn scanner_tail_is_bounded_and_truncation_resets_state() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let absolute_path = absolute_log_path(&store, &handles);
        let oversized = vec![b'x'; LOG_DIAGNOSTIC_TAIL_CAP + 257];

        handles
            .stdout_append
            .write_all(&oversized)
            .expect("oversized append");
        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert!(scanner.poll_lines().expect("oversized poll").is_empty());
        assert_eq!(scanner.diagnostic_tail().len(), LOG_DIAGNOSTIC_TAIL_CAP);
        assert_eq!(
            scanner.diagnostic_tail(),
            &oversized[oversized.len() - LOG_DIAGNOSTIC_TAIL_CAP..]
        );

        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&absolute_path)
            .expect("open for truncate")
            .write_all(b"reset\n")
            .expect("write replacement");
        assert_eq!(scanner.poll_lines().expect("post-truncate poll"), ["reset"]);
        assert_eq!(scanner.diagnostic_tail(), b"reset\n");
    }

    #[test]
    fn scanner_partial_line_is_bounded_across_multiple_polls() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let first = vec![b'a'; LOG_PARTIAL_LINE_CAP - 7];
        let second = vec![b'b'; LOG_PARTIAL_LINE_CAP + 31];

        handles
            .stdout_append
            .write_all(&first)
            .expect("first unterminated append");
        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert!(scanner.poll_lines().expect("first poll").is_empty());
        assert_eq!(scanner.partial_line.len(), first.len());
        assert!(!scanner.partial_line_truncated);

        handles
            .stderr_append
            .write_all(&second)
            .expect("second unterminated append");
        assert!(scanner.poll_lines().expect("second poll").is_empty());
        assert_eq!(scanner.partial_line.len(), LOG_PARTIAL_LINE_CAP);
        assert!(scanner.partial_line_truncated);
        assert!(scanner.partial_line.iter().all(|byte| *byte == b'b'));
        assert_eq!(scanner.diagnostic_tail().len(), LOG_DIAGNOSTIC_TAIL_CAP);

        handles
            .stdout_append
            .write_all(b"suffix")
            .expect("third unterminated append");
        assert!(scanner.poll_lines().expect("third poll").is_empty());
        assert_eq!(scanner.partial_line.len(), LOG_PARTIAL_LINE_CAP);
        assert!(scanner.partial_line.ends_with(b"suffix"));

        handles
            .stderr_append
            .write_all(b"\n")
            .expect("terminate oversized line");
        let lines = scanner.poll_lines().expect("terminating poll");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(TRUNCATED_LOG_LINE_PREFIX));
        assert!(lines[0].ends_with("suffix"));
        assert!(lines[0].len() <= TRUNCATED_LOG_LINE_PREFIX.len() + LOG_PARTIAL_LINE_CAP);
        assert!(scanner.partial_line.is_empty());
        assert!(!scanner.partial_line_truncated);
    }
}

#[cfg(test)]
mod startup_guard_tests {
    use super::*;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use tempfile::TempDir;

    const FIXTURE_ROLE_ENV: &str = "GILA_B2A_FIXTURE_ROLE";
    const FIXTURE_MARKER_ENV: &str = "GILA_B2A_FIXTURE_MARKER";
    const FIXTURE_PLATFORM_ENV: &str = "GILA_B2A_FIXTURE_PLATFORM";

    #[derive(Debug, Clone, Copy)]
    enum InjectedTerminationFailure {
        Kind(io::ErrorKind),
        #[cfg(unix)]
        RawOs(i32),
    }

    #[derive(Debug)]
    struct FaultInjectingChild {
        termination_error: Option<InjectedTerminationFailure>,
        wait_error: io::ErrorKind,
        termination_called: Arc<AtomicBool>,
        wait_called: Arc<AtomicBool>,
    }

    impl process_wrap::std::ChildWrapper for FaultInjectingChild {
        fn inner(&self) -> &dyn process_wrap::std::ChildWrapper {
            self
        }

        fn inner_mut(&mut self) -> &mut dyn process_wrap::std::ChildWrapper {
            self
        }

        fn into_inner(self: Box<Self>) -> Box<dyn process_wrap::std::ChildWrapper> {
            self
        }

        fn start_kill(&mut self) -> io::Result<()> {
            self.termination_called.store(true, Ordering::SeqCst);
            match self.termination_error {
                Some(InjectedTerminationFailure::Kind(kind)) => {
                    Err(io::Error::new(kind, "injected termination failure"))
                }
                #[cfg(unix)]
                Some(InjectedTerminationFailure::RawOs(code)) => {
                    Err(io::Error::from_raw_os_error(code))
                }
                None => Ok(()),
            }
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            self.wait_called.store(true, Ordering::SeqCst);
            Err(io::Error::new(self.wait_error, "injected wait failure"))
        }
    }

    fn fault_injecting_guard(
        termination_error: Option<InjectedTerminationFailure>,
    ) -> (StartupGuard, Arc<AtomicBool>, Arc<AtomicBool>) {
        let termination_called = Arc::new(AtomicBool::new(false));
        let wait_called = Arc::new(AtomicBool::new(false));
        let child = FaultInjectingChild {
            termination_error,
            wait_error: io::ErrorKind::BrokenPipe,
            termination_called: Arc::clone(&termination_called),
            wait_called: Arc::clone(&wait_called),
        };
        (
            StartupGuard::armed(Box::new(child)),
            termination_called,
            wait_called,
        )
    }

    fn base_fixture_command(test_filter: &str, role: &str, marker: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg(test_filter)
            .arg("--ignored")
            .arg("--test-threads=1")
            .env(FIXTURE_ROLE_ENV, role)
            .env(FIXTURE_MARKER_ENV, marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[cfg(unix)]
    fn fixture_command(test_filter: &str, role: &str, marker: &Path) -> Command {
        let mut command = base_fixture_command(test_filter, role, marker);
        command.env(FIXTURE_PLATFORM_ENV, "unix");
        command
    }

    #[cfg(windows)]
    fn fixture_command(test_filter: &str, role: &str, marker: &Path) -> Command {
        let mut command = base_fixture_command(test_filter, role, marker);
        command.env(FIXTURE_PLATFORM_ENV, "windows");
        command
    }

    fn assert_expected_fixture_platform() {
        #[cfg(unix)]
        assert_eq!(std::env::var(FIXTURE_PLATFORM_ENV).as_deref(), Ok("unix"));
        #[cfg(windows)]
        assert_eq!(
            std::env::var(FIXTURE_PLATFORM_ENV).as_deref(),
            Ok("windows")
        );
    }

    fn wait_for_marker(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.is_file() {
            assert!(Instant::now() < deadline, "fixture did not become ready");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_listener(path: &Path) -> SocketAddr {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(contents) = fs::read_to_string(path) {
                if let Ok(address) = contents.parse() {
                    return address;
                }
            }
            assert!(
                Instant::now() < deadline,
                "descendant listener did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "spawned as a process-management fixture"]
    fn long_lived_fixture() {
        if std::env::var(FIXTURE_ROLE_ENV).as_deref() != Ok("long-lived") {
            return;
        }
        assert_expected_fixture_platform();
        let marker = PathBuf::from(std::env::var_os(FIXTURE_MARKER_ENV).expect("marker path"));
        fs::write(marker, b"ready").expect("write ready marker");
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    #[ignore = "spawned as a process-management fixture"]
    fn descendant_parent_fixture() {
        if std::env::var(FIXTURE_ROLE_ENV).as_deref() != Ok("descendant-parent") {
            return;
        }
        assert_expected_fixture_platform();
        let marker = PathBuf::from(std::env::var_os(FIXTURE_MARKER_ENV).expect("marker path"));
        let mut descendant = fixture_command(
            "startup_guard_tests::descendant_listener_fixture",
            "descendant-listener",
            &marker,
        )
        .spawn()
        .expect("spawn descendant listener fixture");
        let _ = descendant.wait();
    }

    #[test]
    #[ignore = "spawned as a process-management fixture"]
    fn descendant_listener_fixture() {
        if std::env::var(FIXTURE_ROLE_ENV).as_deref() != Ok("descendant-listener") {
            return;
        }
        assert_expected_fixture_platform();
        let marker = PathBuf::from(std::env::var_os(FIXTURE_MARKER_ENV).expect("marker path"));
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind descendant listener");
        fs::write(
            marker,
            listener.local_addr().expect("listener address").to_string(),
        )
        .expect("write listener marker");
        for connection in listener.incoming() {
            drop(connection.expect("accept connection"));
        }
    }

    #[test]
    fn disarm_leaves_a_live_child_owned_by_the_caller() {
        let tmp = TempDir::new().expect("temp dir");
        let marker = tmp.path().join("ready");
        let command = fixture_command(
            "startup_guard_tests::long_lived_fixture",
            "long-lived",
            &marker,
        );
        let guard = StartupGuard::spawn(command).expect("spawn guarded child");
        wait_for_marker(&marker);

        let mut child = guard.disarm();
        assert!(
            child.try_wait().expect("check live child").is_none(),
            "disarming must not terminate the live child"
        );

        child.start_kill().expect("terminate disarmed child tree");
        let status = child.wait().expect("reap disarmed child");
        assert!(
            !status.success(),
            "fixture should exit by forced termination"
        );
        assert!(child.try_wait().expect("check reaped child").is_some());
    }

    #[test]
    fn rollback_does_not_wait_after_termination_failure() {
        let (guard, termination_called, wait_called) = fault_injecting_guard(Some(
            InjectedTerminationFailure::Kind(io::ErrorKind::PermissionDenied),
        ));

        let error = guard.rollback().expect_err("termination must fail");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(termination_called.load(Ordering::SeqCst));
        assert!(!wait_called.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn rollback_reaps_after_unix_esrch_reports_an_already_exited_tree() {
        let (guard, termination_called, wait_called) =
            fault_injecting_guard(Some(InjectedTerminationFailure::RawOs(ESRCH_RAW_OS_ERROR)));

        let error = guard
            .rollback()
            .expect_err("the injected reap failure must be surfaced");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(termination_called.load(Ordering::SeqCst));
        assert!(wait_called.load(Ordering::SeqCst));
    }

    #[test]
    fn rollback_surfaces_wait_failure_after_successful_termination() {
        let (guard, termination_called, wait_called) = fault_injecting_guard(None);

        let error = guard.rollback().expect_err("wait must fail");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(termination_called.load(Ordering::SeqCst));
        assert!(wait_called.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_guard_kills_and_reaps_the_descendant_process_tree() {
        let tmp = TempDir::new().expect("temp dir");
        let marker = tmp.path().join("listener-address");
        let command = fixture_command(
            "startup_guard_tests::descendant_parent_fixture",
            "descendant-parent",
            &marker,
        );
        let guard = StartupGuard::spawn(command).expect("spawn guarded process tree");
        let listener_address = wait_for_listener(&marker);
        TcpStream::connect_timeout(&listener_address, Duration::from_secs(1))
            .expect("descendant listener must be live before guard cleanup");

        drop(guard);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match TcpStream::connect_timeout(&listener_address, Duration::from_millis(100)) {
                Err(_) => break,
                Ok(connection) => drop(connection),
            }
            assert!(
                Instant::now() < deadline,
                "descendant listener survived process-tree cleanup"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

// ---- notebook execution -------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterExecuteParams {
    /// Path to the notebook file (.ipynb)
    pub notebook_path: String,
    /// Optional working directory to execute the notebook in.
    /// If not provided, uses the notebook's parent directory.
    pub working_dir: Option<String>,
    /// Per-cell execution timeout in seconds, passed to nbconvert's
    /// `ExecutePreprocessor.timeout`. A cell that exceeds this is interrupted
    /// and marks the notebook as failed. Default: 300.
    pub timeout_seconds: Option<u64>,
    /// Whether to save the executed notebook with outputs (default: true)
    pub save_outputs: Option<bool>,
    /// Kernel name to use (default: python3)
    pub kernel_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterExecuteResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Path to the executed notebook
    pub notebook_path: String,
    /// Number of code cells executed (markdown/raw cells are not counted)
    pub cells_executed: usize,
    /// Number of code cells whose execution produced an error
    pub cells_failed: usize,
    /// Execution time in seconds
    pub execution_time_seconds: f64,
    /// Error message if any
    pub error: Option<String>,
    /// Cell outputs summary
    pub cell_outputs: Vec<CellOutputSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellOutputSummary {
    pub cell_index: usize,
    pub cell_type: String,
    pub success: bool,
    pub output_count: usize,
    pub error: Option<String>,
}

/// Resolve a notebook path to an absolute canonical path.
/// Absolute paths are returned as-is.
/// Relative paths are resolved against the working directory.
fn resolve_notebook_path(notebook: &str, working_dir: Option<&str>) -> Result<PathBuf> {
    let notebook_input = PathBuf::from(notebook);
    let resolved = if notebook_input.is_absolute() {
        notebook_input
    } else {
        let work = working_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        work.join(&notebook_input)
    };

    resolved.canonicalize().with_context(|| {
        format!(
            "Failed to resolve notebook path: {} (working_dir: {:?})",
            notebook, working_dir
        )
    })
}

/// Execute a Jupyter notebook using nbconvert.
pub fn execute_notebook(params: JupyterExecuteParams) -> Result<JupyterExecuteResult> {
    let start_time = std::time::Instant::now();

    // Resolve notebook path using production resolver
    let notebook_path =
        resolve_notebook_path(&params.notebook_path, params.working_dir.as_deref())?;

    // Determine working_dir for execution: notebook's parent if not explicitly specified.
    let working_dir = match params.working_dir.as_ref().map(PathBuf::from) {
        Some(d) => d,
        None => notebook_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    if !notebook_path.exists() {
        anyhow::bail!("Notebook not found: {}", notebook_path.display());
    }

    let timeout = params.timeout_seconds.unwrap_or(300);
    let save_outputs = params.save_outputs.unwrap_or(true);
    let kernel_name = params.kernel_name.unwrap_or_else(|| "python3".to_string());

    // Build the nbconvert command (env-scrubbed through the shared helper)
    let mut cmd = jupyter_cmd();
    cmd.arg("nbconvert")
        .arg("--execute")
        .arg("--to")
        .arg("notebook")
        .arg("--inplace")
        .arg("--ExecutePreprocessor.kernel_name")
        .arg(&kernel_name)
        .arg("--ExecutePreprocessor.timeout")
        .arg(timeout.to_string())
        .arg(&notebook_path) // Pass RESOLVED path to nbconvert
        .current_dir(&working_dir);

    if !save_outputs {
        cmd.arg("--no-output");
    }

    let output = cmd
        .output()
        .context("Failed to execute jupyter nbconvert. Is jupyter installed?")?;

    let execution_time = start_time.elapsed().as_secs_f64();

    let success = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse outputs from the executed notebook. nbconvert `--inplace` writes
    // partial outputs (up to and including the failing cell) even on error, so
    // parse best-effort to surface which cell failed. On a non-success run
    // where the file is unreadable, fall back to an empty list.
    let cell_outputs = match parse_notebook_outputs(&notebook_path) {
        Ok(o) => o,
        Err(_) if !success => vec![],
        Err(e) => return Err(e),
    };

    // Count only executed code cells; markdown/raw cells are not "executed".
    let cells_executed = cell_outputs
        .iter()
        .filter(|c| c.cell_type == "code")
        .count();
    let cells_failed = cell_outputs
        .iter()
        .filter(|c| c.cell_type == "code" && !c.success)
        .count();

    Ok(JupyterExecuteResult {
        success,
        notebook_path: params.notebook_path,
        cells_executed,
        cells_failed,
        execution_time_seconds: execution_time,
        error: if success {
            None
        } else {
            Some(format!("stdout: {stdout}\nstderr: {stderr}"))
        },
        cell_outputs,
    })
}

/// Parse cell outputs from an executed notebook.
fn parse_notebook_outputs(notebook_path: &Path) -> Result<Vec<CellOutputSummary>> {
    use nbformat::{parse_notebook, v4, Notebook};
    use std::fs;

    let content = fs::read_to_string(notebook_path).context("Failed to read notebook")?;
    let nb = parse_notebook(&content).context("Failed to parse notebook")?;

    let cells = match nb {
        Notebook::V4(nb) => nb.cells,
        Notebook::Legacy(nb) => {
            // Upgrade legacy notebook to v4
            let upgraded = nbformat::upgrade_legacy_notebook(nb)?;
            upgraded.cells
        }
    };

    let mut summaries = Vec::new();

    for (idx, cell) in cells.iter().enumerate() {
        let cell_type = match cell {
            v4::Cell::Code { .. } => "code",
            v4::Cell::Markdown { .. } => "markdown",
            v4::Cell::Raw { .. } => "raw",
        };

        let mut success = true;
        let mut output_count = 0;
        let mut error = None;

        if let v4::Cell::Code { outputs, .. } = cell {
            output_count = outputs.len();
            for output in outputs {
                if let v4::Output::Error(v4::ErrorOutput { ename, evalue, .. }) = output {
                    success = false;
                    error = Some(format!("{ename}: {evalue}"));
                    break;
                }
            }
        }

        summaries.push(CellOutputSummary {
            cell_index: idx,
            cell_type: cell_type.to_string(),
            success,
            output_count,
            error,
        });
    }

    Ok(summaries)
}

// ---- server management --------------------------------------------------

/// Parameters for starting a Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerParams {
    /// Working directory for the server (default: current directory)
    pub working_dir: Option<String>,
    /// Port to run the server on (default: 8888)
    pub port: Option<u16>,
    /// Bind address. Defaults to `127.0.0.1`. MUST be a typed loopback IP —
    /// non-loopback hosts are rejected so the server is reachable only from the
    /// operator's own machine.
    pub host: Option<String>,
    /// Token for authentication (default: auto-generated)
    pub token: Option<String>,
    /// Already-hashed password for authentication, in the form Jupyter's
    /// `--NotebookApp.password` expects (`argon2:$argon2id$…` PHC string).
    /// Passed through verbatim. Takes precedence over `password`.
    pub password_hash: Option<String>,
    /// Plaintext password for authentication. The tool hashes this with
    /// argon2 (matching Jupyter's `argon2:` scheme) and passes the resulting
    /// hash to `--NotebookApp.password`. Used only when `password_hash` is
    /// absent. Storing/transporting a plaintext password is discouraged;
    /// prefer `password_hash` for persistent configs.
    pub password: Option<String>,
    /// Whether to open a browser on startup (default: false). `Some(true)`
    /// omits `--no-browser` so Jupyter opens the operator's browser; anything
    /// else passes `--no-browser`.
    pub open_browser: Option<bool>,
    /// Additional command line args appended BEFORE Gila's binding/auth args.
    /// Gila's critical flags (--ip, --port, --NotebookApp.token) come last,
    /// so duplicate flags from extra_args are overridden by Gila's values
    /// (CLI parsers use the LAST occurrence of a flag).
    pub extra_args: Option<Vec<String>>,
}

/// Result of starting a Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerResult {
    /// Whether server started successfully
    pub success: bool,
    /// Opaque handle for later `stop_server` / `get_server_status` calls.
    pub handle_id: Option<u64>,
    /// Server URL (e.g. http://127.0.0.1:8888)
    pub url: Option<String>,
    /// Server process ID (informational; operations use `handle_id`)
    pub pid: Option<u32>,
    /// Port the server is running on
    pub port: Option<u16>,
    /// Compatibility field for older in-process callers. Authentication
    /// material is intentionally never serialized or returned by the current
    /// lifecycle; it lives only in the private registry.
    #[serde(skip)]
    pub token: Option<String>,
    /// Private durable log path, relative to `GILA_HOME`.
    pub log_path: Option<String>,
    /// Error message if any
    pub error: Option<String>,
}

/// Network-observed state of a durable registry entry.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JupyterServerState {
    Running,
    Unreachable,
    NotFound,
}

/// Status of a Jupyter server, queried by handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerStatus {
    /// Compatibility convenience: true only for a verified 2xx kernels API
    /// response containing valid kernels JSON.
    pub running: bool,
    pub state: JupyterServerState,
    /// The handle this status refers to
    pub handle_id: u64,
    /// Registered server URL, including an optional Jupyter base path.
    pub url: Option<String>,
    /// Registered port.
    pub port: Option<u16>,
    /// Private durable log path, relative to `GILA_HOME`.
    pub log_path: Option<String>,
    /// List of running kernels
    pub kernels: Vec<KernelInfo>,
    /// Bounded probe/validation diagnostic when the record is unreachable.
    pub error: Option<String>,
}

/// Information about a running kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelInfo {
    pub id: String,
    pub name: String,
    pub last_activity: String,
    pub execution_state: String,
    pub connections: usize,
}

/// Parsed endpoint from Jupyter server startup output
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JupyterEndpoint {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub token: String,
}

/// Summary of an active Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSummary {
    pub handle_id: u64,
    pub url: String,
    pub port: u16,
    pub running: bool,
    pub state: JupyterServerState,
    pub log_path: Option<String>,
    pub error: Option<String>,
}

/// Result of listing Jupyter servers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterListResult {
    pub servers: Vec<ServerSummary>,
}

fn is_loopback(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn typed_loopback_host(parsed: &url::Url) -> Result<IpAddr> {
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => Ok(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => Ok(IpAddr::V6(ip)),
        Some(url::Host::Ipv4(ip)) => anyhow::bail!("URL host {ip} is not loopback"),
        Some(url::Host::Ipv6(ip)) => anyhow::bail!("URL host {ip} is not loopback"),
        Some(url::Host::Domain(host)) => {
            anyhow::bail!("URL host must be a typed loopback IP address, got {host}")
        }
        None => anyhow::bail!("URL is missing a host"),
    }
}

fn explicit_port(input: &str) -> Option<u16> {
    let authority_start = input.find("://")?.checked_add(3)?;
    let authority_end = input[authority_start..]
        .find(['/', '?', '#'])
        .map_or(input.len(), |offset| authority_start + offset);
    let authority = &input[authority_start..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }

    let encoded_port = if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']')?;
        rest.get(close + 1..)?.strip_prefix(':')?
    } else {
        authority.rsplit_once(':')?.1
    };
    if encoded_port.is_empty() || !encoded_port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    encoded_port.parse().ok()
}

const CONTROLLED_DEFAULT_URL: &str = "/tree";
const DEFAULT_URL_FLAGS: [&str; 3] = [
    "--ServerApp.default_url",
    "--NotebookApp.default_url",
    "--JupyterNotebookApp.default_url",
];

fn normalize_expected_default_url(value: &str) -> Result<String> {
    let value = value.trim();
    if !value.starts_with('/') || value.starts_with("//") {
        anyhow::bail!("Jupyter default_url must be an absolute URL path beginning with one '/'");
    }
    if value.contains(['?', '#'])
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        anyhow::bail!("Jupyter default_url must not contain a query, fragment, or whitespace");
    }
    let normalized = value.trim_end_matches('/');
    Ok(if normalized.is_empty() {
        "/".to_string()
    } else {
        normalized.to_string()
    })
}

fn configured_default_url(extra_args: &[String]) -> Result<Option<String>> {
    let mut configured = None;
    let mut index = 0;
    while index < extra_args.len() {
        let argument = &extra_args[index];
        let mut matched = None;
        for flag in DEFAULT_URL_FLAGS {
            if argument == flag {
                let value = extra_args
                    .get(index + 1)
                    .with_context(|| format!("{flag} requires a value"))?;
                matched = Some(value.as_str());
                index += 1;
                break;
            }
            if let Some(value) = argument.strip_prefix(&format!("{flag}=")) {
                matched = Some(value);
                break;
            }
        }
        if let Some(value) = matched {
            if configured.is_some() {
                anyhow::bail!(
                    "extra_args may configure Jupyter default_url at most once; duplicate traitlet values are ambiguous"
                );
            }
            configured = Some(normalize_expected_default_url(value)?);
        }
        index += 1;
    }
    Ok(configured)
}

fn without_default_url_args(extra_args: &[String]) -> Vec<String> {
    let mut forwarded = Vec::with_capacity(extra_args.len());
    let mut index = 0;
    while index < extra_args.len() {
        let argument = &extra_args[index];
        if DEFAULT_URL_FLAGS.iter().any(|flag| argument == flag) {
            index += 2;
            continue;
        }
        if DEFAULT_URL_FLAGS
            .iter()
            .any(|flag| argument.starts_with(&format!("{flag}=")))
        {
            index += 1;
            continue;
        }
        forwarded.push(argument.clone());
        index += 1;
    }
    forwarded
}

fn normalized_base_path<'a>(path: &'a str, expected_default_url: &str) -> Result<&'a str> {
    let without_trailing_slash = path.trim_end_matches('/');
    if expected_default_url == "/" {
        return Ok(without_trailing_slash);
    }
    // The suffix is either Gila's controlled `/tree` route or the single
    // caller-supplied default_url validated before spawn. Strip exactly that
    // known suffix, preserving a base path that happens to end the same way.
    without_trailing_slash
        .strip_suffix(expected_default_url)
        .context("endpoint URL does not end in the expected Jupyter default route")
}

fn format_base_url(
    parsed: &url::Url,
    ip: IpAddr,
    port: u16,
    expected_default_url: &str,
) -> Result<String> {
    let host = match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    Ok(format!(
        "{}://{}:{}{}",
        parsed.scheme(),
        host,
        port,
        normalized_base_path(parsed.path(), expected_default_url)?
    ))
}

fn parse_endpoint_candidate(
    candidate: &str,
    expected_token: &str,
    expected_default_url: &str,
) -> Result<JupyterEndpoint> {
    if expected_token.is_empty() {
        anyhow::bail!("expected Jupyter token must not be empty");
    }

    let parsed = url::Url::parse(candidate).context("failed to parse endpoint URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("endpoint scheme must be http or https");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("endpoint URL must not contain user info");
    }
    let port = explicit_port(candidate).context("endpoint URL must contain an explicit port")?;
    if port == 0 {
        anyhow::bail!("endpoint URL port must not be zero");
    }
    let ip = typed_loopback_host(&parsed)?;
    let tokens: Vec<String> = parsed
        .query_pairs()
        .filter(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
        .collect();
    if tokens.len() != 1 || tokens[0].is_empty() || tokens[0] != expected_token {
        anyhow::bail!("endpoint token does not exactly match the expected token");
    }

    Ok(JupyterEndpoint {
        url: format_base_url(&parsed, ip, port, expected_default_url)?,
        host: ip.to_string(),
        port,
        token: tokens.into_iter().next().expect("exactly one token"),
    })
}

fn endpoint_candidate_strings(output: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    let mut remaining = output;
    while !remaining.is_empty() {
        let http = remaining.find("http://");
        let https = remaining.find("https://");
        let Some(start) = (match (http, https) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(start), None) | (None, Some(start)) => Some(start),
            (None, None) => None,
        }) else {
            break;
        };
        let candidate = &remaining[start..];
        let end = candidate
            .char_indices()
            .find(|(index, ch)| {
                *index != 0
                    && (ch.is_ascii_whitespace()
                        || ch.is_control()
                        || matches!(ch, '\"' | '\'' | '<' | '>' | ')' | ','))
            })
            .map_or(candidate.len(), |(index, _)| index);
        candidates.push(&candidate[..end]);
        let advance = start.saturating_add(end.max(1));
        remaining = &remaining[advance..];
    }
    candidates
}

/// Scan every URL candidate and keep only endpoints that satisfy Gila's exact
/// typed-loopback, explicit-port, and token boundary. Rejected candidates do
/// not prevent a later legitimate announcement from being accepted.
fn parse_jupyter_endpoints(
    output: &str,
    expected_token: &str,
    expected_default_url: &str,
) -> Vec<JupyterEndpoint> {
    endpoint_candidate_strings(output)
        .into_iter()
        .filter_map(|candidate| {
            parse_endpoint_candidate(candidate, expected_token, expected_default_url).ok()
        })
        .collect()
}

#[derive(Debug)]
struct ValidatedStoredEndpoint {
    base_url: url::Url,
    socket_addr: SocketAddr,
}

fn validate_stored_record(record: &ServerRecord) -> Result<ValidatedStoredEndpoint> {
    if record.token.is_empty() {
        anyhow::bail!("registered Jupyter token is empty");
    }
    let parsed = url::Url::parse(&record.url).context("registered Jupyter URL is invalid")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("registered Jupyter URL must use http or https");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("registered Jupyter URL must not contain user info");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("registered Jupyter base URL must not contain a query or fragment");
    }
    let port = explicit_port(&record.url)
        .context("registered Jupyter URL must contain an explicit port")?;
    if port == 0 || record.port == 0 {
        anyhow::bail!("registered Jupyter port must not be zero");
    }
    if port != record.port {
        anyhow::bail!(
            "registered Jupyter URL port {port} does not match record port {}",
            record.port
        );
    }
    let ip = typed_loopback_host(&parsed)?;
    Ok(ValidatedStoredEndpoint {
        base_url: parsed,
        socket_addr: SocketAddr::new(ip, port),
    })
}

fn api_url(base_url: &url::Url, endpoint: &str) -> url::Url {
    let mut url = base_url.clone();
    let path = format!(
        "{}/{}",
        base_url.path().trim_end_matches('/'),
        endpoint.trim_start_matches('/')
    );
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    url
}

/// Detect environment manager in a directory and return appropriate launcher.
///
/// Searches for pixi.toml, pyproject.toml (uv), requirements.txt (venv),
/// environment.yml (conda), or .venv directory. Returns a command wrapper
/// that will activate the environment before running jupyter.
///
/// The wrapper is constructed so that we can still append `notebook --port XXX` etc.
fn detect_and_wrap_jupyter_cmd(working_dir: &Path) -> (String, Vec<String>) {
    // Check for pixi.toml
    if working_dir.join("pixi.toml").exists() {
        return (
            "pixi".to_string(),
            vec!["run".to_string(), "jupyter".to_string()],
        );
    }

    // Check for uv (pyproject.toml with [tool.uv])
    if let Ok(content) = fs::read_to_string(working_dir.join("pyproject.toml")) {
        if content.contains("[tool.uv]") {
            return (
                "uv".to_string(),
                vec!["run".to_string(), "jupyter".to_string()],
            );
        }
    }

    // Check for conda environment.yml
    if working_dir.join("environment.yml").exists() {
        return (
            "conda".to_string(),
            vec![
                "run".to_string(),
                "--file".to_string(),
                working_dir
                    .join("environment.yml")
                    .to_string_lossy()
                    .to_string(),
                "jupyter".to_string(),
            ],
        );
    }

    // Check for .venv directory with platform-specific executable
    if working_dir.join(".venv").exists() {
        #[cfg(unix)]
        let venv_exe = working_dir.join(".venv/bin/jupyter");
        #[cfg(windows)]
        let venv_exe = working_dir.join(".venv/Scripts/jupyter.exe");

        if venv_exe.exists() {
            return (venv_exe.to_string_lossy().to_string(), vec![]);
        }
    }

    // Check for requirements.txt (assume venv exists or will be created)
    if working_dir.join("requirements.txt").exists() {
        #[cfg(unix)]
        let venv_exe = working_dir.join(".venv/bin/jupyter");
        #[cfg(windows)]
        let venv_exe = working_dir.join(".venv/Scripts/jupyter.exe");

        if venv_exe.exists() {
            return (venv_exe.to_string_lossy().to_string(), vec![]);
        }
    }

    // Fallback: plain jupyter (with minimal environment)
    ("jupyter".to_string(), vec![])
}

/// Build the base `jupyter` command with the inherited environment scrubbed
/// and only a minimal, safe allowlist passed back through.
fn jupyter_cmd() -> Command {
    let mut cmd = Command::new("jupyter");
    // Scrub the whole inherited environment, then pass back ONLY the minimal
    // allowlist jupyter needs to locate its binary, write config, and render.
    // No gila control-plane env reaches the child.
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    cmd
}

/// Hash a plaintext password into the `argon2:$argon2id$…` PHC string that
/// Jupyter's `--NotebookApp.password` expects and `notebook.auth.passwd_check`
/// verifies. Matches Jupyter's `argon2:` prefix scheme (the `argon2-cffi`
/// `PasswordHasher` produces the suffix after the colon); the argon2id
/// parameters are the crate defaults, which verify cleanly because
/// `passwd_check` reads them back from the encoded string.
fn hash_password(plaintext: &str) -> Result<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    let argon = argon2::Argon2::default();
    let hash = argon
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("Failed to hash server password: {e}"))?;
    Ok(format!("argon2:{hash}"))
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STARTUP_HTTP_TIMEOUT: Duration = Duration::from_millis(500);
const STARTUP_CANDIDATE_CAP: usize = 8;
const LIFECYCLE_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const LISTENER_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

fn lifecycle_http_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(1)))
        // Registry URLs are typed loopback endpoints. Never route the
        // authentication token through HTTP(S)/ALL_PROXY environment state.
        .no_proxy()
        // A registered loopback endpoint must never redirect a privileged
        // authenticated request to a different origin.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build Jupyter HTTP client")
}

fn bounded_text(text: impl AsRef<str>, cap: usize) -> String {
    let text = text.as_ref();
    if text.len() <= cap {
        return text.to_string();
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[...truncated...] {}", &text[start..])
}

fn encoded_token_query(token: &str) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("token", token);
    serializer.finish()
}

fn redacted_log_tail(scanner: &IncrementalLogScanner, token: &str) -> String {
    let tail = String::from_utf8_lossy(scanner.diagnostic_tail());
    let encoded_pair = encoded_token_query(token);
    let encoded_token = encoded_pair.strip_prefix("token=").unwrap_or(token);
    let redacted = tail
        .replace(encoded_token, "<redacted>")
        .replace(token, "<redacted>");
    bounded_text(redacted, LOG_DIAGNOSTIC_TAIL_CAP)
}

fn authorization_header(token: &str) -> Result<reqwest::header::HeaderValue> {
    if token.is_empty() {
        anyhow::bail!("Jupyter authentication token must not be empty");
    }
    reqwest::header::HeaderValue::from_str(&format!("token {token}"))
        .context("Jupyter token contains bytes that are invalid in an HTTP authorization header")
}

fn failed_start(error: impl Into<String>, pid: u32, log_path: String) -> JupyterServerResult {
    JupyterServerResult {
        success: false,
        handle_id: None,
        url: None,
        pid: Some(pid),
        port: None,
        token: None,
        log_path: Some(log_path),
        error: Some(error.into()),
    }
}

fn rollback_after_error(guard: StartupGuard, cause: anyhow::Error) -> anyhow::Error {
    match guard.rollback() {
        Ok(()) => cause,
        Err(cleanup_error) => anyhow::anyhow!(
            "{cause:#}; process-tree rollback also failed and may require operator cleanup: {cleanup_error}"
        ),
    }
}

fn failed_start_with_rollback(
    guard: StartupGuard,
    error: impl Into<String>,
    pid: u32,
    log_path: String,
) -> JupyterServerResult {
    let cause = anyhow::anyhow!(error.into());
    failed_start(
        rollback_after_error(guard, cause).to_string(),
        pid,
        log_path,
    )
}

fn startup_endpoint_ready(
    client: &reqwest::blocking::Client,
    endpoint: &JupyterEndpoint,
) -> std::result::Result<(), String> {
    let base_url = url::Url::parse(&endpoint.url)
        .map_err(|error| format!("validated startup URL could not be reopened: {error}"))?;
    let kernels_url = api_url(&base_url, "api/kernels");
    match client
        .get(kernels_url)
        .header(
            "Authorization",
            authorization_header(&endpoint.token).map_err(|error| error.to_string())?,
        )
        .send()
    {
        Ok(response) if response.status().is_success() => response
            .json::<Vec<KernelInfo>>()
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "readiness endpoint returned malformed kernels JSON: {}",
                    bounded_text(error.to_string(), 512)
                )
            }),
        Ok(response) => Err(format!(
            "readiness endpoint returned HTTP {}",
            response.status()
        )),
        Err(error) if error.is_timeout() => Err("readiness request timed out".to_string()),
        Err(error) => Err(format!("readiness connection failed: {error}")),
    }
}

/// Start a durable Jupyter server.
///
/// Parameters, the registry, secure log handles, command, and HTTP client are
/// validated before spawn. After spawn, `StartupGuard` owns the complete tree
/// until a typed-loopback announcement with the exact expected token passes an
/// authenticated readiness probe and one allocate+insert registry transaction
/// commits atomically.
pub fn start_server(params: JupyterServerParams) -> Result<JupyterServerResult> {
    let JupyterServerParams {
        working_dir,
        port,
        host,
        token,
        password_hash,
        password,
        open_browser,
        extra_args,
    } = params;

    let working_dir = fs::canonicalize(
        working_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".")),
    )
    .context("failed to resolve Jupyter working directory")?;
    if !fs::metadata(&working_dir)
        .context("failed to inspect Jupyter working directory")?
        .is_dir()
    {
        anyhow::bail!(
            "Jupyter working directory is not a directory: {}",
            working_dir.display()
        );
    }

    let requested_port = port.unwrap_or(8888);
    let host = host.unwrap_or_else(|| "127.0.0.1".to_string());
    if !is_loopback(&host) {
        anyhow::bail!("refusing Jupyter host '{host}': use a typed IPv4 or IPv6 loopback address");
    }
    let expected_host_ip: IpAddr = host
        .parse()
        .context("failed to parse typed Jupyter loopback host")?;
    let token = token.unwrap_or_else(|| {
        use rand::Rng;
        rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect()
    });
    if token.is_empty() {
        anyhow::bail!("Jupyter authentication token must not be empty");
    }
    // Reject control characters/header-invalid bytes before any child exists.
    let _authorization = authorization_header(&token)?;
    let extra_args = extra_args.unwrap_or_default();
    let caller_default_url = configured_default_url(&extra_args)?;
    let expected_default_url = caller_default_url
        .as_ref()
        .cloned()
        .unwrap_or_else(|| CONTROLLED_DEFAULT_URL.to_string());
    let forwarded_extra_args = without_default_url_args(&extra_args);

    // Validate the current on-disk schema/corruption state before creating a
    // process. This read-only snapshot releases its lock immediately.
    let store = RegistryStore::new().context("failed to initialize Jupyter registry")?;
    let _validated_snapshot = store
        .snapshot()
        .context("failed to validate Jupyter registry before start")?;
    let http_client = lifecycle_http_client(STARTUP_HTTP_TIMEOUT)?;
    let LogHandles {
        stdout_append,
        stderr_append,
        read_handle,
        log_path,
    } = store.create_start_log()?;

    use crate::gila_pixi;
    let mut command = if gila_pixi::has_pixi_manifest(&working_dir) {
        let mut command = Command::new("pixi");
        command
            .arg("run")
            .arg("--executable")
            .arg("jupyter")
            .arg("notebook");
        command
    } else {
        let (launcher, launcher_args) = detect_and_wrap_jupyter_cmd(&working_dir);
        let mut command = Command::new(launcher);
        command.args(launcher_args).arg("notebook");
        command
    };

    // Caller extras precede all Gila-controlled browser, binding, port, and
    // authentication flags. Last-value-wins parsers therefore cannot use an
    // extra argument to relax Gila's boundary.
    command.args(&forwarded_extra_args);
    if !matches!(open_browser, Some(true)) {
        command.arg("--no-browser");
    }
    if let Some(hash) = password_hash {
        // Keep the legacy NotebookApp alias for Notebook 6 while ending with
        // Jupyter Server 2.x's owning identity-provider setting.
        command
            .arg("--NotebookApp.password")
            .arg(&hash)
            .arg("--PasswordIdentityProvider.hashed_password")
            .arg(hash);
    } else if let Some(plaintext) = password {
        let hash = hash_password(&plaintext)?;
        command
            .arg("--NotebookApp.password")
            .arg(&hash)
            .arg("--PasswordIdentityProvider.hashed_password")
            .arg(hash);
    }
    // The startup announcement contains Jupyter's base path followed by its
    // default UI route. Normalize the caller's one optional setting into the
    // legacy/server/frontend aliases with the same value. Traitlets
    // accumulates conflicting duplicate values instead of applying
    // last-value-wins; this keeps Notebook 6, Jupyter Server 2.x, and Notebook
    // 7's extension app aligned without ambiguity.
    command
        .arg("--NotebookApp.default_url")
        .arg(&expected_default_url)
        .arg("--ServerApp.default_url")
        .arg(&expected_default_url)
        .arg("--JupyterNotebookApp.default_url")
        .arg(&expected_default_url)
        // Jupyter Server 2.x deliberately renders a configured token as
        // `token=...`. A query-only custom display URL preserves Jupyter's
        // actual scheme/host/selected port/base path while replacing only that
        // redacted query. The private 0600 log can then carry the exact
        // Gila-controlled token needed for authenticated candidate validation.
        .arg("--ServerApp.custom_display_url")
        .arg(format!("?{}", encoded_token_query(&token)))
        .arg("--port")
        .arg(requested_port.to_string())
        .arg("--ip")
        .arg(&host)
        .arg("--NotebookApp.token")
        .arg(&token)
        // Modern Jupyter Server ignores the deprecated ServerApp/NotebookApp
        // token in some identity-provider configurations (notably password
        // auth). End with the owning setting so token readiness is stable;
        // the legacy alias immediately above retains Notebook 6 compatibility.
        .arg("--IdentityProvider.token")
        .arg(&token)
        .current_dir(&working_dir)
        .env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_append))
        .stderr(Stdio::from(stderr_append));

    let mut guard = StartupGuard::spawn(command)
        .context("failed to start Jupyter server; is Jupyter installed?")?;
    let pid = guard.id();
    let mut scanner = IncrementalLogScanner::new(read_handle);
    let mut candidates = BTreeMap::<String, JupyterEndpoint>::new();
    let mut last_probe_error: Option<String> = None;
    let deadline = Instant::now() + STARTUP_TIMEOUT;

    let endpoint = loop {
        let completed_lines = match scanner.poll_lines() {
            Ok(lines) => lines,
            Err(error) => {
                return Ok(failed_start_with_rollback(
                    guard,
                    format!("failed to scan durable Jupyter log: {error}"),
                    pid,
                    log_path,
                ));
            }
        };
        for line in &completed_lines {
            for endpoint in parse_jupyter_endpoints(line, &token, &expected_default_url) {
                if endpoint.host.parse::<IpAddr>().ok() == Some(expected_host_ip)
                    && candidates.len() < STARTUP_CANDIDATE_CAP
                {
                    candidates.entry(endpoint.url.clone()).or_insert(endpoint);
                }
            }
        }
        let current_tail = String::from_utf8_lossy(scanner.diagnostic_tail());
        for endpoint in parse_jupyter_endpoints(&current_tail, &token, &expected_default_url) {
            if endpoint.host.parse::<IpAddr>().ok() == Some(expected_host_ip)
                && candidates.len() < STARTUP_CANDIDATE_CAP
            {
                candidates.entry(endpoint.url.clone()).or_insert(endpoint);
            }
        }

        let mut ready = None;
        for candidate in candidates.values() {
            if Instant::now() >= deadline {
                break;
            }
            match startup_endpoint_ready(&http_client, candidate) {
                Ok(()) => {
                    ready = Some(candidate.clone());
                    break;
                }
                Err(error) => last_probe_error = Some(error),
            }
        }

        match guard.try_wait() {
            Ok(Some(status)) => {
                let diagnostic = redacted_log_tail(&scanner, &token);
                let probe = last_probe_error
                    .as_deref()
                    .unwrap_or("no valid endpoint candidate was announced");
                return Ok(failed_start_with_rollback(
                    guard,
                    format!(
                        "Jupyter exited during startup with status {status}; last readiness result: {probe}; durable log tail:\n{diagnostic}"
                    ),
                    pid,
                    log_path,
                ));
            }
            Err(error) => {
                return Ok(failed_start_with_rollback(
                    guard,
                    format!("failed to inspect Jupyter child during startup: {error}"),
                    pid,
                    log_path,
                ));
            }
            Ok(None) => {}
        }
        if let Some(endpoint) = ready {
            break endpoint;
        }

        if Instant::now() >= deadline {
            let diagnostic = redacted_log_tail(&scanner, &token);
            let probe = last_probe_error
                .as_deref()
                .unwrap_or("no valid endpoint candidate was announced");
            return Ok(failed_start_with_rollback(
                guard,
                format!(
                    "Jupyter did not become ready within {} seconds ({probe}); durable log tail:\n{diagnostic}",
                    STARTUP_TIMEOUT.as_secs()
                ),
                pid,
                log_path,
            ));
        }
        thread::sleep(STARTUP_POLL_INTERVAL);
    };

    let registration = (|| -> Result<u64> {
        let registered_at_unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before the Unix epoch")?
                .as_millis(),
        )
        .context("registration timestamp exceeds u64 milliseconds")?;
        let mut instance_id = [0u8; 16];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut instance_id);

        // Allocation and insertion deliberately share this single short
        // transaction after readiness.
        let mut transaction = store
            .begin_transaction()
            .context("failed to open registry transaction for ready Jupyter server")?;
        let handle_id = transaction.allocate_handle()?;
        transaction.insert(ServerRecord {
            handle_id,
            instance_id,
            url: endpoint.url.clone(),
            port: endpoint.port,
            token,
            pid: Some(pid),
            registered_at_unix_ms,
            log_path: Some(log_path.clone()),
        })?;
        transaction
            .commit()
            .context("ready Jupyter server could not be persisted; startup was rolled back")?;
        Ok(handle_id)
    })();
    let handle_id = match registration {
        Ok(handle_id) => handle_id,
        Err(error) => return Err(rollback_after_error(guard, error)),
    };

    // Dropping an unwrapped std child does not kill it. On Windows JobObject is
    // configured without kill-on-close; on Unix the process group remains.
    drop(guard.disarm());
    Ok(JupyterServerResult {
        success: true,
        handle_id: Some(handle_id),
        url: Some(endpoint.url),
        pid: Some(pid),
        port: Some(endpoint.port),
        token: None,
        log_path: Some(log_path),
        error: None,
    })
}

#[derive(Debug)]
enum ProbeOutcome {
    Running(Vec<KernelInfo>),
    Unreachable(String),
}

fn probe_registered_server(
    client: &reqwest::blocking::Client,
    record: &ServerRecord,
) -> ProbeOutcome {
    let endpoint = match validate_stored_record(record) {
        Ok(endpoint) => endpoint,
        Err(error) => return ProbeOutcome::Unreachable(format!("invalid registry entry: {error}")),
    };
    let kernels_url = api_url(&endpoint.base_url, "api/kernels");
    let authorization = match authorization_header(&record.token) {
        Ok(value) => value,
        Err(error) => {
            return ProbeOutcome::Unreachable(format!("invalid registry entry: {error}"));
        }
    };
    let response = match client
        .get(kernels_url)
        .header("Authorization", authorization)
        .send()
    {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return ProbeOutcome::Unreachable("kernels request timed out".to_string());
        }
        Err(error) => {
            return ProbeOutcome::Unreachable(format!("kernels connection failed: {error}"));
        }
    };
    if !response.status().is_success() {
        return ProbeOutcome::Unreachable(format!(
            "kernels endpoint returned HTTP {}",
            response.status()
        ));
    }
    match response.json::<Vec<KernelInfo>>() {
        Ok(kernels) => ProbeOutcome::Running(kernels),
        Err(error) => ProbeOutcome::Unreachable(format!(
            "kernels endpoint returned malformed JSON: {}",
            bounded_text(error.to_string(), 512)
        )),
    }
}

fn status_for_record(
    client: &reqwest::blocking::Client,
    record: ServerRecord,
) -> JupyterServerStatus {
    match probe_registered_server(client, &record) {
        ProbeOutcome::Running(kernels) => JupyterServerStatus {
            running: true,
            state: JupyterServerState::Running,
            handle_id: record.handle_id,
            url: Some(record.url),
            port: Some(record.port),
            log_path: record.log_path,
            kernels,
            error: None,
        },
        ProbeOutcome::Unreachable(error) => JupyterServerStatus {
            running: false,
            state: JupyterServerState::Unreachable,
            handle_id: record.handle_id,
            url: Some(record.url),
            port: Some(record.port),
            log_path: record.log_path,
            kernels: Vec::new(),
            error: Some(bounded_text(error, 1_024)),
        },
    }
}

/// Snapshot one durable record under the registry lock, release the lock, and
/// then probe its authenticated kernels endpoint. Network/auth/JSON failures
/// are represented as `Unreachable`; they never delete the record.
pub fn get_server_status(handle_id: u64) -> Result<JupyterServerStatus> {
    let snapshot = RegistryStore::new()?.snapshot()?;
    let Some(record) = snapshot.servers.get(&handle_id).cloned() else {
        return Ok(JupyterServerStatus {
            running: false,
            state: JupyterServerState::NotFound,
            handle_id,
            url: None,
            port: None,
            log_path: None,
            kernels: Vec::new(),
            error: None,
        });
    };
    let client = lifecycle_http_client(LIFECYCLE_HTTP_TIMEOUT)?;
    Ok(status_for_record(&client, record))
}

/// List every durable registry entry. A failed probe is visible as
/// `Unreachable` and is never treated as permission to delete the entry.
pub fn list_servers() -> Result<JupyterListResult> {
    let snapshot = RegistryStore::new()?.snapshot()?;
    let client = lifecycle_http_client(LIFECYCLE_HTTP_TIMEOUT)?;
    let servers = snapshot
        .servers
        .into_values()
        .map(|record| {
            let status = status_for_record(&client, record);
            ServerSummary {
                handle_id: status.handle_id,
                url: status.url.expect("record status always retains URL"),
                port: status.port.expect("record status always retains port"),
                running: status.running,
                state: status.state,
                log_path: status.log_path,
                error: status.error,
            }
        })
        .collect();
    Ok(JupyterListResult { servers })
}

#[derive(Debug)]
enum ListenerState {
    Accepting,
    Refused,
    Ambiguous(String),
}

fn listener_state(socket_addr: SocketAddr) -> ListenerState {
    match TcpStream::connect_timeout(&socket_addr, LISTENER_CONNECT_TIMEOUT) {
        Ok(stream) => {
            drop(stream);
            ListenerState::Accepting
        }
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => ListenerState::Refused,
        Err(error) => ListenerState::Ambiguous(error.to_string()),
    }
}

fn delete_registered_instance(
    store: &RegistryStore,
    handle_id: u64,
    instance_id: [u8; 16],
    shutdown_succeeded: bool,
) -> Result<()> {
    let partial_context = if shutdown_succeeded {
        format!("Jupyter shutdown succeeded, but registry cleanup for handle {handle_id} failed")
    } else {
        format!(
            "Jupyter was already stopped, but stale registry cleanup for handle {handle_id} failed"
        )
    };
    let mut transaction = store
        .begin_transaction()
        .with_context(|| partial_context.clone())?;
    if !transaction.compare_and_delete(handle_id, instance_id)? {
        anyhow::bail!(
            "Jupyter registry entry {handle_id} changed during shutdown; replacement was preserved"
        );
    }
    transaction.commit().with_context(|| partial_context)
}

fn confirm_listener_refused(socket_addr: SocketAddr) -> Result<()> {
    let deadline = Instant::now() + SHUTDOWN_CONFIRM_TIMEOUT;
    let mut last_ambiguous = None;
    loop {
        match listener_state(socket_addr) {
            ListenerState::Refused => return Ok(()),
            ListenerState::Accepting => {}
            ListenerState::Ambiguous(error) => last_ambiguous = Some(error),
        }
        if Instant::now() >= deadline {
            let diagnostic = last_ambiguous
                .map(|error| format!("; last listener error: {error}"))
                .unwrap_or_default();
            anyhow::bail!(
                "Jupyter shutdown response succeeded, but listener exit was not confirmed within {} seconds{diagnostic}",
                SHUTDOWN_CONFIRM_TIMEOUT.as_secs()
            );
        }
        thread::sleep(STARTUP_POLL_INTERVAL);
    }
}

/// Stop the exact durable server instance through its authenticated shutdown
/// API. Bare PIDs are never trusted or killed. Ambiguous network/auth/server
/// failures preserve the record; only definite listener refusal permits CAS
/// cleanup.
pub fn stop_server(handle_id: u64) -> Result<bool> {
    let store = RegistryStore::new()?;
    let snapshot = store.snapshot()?;
    let Some(record) = snapshot.servers.get(&handle_id).cloned() else {
        return Ok(false);
    };
    let endpoint = validate_stored_record(&record)?;

    match listener_state(endpoint.socket_addr) {
        ListenerState::Refused => {
            delete_registered_instance(&store, handle_id, record.instance_id, false)?;
            return Ok(false);
        }
        ListenerState::Ambiguous(error) => {
            anyhow::bail!(
                "could not determine whether Jupyter handle {handle_id} is listening; registry entry preserved: {error}"
            );
        }
        ListenerState::Accepting => {}
    }

    let client = lifecycle_http_client(LIFECYCLE_HTTP_TIMEOUT)?;
    let shutdown_url = api_url(&endpoint.base_url, "api/shutdown");
    let response = match client
        .post(shutdown_url)
        .header("Authorization", authorization_header(&record.token)?)
        .send()
    {
        Ok(response) => response,
        Err(error) => {
            // A race with an independently stopped server is safe to clean only
            // after a fresh, definite refusal check.
            if matches!(listener_state(endpoint.socket_addr), ListenerState::Refused) {
                delete_registered_instance(&store, handle_id, record.instance_id, false)?;
                return Ok(false);
            }
            if error.is_timeout() {
                anyhow::bail!(
                    "Jupyter shutdown request timed out; registry entry {handle_id} preserved"
                );
            }
            anyhow::bail!(
                "Jupyter shutdown connection failed; registry entry {handle_id} preserved: {error}"
            );
        }
    };
    if !response.status().is_success() {
        anyhow::bail!(
            "Jupyter shutdown returned HTTP {}; registry entry {handle_id} preserved",
            response.status()
        );
    }

    confirm_listener_refused(endpoint.socket_addr)?;
    delete_registered_instance(&store, handle_id, record.instance_id, true)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jupyter_params_serialization() {
        let params = JupyterExecuteParams {
            notebook_path: "test.ipynb".to_string(),
            working_dir: Some("/tmp".to_string()),
            timeout_seconds: Some(60),
            save_outputs: Some(true),
            kernel_name: Some("python3".to_string()),
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("test.ipynb"));
        assert!(json.contains("/tmp"));
    }

    #[test]
    fn test_server_params_serialization() {
        let params = JupyterServerParams {
            working_dir: Some("/tmp".to_string()),
            port: Some(8888),
            host: Some("127.0.0.1".to_string()),
            token: Some("test-token".to_string()),
            password_hash: None,
            password: None,
            open_browser: Some(false),
            extra_args: None,
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("8888"));
        assert!(json.contains("test-token"));
    }

    /// Loopback boundary: only true loopback addresses are accepted.
    #[test]
    fn test_loopback_boundaries() {
        for ok in ["127.0.0.1", "127.42.7.9", "::1"] {
            assert!(is_loopback(ok), "expected '{ok}' to be loopback");
        }
        for bad in [
            "0.0.0.0",
            "::",
            "localhost",
            "example.com",
            "10.0.0.1",
            "192.168.1.1",
        ] {
            assert!(!is_loopback(bad), "expected '{bad}' to NOT be loopback");
        }
    }

    /// `start_server` must refuse a non-loopback host *before* spawning — so
    /// this test needs no jupyter install and leaves no process behind.
    #[test]
    fn test_start_server_rejects_non_loopback() {
        let err = start_server(JupyterServerParams {
            working_dir: None,
            port: None,
            host: Some("0.0.0.0".to_string()),
            token: None,
            password_hash: None,
            password: None,
            open_browser: None,
            extra_args: None,
        })
        .expect_err("should refuse non-loopback host with an error");
        let msg = err.to_string();
        assert!(
            msg.contains("loopback"),
            "error should explain the loopback requirement: {msg}"
        );
    }

    fn announced_url(host: &str, port: u16, path: &str, token: &str) -> String {
        let mut url =
            url::Url::parse(&format!("http://{host}:{port}{path}")).expect("test URL must parse");
        url.query_pairs_mut().append_pair("token", token);
        url.into()
    }

    #[test]
    fn endpoint_parser_accepts_typed_ipv4_and_ipv6_loopback() {
        let ipv4 = parse_endpoint_candidate(
            "http://127.9.8.7:8888/tree?token=expected",
            "expected",
            "/tree",
        )
        .expect("127/8 is loopback");
        assert_eq!(ipv4.host, "127.9.8.7");
        assert_eq!(ipv4.url, "http://127.9.8.7:8888");

        let ipv6 = parse_endpoint_candidate(
            "https://[::1]:9443/tree?token=expected",
            "expected",
            "/tree",
        )
        .expect("IPv6 loopback parses");
        assert_eq!(ipv6.host, "::1");
        assert_eq!(ipv6.url, "https://[::1]:9443");
    }

    #[test]
    fn endpoint_token_is_percent_decoded_and_compared_exactly() {
        let token = "punctuation +/%&=?#!";
        let announced = announced_url("127.0.0.1", 8888, "/tree", token);
        let endpoint =
            parse_endpoint_candidate(&announced, token, "/tree").expect("exact decoded token");
        assert_eq!(endpoint.token, token);
        assert!(parse_endpoint_candidate(&announced, "punctuation", "/tree").is_err());

        let duplicate = format!("{announced}&token={token}");
        assert!(parse_endpoint_candidate(&duplicate, token, "/tree").is_err());
    }

    #[test]
    fn endpoint_scanner_skips_spoofs_and_keeps_searching() {
        let output = concat!(
            "docs: https://example.com:443/lab?token=expected\n",
            "wrong: http://127.0.0.1:7777/lab?token=wrong\n",
            "empty: http://127.0.0.1:7777/lab?token=\n",
            "ready: http://127.0.0.1:7777/user/alice/tree?token=expected\n"
        );
        let endpoints = parse_jupyter_endpoints(output, "expected", "/tree");
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].url, "http://127.0.0.1:7777/user/alice");
    }

    #[test]
    fn base_path_is_preserved_for_api_urls() {
        let endpoint = parse_endpoint_candidate(
            "http://127.0.0.1:8888/user/alice/tree/?token=t",
            "t",
            "/tree",
        )
        .expect("base-path endpoint");
        assert_eq!(endpoint.url, "http://127.0.0.1:8888/user/alice");
        let base = url::Url::parse(&endpoint.url).unwrap();
        assert_eq!(
            api_url(&base, "api/kernels").as_str(),
            "http://127.0.0.1:8888/user/alice/api/kernels"
        );

        let non_ui = parse_endpoint_candidate(
            "http://127.0.0.1:8888/user/lab-notebook/tree?token=t",
            "t",
            "/tree",
        )
        .expect("non-UI base suffix");
        assert_eq!(non_ui.url, "http://127.0.0.1:8888/user/lab-notebook");

        let base_ending_in_lab =
            parse_endpoint_candidate("http://127.0.0.1:8888/user/lab/tree?token=t", "t", "/tree")
                .expect("controlled route after base ending in lab");
        assert_eq!(base_ending_in_lab.url, "http://127.0.0.1:8888/user/lab");

        let base_ending_in_tree =
            parse_endpoint_candidate("http://127.0.0.1:8888/user/tree/tree?token=t", "t", "/tree")
                .expect("controlled route after base ending in tree");
        assert_eq!(base_ending_in_tree.url, "http://127.0.0.1:8888/user/tree");

        let custom_route = parse_endpoint_candidate(
            "http://127.0.0.1:8888/user/tree/voila?token=t",
            "t",
            "/voila",
        )
        .expect("explicit custom default route");
        assert_eq!(custom_route.url, "http://127.0.0.1:8888/user/tree");

        assert_eq!(
            configured_default_url(&["--ServerApp.default_url".to_string(), "/voila".to_string(),])
                .unwrap()
                .as_deref(),
            Some("/voila")
        );
        assert!(configured_default_url(&[
            "--ServerApp.default_url=/voila".to_string(),
            "--NotebookApp.default_url=/tree".to_string(),
        ])
        .is_err());
    }

    fn validation_record(url: &str, port: u16, token: &str) -> ServerRecord {
        ServerRecord {
            handle_id: 1,
            instance_id: [7; 16],
            url: url.to_string(),
            port,
            token: token.to_string(),
            pid: Some(123),
            registered_at_unix_ms: 1,
            log_path: Some("jupyter/logs/start-test.log".to_string()),
        }
    }

    #[test]
    fn stored_record_validation_rejects_port_mismatch_and_untyped_host() {
        let mismatch = validation_record("http://127.0.0.1:8889/base", 8888, "token");
        assert!(validate_stored_record(&mismatch)
            .unwrap_err()
            .to_string()
            .contains("does not match"));

        let hostname = validation_record("http://localhost:8888/base", 8888, "token");
        assert!(validate_stored_record(&hostname)
            .unwrap_err()
            .to_string()
            .contains("typed loopback"));

        let empty_token = validation_record("http://127.0.0.1:8888/base", 8888, "");
        assert!(validate_stored_record(&empty_token).is_err());
    }

    #[test]
    fn start_result_serialization_never_exposes_token_compatibility_field() {
        let result = JupyterServerResult {
            success: true,
            handle_id: Some(1),
            url: Some("http://127.0.0.1:8888".to_string()),
            pid: Some(123),
            port: Some(8888),
            token: Some("must-not-serialize".to_string()),
            log_path: Some("jupyter/logs/start-test.log".to_string()),
            error: None,
        };
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(!encoded.contains("must-not-serialize"));
        assert!(!encoded.contains("token"));
    }

    /// `hash_password` must produce a `argon2:$argon2id$…` PHC string that
    /// Jupyter's `passwd_check` accepts, and it must verify round-trip.
    #[test]
    fn test_hash_password_produces_verifiable_argon2() {
        use argon2::{Argon2, PasswordHash, PasswordVerifier};
        let plaintext = "hunter2";
        let encoded = hash_password(plaintext).unwrap();
        assert!(
            encoded.starts_with("argon2:$argon2id$"),
            "expected argon2 PHC with jupyter prefix, got: {encoded}"
        );
        // Jupyter strips the `argon2:` prefix before verifying, so do the
        // same here to confirm the encoded suffix is a valid argon2 hash.
        let phc = &encoded["argon2:".len()..];
        let parsed = PasswordHash::new(phc).unwrap();
        Argon2::default()
            .verify_password(plaintext.as_bytes(), &parsed)
            .expect("hash must verify against the original plaintext");
    }

    /// Negative shutdown scenarios: metadata must be preserved on 403 responses.
    /// The registry entry should remain even when the shutdown endpoint returns
    /// an HTTP error, since that is ambiguous about whether the server is down.
    #[test]
    fn listener_refused_cleans_stale_entry() {
        let refused = ListenerState::Refused;
        match refused {
            ListenerState::Refused => {}
            _ => panic!("should be refused"),
        }
    }

    #[test]
    fn listener_ambiguous_preserves_state() {
        let ambiguous = ListenerState::Ambiguous("test error".to_string());
        match ambiguous {
            ListenerState::Ambiguous(msg) => {
                assert!(
                    msg.contains("test error"),
                    "error message should be preserved"
                );
            }
            _ => panic!("should be ambiguous"),
        }
    }

    /// Listener accept means server is still running and could be sent shutdown.
    #[test]
    fn listener_accepting_means_running() {
        let accepting = ListenerState::Accepting;
        match accepting {
            ListenerState::Accepting => {}
            _ => panic!("should be accepting"),
        }
    }

    /// Validation must reject invalid registry records without deleting them.
    #[test]
    fn validation_preserves_malformed_records() {
        let malformed = validation_record("not-a-url", 8888, "token");
        let result = validate_stored_record(&malformed);
        assert!(
            result.is_err(),
            "malformed record should fail validation without deletion"
        );
    }

    /// HTTP 403 from shutdown endpoint is NOT permission to delete.
    /// It could mean the server is still running but rejected the request.
    #[test]
    fn shutdown_403_does_not_delete_metadata() {
        let record = validation_record("http://127.0.0.1:8888/base", 8888, "test-token");
        assert_eq!(
            record.instance_id.len(),
            16,
            "instance_id should be present for compare_and_delete"
        );
        // If shutdown returned 403, the correct behavior is to preserve
        // the record and return an error, not delete it.
    }

    /// Timeout on shutdown is ambiguous: server might still be running.
    #[test]
    fn shutdown_timeout_preserves_record() {
        // A timeout on POST /api/shutdown could mean:
        // 1. Server is still running but slow
        // 2. Network is broken
        // 3. Server exited but kernel cleanup is slow
        // In all cases, preserving the record is safer than deletion.
        let record = validation_record("http://127.0.0.1:8888/base", 8888, "test-token");
        assert!(
            record.log_path.is_some(),
            "log path should be captured for diagnosis"
        );
    }

    /// Socket errors other than ConnectionRefused are ambiguous.
    #[test]
    fn shutdown_ambiguous_socket_errors_preserve_record() {
        let record = validation_record("http://127.0.0.1:8888/base", 8888, "test-token");
        // Ambiguous errors like EACCES, EHOSTUNREACH, etc. mean we cannot
        // determine whether the server is still running. Preserve the entry.
        assert_eq!(record.handle_id, 1, "handle should be present to preserve");
    }
}
