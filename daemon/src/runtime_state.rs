use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::header::{HeaderName, HeaderValue};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

const KNOWN_PROVIDERS: [&str; 5] = ["claude", "codex", "cursor", "opencode-go", "muse"];
const RECOVERY_COOLDOWN_SECONDS: i64 = 300;
const MAX_MANAGED_FILE_BYTES: u64 = 64 * 1024;
const CONTROL_TOKEN_BYTES: usize = 32;
const CONTROL_TOKEN_HEX_LENGTH: usize = CONTROL_TOKEN_BYTES * 2;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A daemon's process-owned view of the local configuration and cooldown files.
/// The instance lock is held until this value is dropped and is never unlinked.
pub struct RuntimeState {
    home: PathBuf,
    quotabar_dir: PathBuf,
    state_dir: PathBuf,
    _instance_lock: File,
}

/// Opaque authentication material loaded from the daemon-owned private file.
/// It intentionally has no `Debug`, serialization, or string conversion API.
pub struct ControlToken([u8; CONTROL_TOKEN_HEX_LENGTH]);

impl ControlToken {
    /// Compare a presented HTTP header value without exposing the stored token.
    pub fn matches(&self, candidate: &[u8]) -> bool {
        constant_time_eq(&self.0, candidate)
    }

    /// Insert the token only as the existing sensitive control header.
    pub fn insert_control_header(
        &self,
        headers: &mut axum::http::HeaderMap,
    ) -> Result<(), axum::http::header::InvalidHeaderValue> {
        let mut value = HeaderValue::from_bytes(&self.0)?;
        value.set_sensitive(true);
        headers.insert(HeaderName::from_static("x-quotabar-control-token"), value);
        Ok(())
    }
}

/// Read the daemon's existing control token without opening RuntimeState,
/// creating directories, taking its instance lock, or changing permissions.
pub fn load_control_token(home: &Path) -> Result<ControlToken, RuntimeStateError> {
    validate_directory(home)?;
    let quotabar_dir = home.join(".quotabar");
    validate_directory(&quotabar_dir)?;
    let state_dir = quotabar_dir.join("daemon");
    validate_directory(&state_dir)?;
    let path = state_dir.join("control-token");
    read_control_token_file(&path).map(ControlToken)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonConfig {
    pub disabled_providers: Vec<String>,
}

/// The atomic rename committed, but a directory-sync failure leaves durability
/// unconfirmed rather than leaving the write uncommitted.
#[derive(Debug)]
pub enum DurabilityOutcome {
    Durable,
    CommittedButDurabilityUnconfirmed(io::Error),
}

#[derive(Debug)]
pub struct ConfigUpdate {
    pub config: DaemonConfig,
    pub durability: DurabilityOutcome,
}

/// A missing or expired deadline means the provider is ready. `recovered` is
/// true only after a safe warning and the replacement deadline were persisted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CooldownState {
    pub not_before: Option<DateTime<Utc>>,
    pub recovered: bool,
}

#[derive(Debug)]
pub enum RuntimeStateError {
    AlreadyRunning,
    InvalidProvider,
    InvalidConfiguration,
    UnsafeManagedPath,
    InvalidTime,
    InvalidControlToken,
    FileTooLarge,
    Io(io::Error),
    Persistence(io::Error),
}

impl fmt::Display for RuntimeStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => {
                formatter.write_str("another quotabard process holds the instance lock")
            }
            Self::InvalidProvider => formatter.write_str("unknown provider ID"),
            Self::InvalidConfiguration => formatter.write_str("daemon configuration is invalid"),
            Self::UnsafeManagedPath => {
                formatter.write_str("managed runtime state is not a regular file or directory")
            }
            Self::InvalidTime => {
                formatter.write_str("runtime-state deadline is outside the supported time range")
            }
            Self::InvalidControlToken => {
                formatter.write_str("daemon control-token file is invalid")
            }
            Self::FileTooLarge => {
                formatter.write_str("managed runtime-state file exceeds its size limit")
            }
            Self::Io(_) => formatter.write_str("runtime-state filesystem operation failed"),
            Self::Persistence(_) => formatter.write_str("runtime-state persistence failed"),
        }
    }
}

impl Error for RuntimeStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) | Self::Persistence(error) => Some(error),
            _ => None,
        }
    }
}

impl RuntimeState {
    /// Open the state rooted at an explicit home directory. This does not read
    /// process-global HOME or invoke any credential, network, or service API.
    pub fn open(home: &Path) -> Result<Self, RuntimeStateError> {
        validate_directory(home)?;
        let quotabar_dir = home.join(".quotabar");
        ensure_private_directory(&quotabar_dir)?;
        let state_dir = quotabar_dir.join("daemon");
        ensure_private_directory(&state_dir)?;

        let lock_path = state_dir.join("instance.lock");
        let instance_lock = open_lock_file(&lock_path)?;
        match instance_lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(RuntimeStateError::AlreadyRunning);
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(RuntimeStateError::Io(error)),
        }

        Ok(Self {
            home: home.to_path_buf(),
            quotabar_dir,
            state_dir,
            _instance_lock: instance_lock,
        })
    }

    /// Load the existing control token or create one from the OS random source.
    /// Malformed and unsafe existing files fail closed and are never replaced.
    pub fn load_or_create_control_token(&self) -> Result<ControlToken, RuntimeStateError> {
        self.validate_directories()?;
        let path = self.state_dir.join("control-token");
        if let Some(bytes) = read_managed_file(&path, CONTROL_TOKEN_HEX_LENGTH as u64 + 1)? {
            return parse_control_token(&bytes)
                .map(ControlToken)
                .ok_or(RuntimeStateError::InvalidControlToken);
        }

        let mut random = [0_u8; CONTROL_TOKEN_BYTES];
        getrandom::fill(&mut random)
            .map_err(|_| RuntimeStateError::Io(io::Error::other("OS random source unavailable")))?;
        let token = encode_control_token(&random);
        let mut contents = Vec::with_capacity(CONTROL_TOKEN_HEX_LENGTH + 1);
        contents.extend_from_slice(&token);
        contents.push(b'\n');
        match self.atomic_write_with(
            &path,
            &contents,
            |file, bytes| file.write_all(bytes),
            |directory| directory.sync_all(),
        )? {
            DurabilityOutcome::Durable => Ok(ControlToken(token)),
            DurabilityOutcome::CommittedButDurabilityUnconfirmed(error) => {
                Err(RuntimeStateError::Persistence(error))
            }
        }
    }

    /// Reload and validate daemon.json every time. Missing configuration is the
    /// default; invalid configuration is never silently replaced.
    pub fn load_config(&self) -> Result<DaemonConfig, RuntimeStateError> {
        self.validate_directories()?;
        let Some(bytes) =
            read_managed_file(&self.config_path(), MAX_MANAGED_FILE_BYTES).map_err(|error| {
                match error {
                    RuntimeStateError::FileTooLarge => RuntimeStateError::InvalidConfiguration,
                    other => other,
                }
            })?
        else {
            return Ok(DaemonConfig {
                disabled_providers: Vec::new(),
            });
        };
        parse_config(&bytes)
    }

    /// Reload before changing one provider, then persist the complete setting
    /// atomically. A returned update is committed and must be applied even when
    /// its directory durability is unconfirmed.
    pub fn set_enabled(
        &mut self,
        id: &str,
        enabled: bool,
    ) -> Result<ConfigUpdate, RuntimeStateError> {
        self.set_enabled_with_durability(id, enabled, |directory| directory.sync_all())
    }

    fn set_enabled_with_durability<F>(
        &mut self,
        id: &str,
        enabled: bool,
        directory_sync: F,
    ) -> Result<ConfigUpdate, RuntimeStateError>
    where
        F: FnOnce(&File) -> io::Result<()>,
    {
        let provider_id = validate_provider_id(id)?;
        let mut config = self.load_config()?;
        if enabled {
            config
                .disabled_providers
                .retain(|disabled| disabled != provider_id);
        } else if !config
            .disabled_providers
            .iter()
            .any(|disabled| disabled == provider_id)
        {
            config.disabled_providers.push(provider_id.to_owned());
        }
        config.disabled_providers.sort_unstable();

        let bytes = serde_json::to_vec(&ConfigFileRef {
            disabled_providers: &config.disabled_providers,
        })
        .map_err(|_| RuntimeStateError::InvalidConfiguration)?;
        let durability = self.atomic_write_with(
            &self.config_path(),
            &bytes,
            |file, bytes| file.write_all(bytes),
            directory_sync,
        )?;
        if matches!(
            &durability,
            DurabilityOutcome::CommittedButDurabilityUnconfirmed(_)
        ) {
            eprintln!(
                "warning: daemon configuration committed; directory durability is unconfirmed"
            );
        }
        Ok(ConfigUpdate { config, durability })
    }

    /// Read a provider's saved deadline. A corrupt or unreadable regular file
    /// is replaced with one persisted five-minute recovery guard before return.
    /// The returned `recovered` flag is safe to log with the validated ID only.
    pub fn load_cooldown(
        &mut self,
        id: &str,
        now: DateTime<Utc>,
    ) -> Result<CooldownState, RuntimeStateError> {
        let provider_id = validate_provider_id(id)?;
        self.load_cooldown_with(provider_id, now, |deadline| {
            self.write_cooldown(provider_id, deadline)
        })
    }

    /// Persist the later of a candidate deadline and any existing future guard.
    /// Persistence errors are returned so callers cannot mistake a blocked
    /// provider for one that is ready to run.
    pub fn extend_cooldown(
        &mut self,
        id: &str,
        candidate: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, RuntimeStateError> {
        self.extend_cooldown_with_durability(id, candidate, now, |directory| directory.sync_all())
    }

    fn extend_cooldown_with_durability<F>(
        &mut self,
        id: &str,
        candidate: DateTime<Utc>,
        now: DateTime<Utc>,
        directory_sync: F,
    ) -> Result<DateTime<Utc>, RuntimeStateError>
    where
        F: FnOnce(&File) -> io::Result<()>,
    {
        let provider_id = validate_provider_id(id)?;
        let current = self.load_cooldown(provider_id, now)?;
        let deadline = current
            .not_before
            .filter(|not_before| not_before > &now)
            .map_or(candidate, |not_before| not_before.max(candidate));
        self.write_cooldown_with_durability(provider_id, deadline, directory_sync)?;
        Ok(deadline)
    }

    fn config_path(&self) -> PathBuf {
        self.quotabar_dir.join("daemon.json")
    }

    fn cooldown_path(&self, provider_id: &'static str) -> PathBuf {
        self.state_dir.join(format!("{provider_id}.json"))
    }

    fn validate_directories(&self) -> Result<(), RuntimeStateError> {
        validate_directory(&self.home)?;
        validate_directory(&self.quotabar_dir)?;
        validate_directory(&self.state_dir)
    }

    fn write_cooldown(
        &self,
        provider_id: &'static str,
        not_before: DateTime<Utc>,
    ) -> Result<(), RuntimeStateError> {
        self.write_cooldown_with_durability(provider_id, not_before, |directory| {
            directory.sync_all()
        })
    }

    fn write_cooldown_with_durability<F>(
        &self,
        provider_id: &'static str,
        not_before: DateTime<Utc>,
        directory_sync: F,
    ) -> Result<(), RuntimeStateError>
    where
        F: FnOnce(&File) -> io::Result<()>,
    {
        let record = CooldownFile {
            provider_id: provider_id.to_owned(),
            not_before: format_deadline(not_before)?,
        };
        let bytes =
            serde_json::to_vec(&record).map_err(|_| RuntimeStateError::InvalidConfiguration)?;
        match self.atomic_write_with(
            &self.cooldown_path(provider_id),
            &bytes,
            |file, bytes| file.write_all(bytes),
            directory_sync,
        )? {
            DurabilityOutcome::Durable => Ok(()),
            DurabilityOutcome::CommittedButDurabilityUnconfirmed(error) => {
                Err(RuntimeStateError::Persistence(error))
            }
        }
    }

    fn load_cooldown_with<F>(
        &self,
        provider_id: &'static str,
        now: DateTime<Utc>,
        mut persist_recovery: F,
    ) -> Result<CooldownState, RuntimeStateError>
    where
        F: FnMut(DateTime<Utc>) -> Result<(), RuntimeStateError>,
    {
        self.validate_directories()?;
        let path = self.cooldown_path(provider_id);
        let contents = match read_managed_file(&path, MAX_MANAGED_FILE_BYTES) {
            Ok(None) => {
                return Ok(CooldownState {
                    not_before: None,
                    recovered: false,
                });
            }
            Ok(Some(bytes)) => Some(bytes),
            Err(error @ RuntimeStateError::UnsafeManagedPath) => return Err(error),
            Err(_) => None,
        };

        let parsed = contents.and_then(|bytes| parse_cooldown(&bytes, provider_id));
        if let Some(not_before) = parsed {
            return Ok(CooldownState {
                not_before: (not_before > now).then_some(not_before),
                recovered: false,
            });
        }

        let deadline = now
            .checked_add_signed(Duration::seconds(RECOVERY_COOLDOWN_SECONDS))
            .ok_or(RuntimeStateError::InvalidTime)?;
        persist_recovery(deadline)?;
        eprintln!(
            "warning: recovered invalid cooldown state for provider {provider_id}; enforcing a 300-second guard"
        );
        Ok(CooldownState {
            not_before: Some(deadline),
            recovered: true,
        })
    }

    /// The writer and directory-sync seams let tests cover both sides of the
    /// rename commit point without replacing filesystem behavior wholesale.
    fn atomic_write_with<F, D>(
        &self,
        path: &Path,
        bytes: &[u8],
        writer: F,
        directory_sync: D,
    ) -> Result<DurabilityOutcome, RuntimeStateError>
    where
        F: FnOnce(&mut File, &[u8]) -> io::Result<()>,
        D: FnOnce(&File) -> io::Result<()>,
    {
        self.validate_directories()?;
        validate_optional_regular_file(path)?;
        let parent = path.parent().ok_or(RuntimeStateError::UnsafeManagedPath)?;
        let parent_directory = File::open(parent).map_err(RuntimeStateError::Persistence)?;
        let (temporary_path, mut temporary_file) = create_temporary_file(parent, path)?;
        let _cleanup = TemporaryFileCleanup(temporary_path.clone());

        writer(&mut temporary_file, bytes).map_err(RuntimeStateError::Persistence)?;
        temporary_file
            .sync_all()
            .map_err(RuntimeStateError::Persistence)?;
        drop(temporary_file);

        validate_optional_regular_file(path)?;
        fs::rename(&temporary_path, path).map_err(RuntimeStateError::Persistence)?;
        Ok(match directory_sync(&parent_directory) {
            Ok(()) => DurabilityOutcome::Durable,
            Err(error) => DurabilityOutcome::CommittedButDurabilityUnconfirmed(error),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigFile {
    disabled_providers: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigFileRef<'a> {
    disabled_providers: &'a [String],
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CooldownFile {
    provider_id: String,
    not_before: String,
}

fn format_deadline(value: DateTime<Utc>) -> Result<String, RuntimeStateError> {
    let formatted = value.to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let round_trip = DateTime::parse_from_rfc3339(&formatted)
        .map_err(|_| RuntimeStateError::InvalidTime)?
        .with_timezone(&Utc);
    if round_trip != value {
        return Err(RuntimeStateError::InvalidTime);
    }
    Ok(formatted)
}

fn parse_control_token(bytes: &[u8]) -> Option<[u8; CONTROL_TOKEN_HEX_LENGTH]> {
    if bytes.len() != CONTROL_TOKEN_HEX_LENGTH + 1 || bytes[CONTROL_TOKEN_HEX_LENGTH] != b'\n' {
        return None;
    }
    let mut token = [0_u8; CONTROL_TOKEN_HEX_LENGTH];
    for (index, byte) in bytes[..CONTROL_TOKEN_HEX_LENGTH]
        .iter()
        .copied()
        .enumerate()
    {
        if !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte) {
            return None;
        }
        token[index] = byte;
    }
    Some(token)
}

fn read_control_token_file(
    path: &Path,
) -> Result<[u8; CONTROL_TOKEN_HEX_LENGTH], RuntimeStateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(RuntimeStateError::UnsafeManagedPath),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(RuntimeStateError::InvalidControlToken);
        }
        Err(error) => return Err(RuntimeStateError::Io(error)),
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        let is_symlink = error.raw_os_error() == Some(libc::ELOOP);
        #[cfg(not(unix))]
        let is_symlink = error.kind() == io::ErrorKind::FilesystemLoop;
        if is_symlink {
            RuntimeStateError::UnsafeManagedPath
        } else {
            RuntimeStateError::Io(error)
        }
    })?;
    let metadata = file.metadata().map_err(RuntimeStateError::Io)?;
    if !metadata.file_type().is_file() {
        return Err(RuntimeStateError::UnsafeManagedPath);
    }
    validate_readonly_token_mode(&metadata)?;

    let maximum = (CONTROL_TOKEN_HEX_LENGTH + 1) as u64;
    if metadata.len() > maximum {
        return Err(RuntimeStateError::FileTooLarge);
    }
    let mut contents = Vec::with_capacity(maximum as usize + 1);
    file.take(maximum + 1)
        .read_to_end(&mut contents)
        .map_err(RuntimeStateError::Io)?;
    if contents.len() as u64 > maximum {
        return Err(RuntimeStateError::FileTooLarge);
    }
    parse_control_token(&contents).ok_or(RuntimeStateError::InvalidControlToken)
}

#[cfg(unix)]
fn validate_readonly_token_mode(metadata: &fs::Metadata) -> Result<(), RuntimeStateError> {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode() & 0o7777;
    if mode & !0o600 != 0 || mode & 0o400 == 0 {
        return Err(RuntimeStateError::InvalidControlToken);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_readonly_token_mode(_metadata: &fs::Metadata) -> Result<(), RuntimeStateError> {
    Ok(())
}

fn encode_control_token(random: &[u8; CONTROL_TOKEN_BYTES]) -> [u8; CONTROL_TOKEN_HEX_LENGTH] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut token = [0_u8; CONTROL_TOKEN_HEX_LENGTH];
    for (index, byte) in random.iter().copied().enumerate() {
        token[index * 2] = HEX[(byte >> 4) as usize];
        token[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    token
}

fn constant_time_eq(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected
        .iter()
        .zip(actual)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn parse_config(bytes: &[u8]) -> Result<DaemonConfig, RuntimeStateError> {
    let file: ConfigFile =
        serde_json::from_slice(bytes).map_err(|_| RuntimeStateError::InvalidConfiguration)?;
    let mut disabled_providers: Vec<String> = Vec::with_capacity(file.disabled_providers.len());
    for id in file.disabled_providers {
        let provider_id =
            validate_provider_id(&id).map_err(|_| RuntimeStateError::InvalidConfiguration)?;
        if disabled_providers
            .iter()
            .any(|existing| existing.as_str() == provider_id)
        {
            return Err(RuntimeStateError::InvalidConfiguration);
        }
        disabled_providers.push(provider_id.to_owned());
    }
    disabled_providers.sort_unstable();
    Ok(DaemonConfig { disabled_providers })
}

fn parse_cooldown(bytes: &[u8], expected_provider: &'static str) -> Option<DateTime<Utc>> {
    let file: CooldownFile = serde_json::from_slice(bytes).ok()?;
    if file.provider_id != expected_provider {
        return None;
    }
    DateTime::parse_from_rfc3339(&file.not_before)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn validate_provider_id(id: &str) -> Result<&'static str, RuntimeStateError> {
    KNOWN_PROVIDERS
        .iter()
        .copied()
        .find(|known| *known == id)
        .ok_or(RuntimeStateError::InvalidProvider)
}

fn read_managed_file(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>, RuntimeStateError> {
    read_managed_file_inner(path, max_bytes, || {})
}

fn read_managed_file_inner<F>(
    path: &Path,
    max_bytes: u64,
    after_regular_check: F,
) -> Result<Option<Vec<u8>>, RuntimeStateError>
where
    F: FnOnce(),
{
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(RuntimeStateError::UnsafeManagedPath),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(RuntimeStateError::Io(error)),
    }

    after_regular_check();
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        let is_symlink = error.raw_os_error() == Some(libc::ELOOP);
        #[cfg(not(unix))]
        let is_symlink = error.kind() == io::ErrorKind::FilesystemLoop;
        if is_symlink {
            RuntimeStateError::UnsafeManagedPath
        } else {
            RuntimeStateError::Io(error)
        }
    })?;
    let metadata = file.metadata().map_err(RuntimeStateError::Io)?;
    if !metadata.file_type().is_file() {
        return Err(RuntimeStateError::UnsafeManagedPath);
    }
    set_private_file_permissions(&file).map_err(RuntimeStateError::Io)?;

    let mut contents = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut contents)
        .map_err(RuntimeStateError::Io)?;
    if contents.len() as u64 > max_bytes {
        return Err(RuntimeStateError::FileTooLarge);
    }
    Ok(Some(contents))
}

#[cfg(test)]
fn read_managed_file_with_hook<F>(
    path: &Path,
    max_bytes: u64,
    after_regular_check: F,
) -> Result<Option<Vec<u8>>, RuntimeStateError>
where
    F: FnOnce(),
{
    read_managed_file_inner(path, max_bytes, after_regular_check)
}

fn validate_directory(path: &Path) -> Result<(), RuntimeStateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(RuntimeStateError::UnsafeManagedPath),
        Err(error) => Err(RuntimeStateError::Io(error)),
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), RuntimeStateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(RuntimeStateError::UnsafeManagedPath),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            set_directory_creation_mode(&mut builder);
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(RuntimeStateError::Io(error)),
            }
            validate_directory(path)?;
        }
        Err(error) => return Err(RuntimeStateError::Io(error)),
    }
    set_private_directory_permissions(path).map_err(RuntimeStateError::Io)
}

fn open_lock_file(path: &Path) -> Result<File, RuntimeStateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(RuntimeStateError::UnsafeManagedPath),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            set_file_creation_mode(&mut options);
            match options.open(path) {
                Ok(file) => {
                    if !file
                        .metadata()
                        .map_err(RuntimeStateError::Io)?
                        .file_type()
                        .is_file()
                    {
                        return Err(RuntimeStateError::UnsafeManagedPath);
                    }
                    set_private_file_permissions(&file).map_err(RuntimeStateError::Io)?;
                    return Ok(file);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(RuntimeStateError::Io(error)),
            }
        }
        Err(error) => return Err(RuntimeStateError::Io(error)),
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true);
    let file = options.open(path).map_err(RuntimeStateError::Io)?;
    if !file
        .metadata()
        .map_err(RuntimeStateError::Io)?
        .file_type()
        .is_file()
    {
        return Err(RuntimeStateError::UnsafeManagedPath);
    }
    set_private_file_permissions(&file).map_err(RuntimeStateError::Io)?;
    Ok(file)
}

fn validate_optional_regular_file(path: &Path) -> Result<(), RuntimeStateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(RuntimeStateError::UnsafeManagedPath),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeStateError::Io(error)),
    }
}

fn create_temporary_file(
    parent: &Path,
    target: &Path,
) -> Result<(PathBuf, File), RuntimeStateError> {
    let file_name = target
        .file_name()
        .ok_or(RuntimeStateError::UnsafeManagedPath)?
        .to_string_lossy();

    for _ in 0..128 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(".{file_name}.{}.{}.tmp", std::process::id(), sequence);
        let temporary_path = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_file_creation_mode(&mut options);
        match options.open(&temporary_path) {
            Ok(file) => {
                if let Err(error) = set_private_file_permissions(&file) {
                    drop(file);
                    let _ = fs::remove_file(&temporary_path);
                    return Err(RuntimeStateError::Persistence(error));
                }
                return Ok((temporary_path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(RuntimeStateError::Persistence(error)),
        }
    }
    Err(RuntimeStateError::Persistence(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    )))
}

struct TemporaryFileCleanup(PathBuf);

impl Drop for TemporaryFileCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn set_directory_creation_mode(builder: &mut fs::DirBuilder) {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
}

#[cfg(not(unix))]
fn set_directory_creation_mode(_builder: &mut fs::DirBuilder) {}

#[cfg(unix)]
fn set_file_creation_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_file_creation_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CONTROL_TOKEN_HEX_LENGTH, ControlToken, CooldownState, DurabilityOutcome, RuntimeState,
        RuntimeStateError, constant_time_eq, load_control_token, parse_config, parse_control_token,
        read_managed_file_with_hook, validate_provider_id,
    };
    use chrono::{DateTime, Duration, Utc};
    use std::fs;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const PROVIDERS: [&str; 5] = ["claude", "codex", "cursor", "opencode-go", "muse"];

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quotabard-runtime-state-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn new_short() -> Self {
            let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("q{}{sequence}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(seconds, 0).unwrap()
    }

    fn config_path(home: &TestHome) -> PathBuf {
        home.path().join(".quotabar").join("daemon.json")
    }

    fn cooldown_path(home: &TestHome, id: &str) -> PathBuf {
        home.path()
            .join(".quotabar")
            .join("daemon")
            .join(format!("{id}.json"))
    }

    fn assert_persistence_error<T>(result: Result<T, RuntimeStateError>) {
        assert!(matches!(result, Err(RuntimeStateError::Persistence(_))));
    }

    #[test]
    fn control_token_is_created_once_with_private_mode_and_strict_format() {
        use std::os::unix::fs::PermissionsExt;

        let home = TestHome::new();
        let state = RuntimeState::open(home.path()).unwrap();
        let first = state.load_or_create_control_token().unwrap();
        let path = home.path().join(".quotabar/daemon/control-token");
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), CONTROL_TOKEN_HEX_LENGTH + 1);
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(parse_control_token(&bytes).is_some());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let second = state.load_or_create_control_token().unwrap();
        assert!(first.matches(&bytes[..CONTROL_TOKEN_HEX_LENGTH]));
        assert!(second.matches(&bytes[..CONTROL_TOKEN_HEX_LENGTH]));
    }

    #[test]
    fn malformed_token_fails_closed_without_replacement() {
        let home = TestHome::new();
        let state = RuntimeState::open(home.path()).unwrap();
        let path = home.path().join(".quotabar/daemon/control-token");
        let malformed = b"NOT-A-TOKEN\n";
        fs::write(&path, malformed).unwrap();

        assert!(matches!(
            state.load_or_create_control_token(),
            Err(RuntimeStateError::InvalidControlToken)
        ));
        assert_eq!(fs::read(path).unwrap(), malformed);
    }

    #[cfg(unix)]
    #[test]
    fn standalone_token_loader_reads_while_instance_lock_is_held_without_mutating_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let home = TestHome::new();
        let state = RuntimeState::open(home.path()).unwrap();
        let path = home.path().join(".quotabar/daemon/control-token");
        let token = [b'a'; CONTROL_TOKEN_HEX_LENGTH];
        let mut contents = token.to_vec();
        contents.push(b'\n');
        fs::write(&path, &contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        let before_mode = fs::metadata(&path).unwrap().mode();
        let before_bytes = fs::read(&path).unwrap();

        let loaded = load_control_token(home.path()).unwrap();
        assert!(loaded.matches(&token));
        assert_eq!(fs::metadata(&path).unwrap().mode(), before_mode);
        assert_eq!(fs::read(&path).unwrap(), before_bytes);
        assert!(matches!(
            RuntimeState::open(home.path()),
            Err(RuntimeStateError::AlreadyRunning)
        ));
        drop(state);
    }

    #[cfg(unix)]
    #[test]
    fn standalone_token_loader_does_not_create_state_directories_or_token() {
        let home = TestHome::new();
        assert!(matches!(
            load_control_token(home.path()),
            Err(RuntimeStateError::Io(_))
        ));
        assert!(!home.path().join(".quotabar").exists());
    }

    #[cfg(unix)]
    #[test]
    fn standalone_token_loader_rejects_malformed_and_unsafe_modes_without_rewriting() {
        use std::os::unix::fs::PermissionsExt;

        let home = TestHome::new();
        let _state = RuntimeState::open(home.path()).unwrap();
        let path = home.path().join(".quotabar/daemon/control-token");
        for malformed in [
            vec![b'A'; CONTROL_TOKEN_HEX_LENGTH]
                .into_iter()
                .chain(*b"\n")
                .collect::<Vec<_>>(),
            vec![b'a'; CONTROL_TOKEN_HEX_LENGTH],
            vec![b'a'; CONTROL_TOKEN_HEX_LENGTH - 1]
                .into_iter()
                .chain(*b"\n")
                .collect::<Vec<_>>(),
            vec![b'a'; CONTROL_TOKEN_HEX_LENGTH + 1]
                .into_iter()
                .chain(*b"\n")
                .collect::<Vec<_>>(),
        ] {
            fs::write(&path, &malformed).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let result = load_control_token(home.path());
            assert!(result.is_err());
            assert_eq!(fs::read(&path).unwrap(), malformed);
        }

        for mode in [0o640, 0o644, 0o700, 0o200] {
            let valid = [b'b'; CONTROL_TOKEN_HEX_LENGTH];
            let mut contents = valid.to_vec();
            contents.push(b'\n');
            fs::write(&path, &contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(load_control_token(home.path()).is_err(), "mode {mode:o}");
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                mode
            );
            if mode & 0o400 == 0 {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            assert_eq!(fs::read(&path).unwrap(), contents);
        }
    }

    #[cfg(unix)]
    #[test]
    fn standalone_token_loader_rejects_nonregular_paths_and_symlinks() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let home = TestHome::new_short();
        let _state = RuntimeState::open(home.path()).unwrap();
        let path = home.path().join(".quotabar/daemon/control-token");
        fs::create_dir(&path).unwrap();
        assert!(load_control_token(home.path()).is_err());
        fs::remove_dir(&path).unwrap();

        let fifo = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a valid NUL-terminated temp path and only creates a FIFO.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(load_control_token(home.path()).is_err());
        fs::remove_file(&path).unwrap();

        let socket = UnixListener::bind(&path).unwrap();
        assert!(load_control_token(home.path()).is_err());
        drop(socket);
        fs::remove_file(&path).unwrap();

        let target = home.path().join("synthetic-token-target");
        fs::write(&target, b"preserve target bytes").unwrap();
        symlink(&target, &path).unwrap();
        assert!(matches!(
            load_control_token(home.path()),
            Err(RuntimeStateError::UnsafeManagedPath)
        ));
        assert_eq!(fs::read(&target).unwrap(), b"preserve target bytes");
    }

    #[test]
    fn control_token_inserts_only_a_sensitive_header_value() {
        let token = ControlToken([b'c'; CONTROL_TOKEN_HEX_LENGTH]);
        let mut headers = axum::http::HeaderMap::new();
        token.insert_control_header(&mut headers).unwrap();
        let value = headers.get("x-quotabar-control-token").unwrap();
        assert!(value.is_sensitive());
        assert_eq!(value.as_bytes(), &[b'c'; CONTROL_TOKEN_HEX_LENGTH]);
    }

    #[cfg(unix)]
    #[test]
    fn token_symlink_fails_closed_without_touching_target() {
        use std::os::unix::fs::symlink;

        let home = TestHome::new();
        let state = RuntimeState::open(home.path()).unwrap();
        let path = home.path().join(".quotabar/daemon/control-token");
        let target = home.path().join("token-target");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, &path).unwrap();

        assert!(matches!(
            state.load_or_create_control_token(),
            Err(RuntimeStateError::UnsafeManagedPath)
        ));
        assert_eq!(fs::read(target).unwrap(), b"untouched");
    }

    #[test]
    fn token_comparison_is_exact_and_rejects_mismatched_values() {
        assert!(constant_time_eq(b"0123", b"0123"));
        assert!(!constant_time_eq(b"0123", b"0124"));
        assert!(!constant_time_eq(b"0123", b"012"));
        assert!(
            parse_control_token(&[vec![b'a'; CONTROL_TOKEN_HEX_LENGTH], vec![b'\n']].concat())
                .is_some()
        );
        assert!(
            parse_control_token(&[vec![b'A'; CONTROL_TOKEN_HEX_LENGTH], vec![b'\n']].concat())
                .is_none()
        );
    }

    #[test]
    fn missing_config_defaults_empty_and_each_load_observes_manual_edits() {
        let home = TestHome::new();
        let state = RuntimeState::open(home.path()).unwrap();
        assert!(state.load_config().unwrap().disabled_providers.is_empty());

        fs::write(config_path(&home), br#"{"disabledProviders":["muse"]}"#).unwrap();
        assert_eq!(state.load_config().unwrap().disabled_providers, ["muse"]);
        fs::write(config_path(&home), br#"{"disabledProviders":["claude"]}"#).unwrap();
        assert_eq!(state.load_config().unwrap().disabled_providers, ["claude"]);
    }

    #[test]
    fn invalid_configuration_fails_closed_and_mutation_preserves_original_bytes() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let invalid = br#"{"disabledProviders":["unknown-provider"]}"#;
        fs::write(config_path(&home), invalid).unwrap();

        assert!(matches!(
            state.load_config(),
            Err(RuntimeStateError::InvalidConfiguration)
        ));
        assert!(matches!(
            state.set_enabled("muse", false),
            Err(RuntimeStateError::InvalidConfiguration)
        ));
        assert_eq!(fs::read(config_path(&home)).unwrap(), invalid);
    }

    #[test]
    fn config_postrename_sync_failure_returns_and_applies_committed_value() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let update = state
            .set_enabled_with_durability("muse", false, |_| {
                Err(io::Error::other("injected directory sync failure"))
            })
            .unwrap();

        assert_eq!(update.config.disabled_providers, ["muse"]);
        assert!(matches!(
            update.durability,
            DurabilityOutcome::CommittedButDurabilityUnconfirmed(_)
        ));
        assert_eq!(state.load_config().unwrap(), update.config);
    }

    #[test]
    fn malformed_configuration_types_unknown_fields_and_duplicates_are_rejected() {
        for invalid in [
            br#"{"disabledProviders":"muse"}"#.as_slice(),
            br#"{"disabledProviders":[],"unexpected":true}"#.as_slice(),
            br#"{"disabledProviders":["muse","muse"]}"#.as_slice(),
            br#"{"disabledProviders":["unknown-provider"]}"#.as_slice(),
            br#"{}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_config(invalid),
                Err(RuntimeStateError::InvalidConfiguration)
            ));
        }
    }

    #[test]
    fn mutation_is_idempotent_validates_ids_and_handles_all_shipped_providers() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();

        for id in PROVIDERS {
            let disabled = state.set_enabled(id, false).unwrap();
            assert_eq!(disabled.config.disabled_providers, [id]);
            assert!(matches!(disabled.durability, DurabilityOutcome::Durable));
            let repeated = state.set_enabled(id, false).unwrap();
            assert_eq!(repeated.config, disabled.config);
            assert!(matches!(repeated.durability, DurabilityOutcome::Durable));
            assert!(
                state
                    .set_enabled(id, true)
                    .unwrap()
                    .config
                    .disabled_providers
                    .is_empty()
            );
            assert!(state.load_config().unwrap().disabled_providers.is_empty());
        }

        assert!(matches!(
            state.set_enabled("not-a-provider", false),
            Err(RuntimeStateError::InvalidProvider)
        ));
        assert!(matches!(
            validate_provider_id("not-a-provider"),
            Err(RuntimeStateError::InvalidProvider)
        ));
    }

    #[test]
    fn oversized_configuration_fails_closed_and_preserves_bytes() {
        let home = TestHome::new();
        let state = RuntimeState::open(home.path()).unwrap();
        let oversized = vec![b' '; 64 * 1024 + 1];
        fs::write(config_path(&home), &oversized).unwrap();

        assert!(matches!(
            state.load_config(),
            Err(RuntimeStateError::InvalidConfiguration)
        ));
        assert_eq!(fs::read(config_path(&home)).unwrap(), oversized);
    }

    #[test]
    fn oversized_cooldown_recovers_with_a_persisted_guard() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let now = time(1_800_000_000);
        fs::write(cooldown_path(&home, "claude"), vec![b' '; 64 * 1024 + 1]).unwrap();

        let recovered = state.load_cooldown("claude", now).unwrap();
        assert_eq!(recovered.not_before, Some(now + Duration::seconds(300)));
        assert!(recovered.recovered);
    }

    #[test]
    fn cooldown_missing_exact_expiry_and_future_deadline_behave_as_expected() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let now = time(1_800_000_000);
        assert_eq!(
            state.load_cooldown("claude", now).unwrap(),
            CooldownState {
                not_before: None,
                recovered: false,
            }
        );

        let deadline = now + Duration::seconds(60);
        assert_eq!(
            state.extend_cooldown("claude", deadline, now).unwrap(),
            deadline
        );
        assert_eq!(
            state.load_cooldown("claude", now).unwrap().not_before,
            Some(now + Duration::seconds(60))
        );
        assert_eq!(
            state
                .load_cooldown("claude", now + Duration::seconds(60))
                .unwrap()
                .not_before,
            None
        );
    }

    #[test]
    fn cooldown_record_contains_only_provider_id_and_rfc3339_deadline_for_each_provider() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let now = time(1_800_000_000);
        let deadline = now + Duration::seconds(90);

        for id in PROVIDERS {
            state.extend_cooldown(id, deadline, now).unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(cooldown_path(&home, id)).unwrap()).unwrap();
            assert_eq!(value.as_object().unwrap().len(), 2);
            assert_eq!(value["providerId"], id);
            assert_eq!(value["notBefore"], "2027-01-15T08:01:30Z");
        }
    }

    #[test]
    fn malformed_timestamp_and_provider_mismatch_trigger_recovery() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let now = time(1_800_000_000);
        let path = cooldown_path(&home, "claude");

        fs::write(&path, br#"{"providerId":"claude","notBefore":"tomorrow"}"#).unwrap();
        let first = state.load_cooldown("claude", now).unwrap();
        assert_eq!(first.not_before, Some(now + Duration::seconds(300)));
        assert!(first.recovered);

        fs::write(
            &path,
            br#"{"providerId":"muse","notBefore":"2027-01-15T08:00:00Z"}"#,
        )
        .unwrap();
        let second = state.load_cooldown("claude", now).unwrap();
        assert_eq!(second.not_before, Some(now + Duration::seconds(300)));
        assert!(second.recovered);
    }

    #[test]
    fn recovery_deadline_is_saved_once_and_honored_after_reopen() {
        let home = TestHome::new();
        let now = time(1_800_000_000);
        {
            let mut state = RuntimeState::open(home.path()).unwrap();
            fs::write(
                cooldown_path(&home, "codex"),
                br#"{"providerId":"codex","notBefore":"bad-time"}"#,
            )
            .unwrap();
            let recovered = state.load_cooldown("codex", now).unwrap();
            assert_eq!(recovered.not_before, Some(now + Duration::seconds(300)));
            assert!(recovered.recovered);
        }
        let mut reopened = RuntimeState::open(home.path()).unwrap();
        let later = reopened
            .load_cooldown("codex", now + Duration::seconds(30))
            .unwrap();
        assert_eq!(later.not_before, Some(now + Duration::seconds(300)));
        assert!(!later.recovered);
    }

    #[test]
    fn failed_recovery_persistence_returns_an_error_instead_of_ready() {
        let home = TestHome::new();
        let state = RuntimeState::open(home.path()).unwrap();
        fs::write(
            cooldown_path(&home, "muse"),
            br#"{"providerId":"muse","notBefore":"bad-time"}"#,
        )
        .unwrap();
        let result = state.load_cooldown_with("muse", time(1_800_000_000), |_| {
            Err(RuntimeStateError::Persistence(io::Error::other("injected")))
        });
        assert!(matches!(result, Err(RuntimeStateError::Persistence(_))));
    }

    #[test]
    fn cooldown_postrename_sync_failure_returns_persistence_error_and_keeps_guard() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let now = time(1_800_000_000);
        let deadline = now + Duration::seconds(900);
        let result = state.extend_cooldown_with_durability("muse", deadline, now, |_| {
            Err(io::Error::other("injected directory sync failure"))
        });

        assert!(matches!(result, Err(RuntimeStateError::Persistence(_))));
        assert_eq!(
            state.load_cooldown("muse", now).unwrap().not_before,
            Some(deadline)
        );
    }

    #[test]
    fn unrepresentable_deadline_fails_before_replacing_existing_record() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let now = time(1_800_000_000);
        state
            .extend_cooldown("codex", now + Duration::seconds(60), now)
            .unwrap();
        let path = cooldown_path(&home, "codex");
        let original = fs::read(&path).unwrap();

        let result = state.extend_cooldown("codex", DateTime::<Utc>::MAX_UTC, now);
        assert!(matches!(result, Err(RuntimeStateError::InvalidTime)));
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn extension_keeps_the_maximum_deadline_across_restarts() {
        let home = TestHome::new();
        let now = time(1_800_000_000);
        let earlier = now + Duration::seconds(10);
        let later = now + Duration::seconds(500);
        {
            let mut state = RuntimeState::open(home.path()).unwrap();
            assert_eq!(state.extend_cooldown("cursor", later, now).unwrap(), later);
        }
        {
            let mut state = RuntimeState::open(home.path()).unwrap();
            assert_eq!(
                state.extend_cooldown("cursor", earlier, now).unwrap(),
                later
            );
        }
        let mut state = RuntimeState::open(home.path()).unwrap();
        assert_eq!(
            state.load_cooldown("cursor", now).unwrap().not_before,
            Some(later)
        );
    }

    #[test]
    fn partial_atomic_write_preserves_old_bytes_and_removes_temporary_file() {
        let home = TestHome::new();
        let state = RuntimeState::open(home.path()).unwrap();
        let path = config_path(&home);
        let original = br#"{"disabledProviders":["muse"]}"#;
        fs::write(&path, original).unwrap();

        let result = state.atomic_write_with(
            &path,
            b"replacement",
            |file, bytes| {
                file.write_all(&bytes[..4])?;
                Err(io::Error::other("injected partial write"))
            },
            |_| Ok(()),
        );
        assert_persistence_error(result);
        assert_eq!(fs::read(&path).unwrap(), original);
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            !names
                .iter()
                .any(|name| name.to_string_lossy().ends_with(".tmp"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn directories_and_managed_files_have_private_modes_and_symlinks_are_not_touched() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        let quotabar_dir = home.path().join(".quotabar");
        let state_dir = quotabar_dir.join("daemon");
        assert_eq!(fs::metadata(&quotabar_dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&state_dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(state_dir.join("instance.lock"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );

        state.set_enabled("muse", false).unwrap();
        state
            .extend_cooldown("muse", time(1_800_000_100), time(1_800_000_000))
            .unwrap();
        assert_eq!(
            fs::metadata(config_path(&home)).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(cooldown_path(&home, "muse")).unwrap().mode() & 0o777,
            0o600
        );

        let config_target = home.path().join("config-target");
        fs::write(&config_target, b"keep config target").unwrap();
        fs::remove_file(config_path(&home)).unwrap();
        symlink(&config_target, config_path(&home)).unwrap();
        assert!(matches!(
            state.load_config(),
            Err(RuntimeStateError::UnsafeManagedPath)
        ));
        assert_eq!(fs::read(&config_target).unwrap(), b"keep config target");

        let cooldown_target = home.path().join("cooldown-target");
        fs::write(&cooldown_target, b"keep cooldown target").unwrap();
        let cooldown = cooldown_path(&home, "claude");
        symlink(&cooldown_target, &cooldown).unwrap();
        assert!(matches!(
            state.load_cooldown("claude", time(1_800_000_000)),
            Err(RuntimeStateError::UnsafeManagedPath)
        ));
        assert_eq!(fs::read(&cooldown_target).unwrap(), b"keep cooldown target");
        assert!(
            fs::symlink_metadata(cooldown)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_read_rejects_regular_to_symlink_swap_without_touching_target() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let home = TestHome::new();
        let _state = RuntimeState::open(home.path()).unwrap();
        let path = home.path().join(".quotabar/daemon/managed-state");
        let target = home.path().join("symlink-target");
        fs::write(&path, b"the checked regular file").unwrap();
        fs::write(&target, b"preserve target bytes").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let target_mode = fs::metadata(&target).unwrap().mode() & 0o777;

        let result = read_managed_file_with_hook(&path, 1024, || {
            fs::remove_file(&path).unwrap();
            symlink(&target, &path).unwrap();
        });

        let rejected = matches!(result, Err(RuntimeStateError::UnsafeManagedPath));
        assert_eq!(fs::read(&target).unwrap(), b"preserve target bytes");
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, target_mode);
        assert!(rejected);
    }

    #[test]
    fn nonregular_configuration_and_cooldown_paths_fail_closed() {
        let home = TestHome::new();
        let mut state = RuntimeState::open(home.path()).unwrap();
        fs::create_dir(config_path(&home)).unwrap();
        fs::create_dir(cooldown_path(&home, "muse")).unwrap();

        assert!(matches!(
            state.load_config(),
            Err(RuntimeStateError::UnsafeManagedPath)
        ));
        assert!(matches!(
            state.load_cooldown("muse", time(1_800_000_000)),
            Err(RuntimeStateError::UnsafeManagedPath)
        ));
    }
}
