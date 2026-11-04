use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

const TEMP_NAME_ATTEMPTS: usize = 64;
const TEMP_NAME_RANDOM_BYTES: usize = 16;

/// Safe, content-free error categories for credential-file operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialFileError {
    Missing,
    Io,
    UnsafeFile,
    TooLarge,
    InvalidPath,
    RandomSource,
}

/// Permission bits accepted for a credential file. Only snapshots can produce
/// this value, and replacements reject any mode with group or world access.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct RestrictiveMode(u32);

/// An exact, bounded credential-file snapshot. The bytes deliberately have no
/// formatting or serialization implementation and are cleared when dropped.
pub struct JsonSnapshot {
    bytes: Vec<u8>,
    mode: RestrictiveMode,
}

impl JsonSnapshot {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn mode(&self) -> RestrictiveMode {
        self.mode
    }

    pub(crate) fn from_replacement(bytes: Vec<u8>, mode: RestrictiveMode) -> Self {
        Self { bytes, mode }
    }
}

impl Drop for JsonSnapshot {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceOutcome {
    Replaced,
    SourceChanged,
    CommittedDurabilityUncertain,
}

/// Read a regular, privately permissioned JSON file through a no-follow,
/// nonblocking descriptor. Loading never changes the CLI-owned file's mode.
pub fn read_json_snapshot(
    path: &Path,
    max_bytes: u64,
) -> Result<JsonSnapshot, CredentialFileError> {
    let (parent, name) = open_parent(path)?;
    read_snapshot_at(&parent, &name, max_bytes, || {})
}

/// Atomically replace a credential file only if its current bytes and mode
/// still match the supplied snapshot. The immediate compare narrows the race
/// with a CLI writer, but a CLI can still write between that compare and rename:
/// there is no shared lock or filesystem compare-and-swap with the CLI.
pub fn replace_if_unchanged(
    path: &Path,
    original_bytes: &[u8],
    updated_bytes: &[u8],
    original_mode: RestrictiveMode,
) -> Result<ReplaceOutcome, CredentialFileError> {
    replace_if_unchanged_with(
        path,
        original_bytes,
        updated_bytes,
        original_mode,
        |file, bytes| file.write_all(bytes),
        |directory| directory.sync_all(),
    )
}

fn replace_if_unchanged_with<W, D>(
    path: &Path,
    original_bytes: &[u8],
    updated_bytes: &[u8],
    original_mode: RestrictiveMode,
    writer: W,
    directory_sync: D,
) -> Result<ReplaceOutcome, CredentialFileError>
where
    W: FnOnce(&mut File, &[u8]) -> io::Result<()>,
    D: FnOnce(&File) -> io::Result<()>,
{
    if original_mode.0 & 0o077 != 0 {
        return Err(CredentialFileError::UnsafeFile);
    }
    let (parent, name) = open_parent(path)?;

    match read_snapshot_at(&parent, &name, original_bytes.len() as u64, || {}) {
        Ok(snapshot) if snapshot.bytes() == original_bytes && snapshot.mode() == original_mode => {}
        Ok(_) | Err(CredentialFileError::Missing | CredentialFileError::TooLarge) => {
            return Ok(ReplaceOutcome::SourceChanged);
        }
        Err(error) => return Err(error),
    }

    let (temporary_name, mut temporary_file) = create_temporary_file(&parent)?;
    let mut cleanup = TemporaryFileCleanup {
        directory: &parent,
        name: temporary_name.clone(),
        committed: false,
    };
    writer(&mut temporary_file, updated_bytes).map_err(|_| CredentialFileError::Io)?;
    temporary_file
        .set_permissions(std::fs::Permissions::from_mode(original_mode.0))
        .map_err(|_| CredentialFileError::Io)?;
    temporary_file
        .sync_all()
        .map_err(|_| CredentialFileError::Io)?;
    drop(temporary_file);

    // Reopen through the already-open parent immediately before the rename.
    // A changed or newly oversized source is not an invitation to retry a
    // stale credential rotation.
    match read_snapshot_at(&parent, &name, original_bytes.len() as u64, || {}) {
        Ok(snapshot) if snapshot.bytes() == original_bytes && snapshot.mode() == original_mode => {}
        Ok(_) | Err(CredentialFileError::Missing | CredentialFileError::TooLarge) => {
            return Ok(ReplaceOutcome::SourceChanged);
        }
        Err(error) => return Err(error),
    }

    rename_at(&parent, &temporary_name, &name).map_err(|_| CredentialFileError::Io)?;
    cleanup.committed = true;

    if directory_sync(&parent).is_ok() {
        Ok(ReplaceOutcome::Replaced)
    } else {
        // Rename is the commit point. A failed directory sync cannot be
        // reported as an uncommitted write or rolled back safely.
        Ok(ReplaceOutcome::CommittedDurabilityUncertain)
    }
}

fn open_parent(path: &Path) -> Result<(File, CString), CredentialFileError> {
    let file_name = path.file_name().ok_or(CredentialFileError::InvalidPath)?;
    let name = CString::new(file_name.as_bytes()).map_err(|_| CredentialFileError::InvalidPath)?;
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let parent = options.open(parent_path).map_err(map_io_error)?;
    if !parent.metadata().map_err(map_io_error)?.is_dir() {
        return Err(CredentialFileError::UnsafeFile);
    }
    Ok((parent, name))
}

fn read_snapshot_at<F>(
    parent: &File,
    name: &CString,
    max_bytes: u64,
    after_path_check: F,
) -> Result<JsonSnapshot, CredentialFileError>
where
    F: FnOnce(),
{
    let mut path_metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            path_metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(map_io_error(io::Error::last_os_error()));
    }
    let path_metadata = unsafe { path_metadata.assume_init() };
    if path_metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(CredentialFileError::UnsafeFile);
    }

    // The hook makes the no-follow race deterministic in tests. Production
    // still relies on openat(O_NOFOLLOW), not this earlier pathname check.
    after_path_check();

    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(map_io_error(io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata().map_err(map_io_error)?;
    if !metadata.is_file() {
        return Err(CredentialFileError::UnsafeFile);
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CredentialFileError::UnsafeFile);
    }
    if metadata.len() > max_bytes {
        return Err(CredentialFileError::TooLarge);
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| CredentialFileError::Io)?;
    if bytes.len() as u64 > max_bytes {
        bytes.fill(0);
        return Err(CredentialFileError::TooLarge);
    }
    Ok(JsonSnapshot {
        bytes,
        mode: RestrictiveMode(mode),
    })
}

fn create_temporary_file(parent: &File) -> Result<(CString, File), CredentialFileError> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let mut random = [0_u8; TEMP_NAME_RANDOM_BYTES];
        getrandom::fill(&mut random).map_err(|_| CredentialFileError::RandomSource)?;
        let name = CString::new(format!(".quotabar-credential-{}.tmp", hex(&random)))
            .map_err(|_| CredentialFileError::InvalidPath)?;
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600 as libc::mode_t as libc::c_int,
            )
        };
        if descriptor >= 0 {
            let file = unsafe { File::from_raw_fd(descriptor) };
            if file
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .is_err()
            {
                drop(file);
                unsafe {
                    libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0);
                }
                return Err(CredentialFileError::Io);
            }
            return Ok((name, file));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(map_io_error(error));
        }
    }
    Err(CredentialFileError::Io)
}

fn rename_at(parent: &File, from: &CString, to: &CString) -> io::Result<()> {
    let result = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(DIGITS[(byte >> 4) as usize] as char);
        value.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    value
}

fn map_io_error(error: io::Error) -> CredentialFileError {
    match error.raw_os_error() {
        Some(libc::ELOOP) => CredentialFileError::UnsafeFile,
        _ if error.kind() == io::ErrorKind::NotFound => CredentialFileError::Missing,
        _ => CredentialFileError::Io,
    }
}

struct TemporaryFileCleanup<'a> {
    directory: &'a File,
    name: CString,
    committed: bool,
}

impl Drop for TemporaryFileCleanup<'_> {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

#[cfg(test)]
fn read_json_snapshot_with_hook<F>(
    path: &Path,
    max_bytes: u64,
    after_path_check: F,
) -> Result<JsonSnapshot, CredentialFileError>
where
    F: FnOnce(),
{
    let (parent, name) = open_parent(path)?;
    read_snapshot_at(&parent, &name, max_bytes, after_path_check)
}

#[cfg(test)]
fn replace_if_unchanged_with_hooks<W, D>(
    path: &Path,
    original_bytes: &[u8],
    updated_bytes: &[u8],
    original_mode: RestrictiveMode,
    writer: W,
    directory_sync: D,
) -> Result<ReplaceOutcome, CredentialFileError>
where
    W: FnOnce(&mut File, &[u8]) -> io::Result<()>,
    D: FnOnce(&File) -> io::Result<()>,
{
    replace_if_unchanged_with(
        path,
        original_bytes,
        updated_bytes,
        original_mode,
        writer,
        directory_sync,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CredentialFileError, ReplaceOutcome, read_json_snapshot, read_json_snapshot_with_hook,
        replace_if_unchanged, replace_if_unchanged_with_hooks,
    };
    use std::fs;
    use std::io::{self, Write};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quotabard-credential-file-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn credential_path(&self) -> PathBuf {
            self.path().join("auth.json")
        }

        fn write_private(&self, path: &Path, bytes: &[u8], mode: u32) {
            fs::write(path, bytes).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snapshot_is_exact_and_does_not_mutate_read_only_cli_permissions() {
        let directory = TestDirectory::new();
        let path = directory.credential_path();
        let original = br#"{"access_token":"synthetic","unknown":{"keep":true}}"#;
        directory.write_private(&path, original, 0o400);

        let snapshot = read_json_snapshot(&path, 1024).unwrap();
        assert_eq!(snapshot.bytes(), original);
        assert_eq!(snapshot.mode().0, 0o400);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn snapshot_rejects_symlink_without_touching_its_target() {
        let directory = TestDirectory::new();
        let target = directory.path().join("target.json");
        let path = directory.credential_path();
        directory.write_private(&target, b"preserve", 0o600);
        symlink(&target, &path).unwrap();

        assert!(matches!(
            read_json_snapshot(&path, 1024),
            Err(CredentialFileError::UnsafeFile)
        ));
        assert_eq!(fs::read(&target).unwrap(), b"preserve");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn nofollow_rejects_a_deterministic_regular_file_to_symlink_swap() {
        let directory = TestDirectory::new();
        let path = directory.credential_path();
        let target = directory.path().join("target.json");
        directory.write_private(&path, b"snapshot", 0o600);
        directory.write_private(&target, b"untouched target", 0o600);
        let hook_path = path.clone();
        let hook_target = target.clone();

        let result = read_json_snapshot_with_hook(&path, 1024, move || {
            fs::remove_file(&hook_path).unwrap();
            symlink(&hook_target, &hook_path).unwrap();
        });

        assert!(matches!(result, Err(CredentialFileError::UnsafeFile)));
        assert_eq!(fs::read(&target).unwrap(), b"untouched target");
    }

    #[test]
    fn snapshot_rejects_nonregular_and_oversized_files() {
        let directory = TestDirectory::new();
        let nonregular = directory.path().join("directory.json");
        fs::create_dir(&nonregular).unwrap();
        assert!(matches!(
            read_json_snapshot(&nonregular, 1024),
            Err(CredentialFileError::UnsafeFile)
        ));

        let oversized = directory.credential_path();
        directory.write_private(&oversized, &[b'x'; 33], 0o600);
        assert!(matches!(
            read_json_snapshot(&oversized, 32),
            Err(CredentialFileError::TooLarge)
        ));
    }

    #[test]
    fn replacement_preserves_bytes_unknown_payload_and_restrictive_mode() {
        let directory = TestDirectory::new();
        let path = directory.credential_path();
        let original = br#"{"access_token":"old","unknown":"preserve"}"#;
        let updated = br#"{"access_token":"new","unknown":"preserve"}"#;
        directory.write_private(&path, original, 0o600);
        let snapshot = read_json_snapshot(&path, 1024).unwrap();

        let outcome =
            replace_if_unchanged(&path, snapshot.bytes(), updated, snapshot.mode()).unwrap();

        assert_eq!(outcome, ReplaceOutcome::Replaced);
        assert_eq!(fs::read(&path).unwrap(), updated);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn replacement_refuses_changed_bytes_or_a_changed_restrictive_mode() {
        let directory = TestDirectory::new();
        let path = directory.credential_path();
        let original = b"{\"token\":\"old\"}";
        let updated = b"{\"token\":\"new\"}";
        directory.write_private(&path, original, 0o600);
        let snapshot = read_json_snapshot(&path, 1024).unwrap();

        fs::write(&path, b"{\"token\":\"newer\"}").unwrap();
        assert_eq!(
            replace_if_unchanged(&path, snapshot.bytes(), updated, snapshot.mode()).unwrap(),
            ReplaceOutcome::SourceChanged
        );
        assert_eq!(fs::read(&path).unwrap(), b"{\"token\":\"newer\"}");

        fs::write(&path, original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(
            replace_if_unchanged(&path, snapshot.bytes(), updated, snapshot.mode()).unwrap(),
            ReplaceOutcome::SourceChanged
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }

    #[test]
    fn failed_precommit_write_keeps_original_and_cleans_temporary_file() {
        let directory = TestDirectory::new();
        let path = directory.credential_path();
        let original = b"{\"token\":\"old\"}";
        directory.write_private(&path, original, 0o600);
        let snapshot = read_json_snapshot(&path, 1024).unwrap();

        let result = replace_if_unchanged_with_hooks(
            &path,
            snapshot.bytes(),
            b"{\"token\":\"new\"}",
            snapshot.mode(),
            |file, bytes| {
                file.write_all(&bytes[..4])?;
                Err(io::Error::other("injected temporary write failure"))
            },
            |_| Ok(()),
        );

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn postrename_directory_sync_failure_reports_committed_uncertain_value() {
        let directory = TestDirectory::new();
        let path = directory.credential_path();
        let original = b"{\"token\":\"old\"}";
        let updated = b"{\"token\":\"new\"}";
        directory.write_private(&path, original, 0o600);
        let snapshot = read_json_snapshot(&path, 1024).unwrap();

        let outcome = replace_if_unchanged_with_hooks(
            &path,
            snapshot.bytes(),
            updated,
            snapshot.mode(),
            |file, bytes| file.write_all(bytes),
            |_| Err(io::Error::other("injected directory sync failure")),
        )
        .unwrap();

        assert_eq!(outcome, ReplaceOutcome::CommittedDurabilityUncertain);
        assert_eq!(fs::read(&path).unwrap(), updated);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
