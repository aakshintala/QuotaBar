use std::env;
use std::fs::{self, DirBuilder};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const ALLOWED_SERVICE: &str = "quotabar-proof-allowed";
pub const RESTRICTED_SERVICE: &str = "quotabar-proof-restricted";
pub const FIXTURE_ACCOUNT: &str = "quotabar-proof-account";
const ALLOWED_SECRET: &[u8] = b"synthetic-quotabar-proof-allowed-secret";
const ALLOWED_UPDATED_SECRET: &[u8] = b"synthetic-quotabar-proof-updated-secret";
const RESTRICTED_SECRET: &[u8] = b"synthetic-quotabar-proof-restricted-secret";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterError {
    OperationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailureKind {
    AccessDenied,
    ItemNotFound,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadFailure {
    pub kind: ReadFailureKind,
    pub data_returned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFailureKind {
    RecognizedRefusal,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteFailure {
    pub kind: WriteFailureKind,
}

pub trait KeychainAdapter {
    type Fixture;

    fn disable_user_interaction(&mut self) -> Result<(), AdapterError>;
    fn create_fixture(&mut self, path: &Path) -> Result<Self::Fixture, AdapterError>;
    fn add_allowed(
        &mut self,
        fixture: &Self::Fixture,
        service: &str,
        account: &str,
        secret: &[u8],
    ) -> Result<(), AdapterError>;
    fn add_restricted(
        &mut self,
        fixture: &Self::Fixture,
        service: &str,
        account: &str,
        secret: &[u8],
    ) -> Result<(), AdapterError>;
    fn modify_control(
        &mut self,
        fixture: &Self::Fixture,
        replacement: &[u8],
    ) -> Result<(), WriteFailure>;
    fn read(
        &mut self,
        fixture: &Self::Fixture,
        service: &str,
        account: &str,
    ) -> Result<Vec<u8>, ReadFailure>;
    fn lock_fixture(&mut self, fixture: &Self::Fixture) -> Result<(), AdapterError>;
    fn fixture_is_locked(&mut self, fixture: &Self::Fixture) -> Result<bool, AdapterError>;
    fn unlock_fixture(&mut self, fixture: &Self::Fixture) -> Result<(), AdapterError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofError {
    UnsafeFixturePath,
    InteractionSuppressionFailed,
    KeychainCreationFailed,
    AllowedItemCreationFailed,
    RestrictedItemCreationFailed,
    AllowedReadFailed,
    AllowedReadMismatch,
    RestrictedReadWasNotDenied,
    FixtureLockFailed,
    LockedReadWasNotDenied,
    AllowedModifyFailed,
    AllowedModifyReadbackMismatch,
    FixtureUnlockFailed,
    LockedModifyVerificationFailed,
    LockedModifyDataMismatch,
    LockedStateUnverified,
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnsafeFixturePath => "refused an unsafe disposable fixture path",
            Self::InteractionSuppressionFailed => "could not disable Keychain user interaction",
            Self::KeychainCreationFailed => "could not create the disposable fixture Keychain",
            Self::AllowedItemCreationFailed => "could not create the allowed synthetic item",
            Self::RestrictedItemCreationFailed => "could not create the restricted synthetic item",
            Self::AllowedReadFailed => "allowed synthetic item could not be read",
            Self::AllowedReadMismatch => "allowed read did not exactly match its synthetic data",
            Self::RestrictedReadWasNotDenied => "restricted read denial was not proven",
            Self::FixtureLockFailed => "could not confirm the fixture Keychain was locked",
            Self::LockedReadWasNotDenied => "locked-fixture read denial was not proven",
            Self::AllowedModifyFailed => "allowed synthetic item could not be modified",
            Self::AllowedModifyReadbackMismatch => {
                "allowed modify read-back did not match synthetic data"
            }
            Self::FixtureUnlockFailed => "could not unlock the disposable fixture for verification",
            Self::LockedModifyVerificationFailed => "locked modify outcome could not be verified",
            Self::LockedModifyDataMismatch => "locked modify outcome did not match fixture data",
            Self::LockedStateUnverified => "fixture lock state could not be verified",
        };
        f.write_str(message)
    }
}

pub fn run<A: KeychainAdapter>(
    adapter: &mut A,
    location: &FixtureDirectory,
) -> Result<(), ProofError> {
    location
        .validate()
        .map_err(|_| ProofError::UnsafeFixturePath)?;

    // This must be the first Keychain operation. The native adapter never restores it.
    adapter
        .disable_user_interaction()
        .map_err(|_| ProofError::InteractionSuppressionFailed)?;
    let fixture = adapter
        .create_fixture(location.keychain_path())
        .map_err(|_| ProofError::KeychainCreationFailed)?;
    adapter
        .add_allowed(&fixture, ALLOWED_SERVICE, FIXTURE_ACCOUNT, ALLOWED_SECRET)
        .map_err(|_| ProofError::AllowedItemCreationFailed)?;
    adapter
        .add_restricted(
            &fixture,
            RESTRICTED_SERVICE,
            FIXTURE_ACCOUNT,
            RESTRICTED_SECRET,
        )
        .map_err(|_| ProofError::RestrictedItemCreationFailed)?;
    require_exact_read(
        adapter,
        &fixture,
        ALLOWED_SERVICE,
        ALLOWED_SECRET,
        ProofError::AllowedReadFailed,
        ProofError::AllowedReadMismatch,
    )?;
    require_denial(
        adapter.read(&fixture, RESTRICTED_SERVICE, FIXTURE_ACCOUNT),
        ProofError::RestrictedReadWasNotDenied,
    )?;
    require_exact_read(
        adapter,
        &fixture,
        ALLOWED_SERVICE,
        ALLOWED_SECRET,
        ProofError::AllowedReadFailed,
        ProofError::AllowedReadMismatch,
    )?;

    adapter
        .modify_control(&fixture, ALLOWED_UPDATED_SECRET)
        .map_err(|_| ProofError::AllowedModifyFailed)?;
    require_exact_read(
        adapter,
        &fixture,
        ALLOWED_SERVICE,
        ALLOWED_UPDATED_SECRET,
        ProofError::AllowedReadFailed,
        ProofError::AllowedModifyReadbackMismatch,
    )?;
    adapter
        .lock_fixture(&fixture)
        .map_err(|_| ProofError::FixtureLockFailed)?;
    if !adapter
        .fixture_is_locked(&fixture)
        .map_err(|_| ProofError::LockedStateUnverified)?
    {
        return Err(ProofError::FixtureLockFailed);
    }
    require_denial(
        adapter.read(&fixture, ALLOWED_SERVICE, FIXTURE_ACCOUNT),
        ProofError::LockedReadWasNotDenied,
    )?;
    let modify_result = adapter.modify_control(&fixture, ALLOWED_SECRET);
    let locked_after_modify = adapter
        .fixture_is_locked(&fixture)
        .map_err(|_| ProofError::LockedStateUnverified)?;
    if !locked_after_modify {
        return Err(ProofError::LockedStateUnverified);
    }
    adapter
        .unlock_fixture(&fixture)
        .map_err(|_| ProofError::FixtureUnlockFailed)?;
    let expected = match modify_result {
        Ok(()) => ALLOWED_SECRET,
        Err(WriteFailure {
            kind: WriteFailureKind::RecognizedRefusal,
        }) => ALLOWED_UPDATED_SECRET,
        Err(WriteFailure {
            kind: WriteFailureKind::Other,
        }) => {
            return Err(ProofError::LockedModifyVerificationFailed);
        }
    };
    require_exact_read(
        adapter,
        &fixture,
        ALLOWED_SERVICE,
        expected,
        ProofError::LockedModifyVerificationFailed,
        ProofError::LockedModifyDataMismatch,
    )
}

fn require_exact_read<A: KeychainAdapter>(
    adapter: &mut A,
    fixture: &A::Fixture,
    service: &str,
    expected: &[u8],
    read_failure: ProofError,
    mismatch_failure: ProofError,
) -> Result<(), ProofError> {
    let mut data = adapter
        .read(fixture, service, FIXTURE_ACCOUNT)
        .map_err(|_| read_failure)?;
    let exact_match = data == expected;
    data.fill(0);
    if exact_match {
        Ok(())
    } else {
        Err(mismatch_failure)
    }
}

fn require_denial(
    result: Result<Vec<u8>, ReadFailure>,
    failure: ProofError,
) -> Result<(), ProofError> {
    match result {
        Err(ReadFailure {
            kind: ReadFailureKind::AccessDenied,
            data_returned: false,
        }) => Ok(()),
        Err(_) => Err(failure),
        Ok(mut data) => {
            data.fill(0);
            Err(failure)
        }
    }
}

/// An exclusive, mode-0700 directory and a new keychain path inside it.
/// Dropping this value removes only that directory using filesystem cleanup.
pub struct FixtureDirectory {
    directory: PathBuf,
    keychain: PathBuf,
}

impl FixtureDirectory {
    pub fn create() -> Result<Self, FixturePathError> {
        let parent = env::temp_dir()
            .canonicalize()
            .map_err(|_| FixturePathError)?;
        for attempt in 0..64_u32 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| FixturePathError)?
                .as_nanos();
            let name = format!("quotabar-proof-{}-{nanos}-{attempt}", std::process::id());
            let directory = parent.join(&name);
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&directory) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                            .is_err()
                        {
                            let _ = fs::remove_dir_all(&directory);
                            return Err(FixturePathError);
                        }
                    }
                    let keychain = directory.join(format!("{name}.keychain"));
                    let location = Self {
                        directory,
                        keychain,
                    };
                    location.validate()?;
                    return Ok(location);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(FixturePathError),
            }
        }
        Err(FixturePathError)
    }

    pub fn keychain_path(&self) -> &Path {
        &self.keychain
    }

    fn validate(&self) -> Result<(), FixturePathError> {
        if !self.directory.is_absolute()
            || !self.keychain.is_absolute()
            || self.keychain.parent() != Some(self.directory.as_path())
            || forbidden_keychain_path(&self.keychain)
            || !self.keychain.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("quotabar-proof-") && name.ends_with(".keychain")
            })
            || !all_existing_components_are_real(&self.directory)
        {
            return Err(FixturePathError);
        }

        let metadata = fs::symlink_metadata(&self.directory).map_err(|_| FixturePathError)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(FixturePathError);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o777 != 0o700 {
                return Err(FixturePathError);
            }
        }
        if fs::read_dir(&self.directory)
            .map_err(|_| FixturePathError)?
            .next()
            .is_some()
        {
            return Err(FixturePathError);
        }
        match fs::symlink_metadata(&self.keychain) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            _ => Err(FixturePathError),
        }
    }
}

impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixturePathError;

fn all_existing_components_are_real(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => current.push(component.as_os_str()),
            _ => return false,
        }
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
    }
    true
}

fn forbidden_keychain_path(path: &Path) -> bool {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return true;
    }
    let path = path.to_string_lossy();
    path.contains("/login.keychain")
        || path.contains("/Library/Keychains/")
        || path.contains("System.keychain")
        || path.contains("SystemRootCertificates.keychain")
}

#[cfg(target_os = "macos")]
pub mod native;

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAdapter {
        events: Vec<&'static str>,
        reads: Vec<PathBuf>,
        allowed: Vec<u8>,
        acl_denial: ReadFailure,
        locked_denial: ReadFailure,
        fail_suppression: bool,
        locked_modify_succeeds: bool,
        corrupt_locked_success: bool,
        mutate_while_refused: bool,
        locked_modify_other: bool,
        lock_before_restricted_read: bool,
        locked: bool,
    }

    impl FakeAdapter {
        fn passing() -> Self {
            let denial = ReadFailure {
                kind: ReadFailureKind::AccessDenied,
                data_returned: false,
            };
            Self {
                events: Vec::new(),
                reads: Vec::new(),
                allowed: ALLOWED_SECRET.to_vec(),
                acl_denial: denial,
                locked_denial: denial,
                fail_suppression: false,
                locked_modify_succeeds: false,
                corrupt_locked_success: false,
                mutate_while_refused: false,
                locked_modify_other: false,
                lock_before_restricted_read: false,
                locked: false,
            }
        }
    }

    impl KeychainAdapter for FakeAdapter {
        type Fixture = PathBuf;

        fn disable_user_interaction(&mut self) -> Result<(), AdapterError> {
            self.events.push("ui-off");
            if self.fail_suppression {
                Err(AdapterError::OperationFailed)
            } else {
                Ok(())
            }
        }

        fn create_fixture(&mut self, path: &Path) -> Result<Self::Fixture, AdapterError> {
            self.events.push("create");
            Ok(path.to_path_buf())
        }

        fn add_allowed(
            &mut self,
            _: &PathBuf,
            service: &str,
            _: &str,
            _: &[u8],
        ) -> Result<(), AdapterError> {
            assert!(service.starts_with("quotabar-proof-"));
            self.events.push("add-allowed");
            Ok(())
        }

        fn add_restricted(
            &mut self,
            _: &PathBuf,
            service: &str,
            _: &str,
            _: &[u8],
        ) -> Result<(), AdapterError> {
            assert!(service.starts_with("quotabar-proof-"));
            self.events.push("add-restricted");
            Ok(())
        }

        fn modify_control(&mut self, _: &PathBuf, replacement: &[u8]) -> Result<(), WriteFailure> {
            self.events.push("modify-control");
            if self.locked && !self.locked_modify_succeeds {
                if self.mutate_while_refused {
                    self.allowed = replacement.to_vec();
                }
                return Err(WriteFailure {
                    kind: if self.locked_modify_other {
                        WriteFailureKind::Other
                    } else {
                        WriteFailureKind::RecognizedRefusal
                    },
                });
            }
            self.allowed = if self.locked && self.corrupt_locked_success {
                b"unexpected synthetic bytes".to_vec()
            } else {
                replacement.to_vec()
            };
            Ok(())
        }

        fn read(
            &mut self,
            path: &PathBuf,
            service: &str,
            account: &str,
        ) -> Result<Vec<u8>, ReadFailure> {
            assert_eq!(account, FIXTURE_ACCOUNT);
            self.reads.push(path.clone());
            if service == RESTRICTED_SERVICE {
                self.events.push("read-restricted");
                if self.lock_before_restricted_read {
                    self.locked = true;
                }
                if self.locked {
                    Err(self.locked_denial)
                } else {
                    Err(self.acl_denial)
                }
            } else if self.locked {
                self.events.push("read-locked");
                Err(self.locked_denial)
            } else {
                self.events.push("read-allowed");
                Ok(self.allowed.clone())
            }
        }

        fn lock_fixture(&mut self, _: &PathBuf) -> Result<(), AdapterError> {
            self.events.push("lock");
            self.locked = true;
            Ok(())
        }

        fn fixture_is_locked(&mut self, _: &PathBuf) -> Result<bool, AdapterError> {
            self.events.push("check-lock");
            Ok(self.locked)
        }

        fn unlock_fixture(&mut self, _: &PathBuf) -> Result<(), AdapterError> {
            self.events.push("unlock");
            self.locked = false;
            Ok(())
        }
    }

    fn location() -> FixtureDirectory {
        FixtureDirectory::create().expect("private disposable directory")
    }

    #[test]
    fn fixture_path_is_private_empty_absolute_and_new() {
        let fixture = location();
        assert!(fixture.keychain_path().is_absolute());
        assert_eq!(
            fixture.keychain_path().parent(),
            Some(fixture.directory.as_path())
        );
        assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&fixture.directory)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        fs::write(fixture.keychain_path(), b"preexisting").unwrap();
        assert!(fixture.validate().is_err());
    }

    #[test]
    fn path_policy_rejects_login_system_and_symlink_fixture_paths() {
        for path in [
            "/tmp/login.keychain",
            "/Users/example/Library/Keychains/login.keychain-db",
            "/Library/Keychains/System.keychain",
            "/System/Library/Keychains/SystemRootCertificates.keychain",
            "relative/quotabar-proof-test.keychain",
        ] {
            assert!(forbidden_keychain_path(Path::new(path)));
        }
        let fixture = location();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("/login.keychain", fixture.keychain_path()).unwrap();
            assert!(fixture.validate().is_err());
        }
    }

    #[test]
    fn proof_disables_interaction_first_and_scopes_reads_to_the_fixture() {
        let location = location();
        let mut adapter = FakeAdapter::passing();
        run(&mut adapter, &location).expect("all synthetic proof cases pass");
        assert_eq!(
            adapter.events,
            [
                "ui-off",
                "create",
                "add-allowed",
                "add-restricted",
                "read-allowed",
                "read-restricted",
                "read-allowed",
                "modify-control",
                "read-allowed",
                "lock",
                "check-lock",
                "read-locked",
                "modify-control",
                "check-lock",
                "unlock",
                "read-allowed"
            ]
        );
        assert_eq!(adapter.reads.len(), 6);
        assert!(
            adapter
                .reads
                .iter()
                .all(|path| path == location.keychain_path())
        );
    }

    #[test]
    fn read_denial_requires_unlocked_positive_controls() {
        let location = location();
        let mut adapter = FakeAdapter::passing();
        adapter.lock_before_restricted_read = true;
        assert_eq!(
            run(&mut adapter, &location),
            Err(ProofError::AllowedReadFailed)
        );
        assert_eq!(
            adapter.events,
            [
                "ui-off",
                "create",
                "add-allowed",
                "add-restricted",
                "read-allowed",
                "read-restricted",
                "read-locked",
            ]
        );
    }

    #[test]
    fn locked_write_success_is_accepted_only_with_exact_replacement() {
        let location = location();
        let mut adapter = FakeAdapter::passing();
        adapter.locked_modify_succeeds = true;
        run(&mut adapter, &location).expect("consistent raw locked success is valid evidence");
        assert_eq!(adapter.allowed, ALLOWED_SECRET);
        assert_eq!(
            adapter
                .events
                .iter()
                .filter(|event| **event == "modify-control")
                .count(),
            2
        );
    }

    #[test]
    fn locked_refusal_requires_unchanged_bytes_and_other_errors_fail() {
        let location = location();
        let mut adapter = FakeAdapter::passing();
        run(&mut adapter, &location).expect("recognized refusal with original bytes is valid");
        assert_eq!(adapter.allowed, ALLOWED_UPDATED_SECRET);

        let mut adapter = FakeAdapter::passing();
        adapter.locked_modify_succeeds = true;
        adapter.corrupt_locked_success = true;
        assert_eq!(
            run(&mut adapter, &location),
            Err(ProofError::LockedModifyDataMismatch)
        );

        let mut adapter = FakeAdapter::passing();
        adapter.mutate_while_refused = true;
        assert_eq!(
            run(&mut adapter, &location),
            Err(ProofError::LockedModifyDataMismatch)
        );

        let mut adapter = FakeAdapter::passing();
        adapter.locked_modify_other = true;
        assert_eq!(
            run(&mut adapter, &location),
            Err(ProofError::LockedModifyVerificationFailed)
        );
    }

    #[test]
    fn failures_are_fail_closed() {
        let location = location();
        let mut adapter = FakeAdapter::passing();
        adapter.fail_suppression = true;
        assert_eq!(
            run(&mut adapter, &location),
            Err(ProofError::InteractionSuppressionFailed)
        );
        assert_eq!(adapter.events, ["ui-off"]);

        let mut adapter = FakeAdapter::passing();
        adapter.allowed = b"wrong synthetic data".to_vec();
        assert_eq!(
            run(&mut adapter, &location),
            Err(ProofError::AllowedReadMismatch)
        );

        for denial in [
            ReadFailure {
                kind: ReadFailureKind::ItemNotFound,
                data_returned: false,
            },
            ReadFailure {
                kind: ReadFailureKind::AccessDenied,
                data_returned: true,
            },
        ] {
            let mut adapter = FakeAdapter::passing();
            adapter.acl_denial = denial;
            assert_eq!(
                run(&mut adapter, &location),
                Err(ProofError::RestrictedReadWasNotDenied)
            );
            let mut adapter = FakeAdapter::passing();
            adapter.locked_denial = denial;
            assert_eq!(
                run(&mut adapter, &location),
                Err(ProofError::LockedReadWasNotDenied)
            );
        }
    }
}
