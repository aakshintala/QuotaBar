#[cfg(target_os = "macos")]
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;

/// The only two Keychain records this reader may address. The mapping is exact
/// and private; callers cannot supply arbitrary service or account names.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum KeychainTarget {
    CursorAccess,
    Muse,
}

/// Validated effective OS login used only for Claude's fixed Keychain item.
/// It is not formatted or exposed through provider-facing APIs.
#[derive(Clone, Eq, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct ClaudeAccount(String);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl ClaudeAccount {
    pub(crate) fn from_login_name(value: String) -> Option<Self> {
        (!value.is_empty() && value.len() <= 64 * 1024 && !value.chars().any(char::is_control))
            .then_some(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum ClaudeWriteOutcome {
    Changed(SecretBytes),
    Updated(SecretBytes),
}

/// Claude-only capability. It accepts a validated account, never a service
/// name, and does not expose writes through the Cursor/Muse reader interface.
pub(crate) trait ClaudeKeychainStore: Send + Sync {
    fn read_claude(&self, account: &ClaudeAccount) -> Result<SecretBytes, KeychainReadError>;

    fn replace_claude_if_unchanged(
        &self,
        account: &ClaudeAccount,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<ClaudeWriteOutcome, KeychainReadError>;
}

impl KeychainTarget {
    #[cfg(any(target_os = "macos", test))]
    fn service_account(self) -> (&'static str, &'static str) {
        match self {
            Self::CursorAccess => ("cursor-access-token", "cursor-user"),
            Self::Muse => ("ai.meta.dev.credentials", "meta"),
        }
    }
}

/// Safe errors carry neither native status text nor credential content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeychainReadError {
    InteractionUnavailable,
    ItemNotFound,
    CredentialTooLarge,
    ReadFailed,
}

/// Opaque Keychain secret bytes. The buffer is cleared when this value drops;
/// it intentionally has no Debug, serialization or string conversion API.
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    /// Wrap secret bytes without exposing them through formatting or
    /// serialization. The owned buffer is cleared when the value drops.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Narrow provider-facing seam: ordinary tests supply a fake and never enter
/// the native adapter.
pub trait CredentialReader {
    fn read(&self, target: KeychainTarget) -> Result<SecretBytes, KeychainReadError>;
}

#[cfg(target_os = "macos")]
pub struct NativeKeychainReader;

#[cfg(target_os = "macos")]
pub(crate) struct NativeClaudeKeychainStore;

#[cfg(not(target_os = "macos"))]
pub(crate) struct NativeClaudeKeychainStore;

#[cfg(target_os = "macos")]
impl CredentialReader for NativeKeychainReader {
    fn read(&self, target: KeychainTarget) -> Result<SecretBytes, KeychainReadError> {
        native::read(target)
    }
}

#[cfg(target_os = "macos")]
impl ClaudeKeychainStore for NativeClaudeKeychainStore {
    fn read_claude(&self, account: &ClaudeAccount) -> Result<SecretBytes, KeychainReadError> {
        native::read_claude(account)
    }

    fn replace_claude_if_unchanged(
        &self,
        account: &ClaudeAccount,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<ClaudeWriteOutcome, KeychainReadError> {
        native::replace_claude_if_unchanged(account, expected, replacement)
    }
}

#[cfg(not(target_os = "macos"))]
impl ClaudeKeychainStore for NativeClaudeKeychainStore {
    fn read_claude(&self, _account: &ClaudeAccount) -> Result<SecretBytes, KeychainReadError> {
        Err(KeychainReadError::InteractionUnavailable)
    }

    fn replace_claude_if_unchanged(
        &self,
        _account: &ClaudeAccount,
        _expected: &[u8],
        _replacement: &[u8],
    ) -> Result<ClaudeWriteOutcome, KeychainReadError> {
        Err(KeychainReadError::InteractionUnavailable)
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::{
        ClaudeAccount, ClaudeWriteOutcome, KeychainReadError, KeychainTarget, MAX_CREDENTIAL_BYTES,
        SecretBytes,
    };
    use core_foundation_sys::base::{CFRelease, OSStatus};
    use security_framework_sys::base::{SecKeychainItemRef, errSecItemNotFound, errSecSuccess};
    use security_framework_sys::keychain::{
        SecKeychainFindGenericPassword, SecKeychainSetUserInteractionAllowed,
    };
    use security_framework_sys::keychain_item::SecKeychainItemFreeContent;
    use std::ffi::{c_char, c_void};
    use std::ptr;
    use std::slice;
    use std::sync::OnceLock;

    const ERR_SEC_AUTH_FAILED: OSStatus = -25293;
    const ERR_SEC_INTERACTION_NOT_ALLOWED: OSStatus = -25308;
    const CLAUDE_SERVICE: &str = "Claude Code-credentials";
    const MAX_CLAUDE_CREDENTIAL_BYTES: usize = 1024 * 1024;
    static INTERACTION_DISABLED: OnceLock<bool> = OnceLock::new();

    fn ensure_interaction_disabled_with(
        cache: &OnceLock<bool>,
        set_interaction_allowed: impl FnOnce() -> bool,
    ) -> Result<(), KeychainReadError> {
        if *cache.get_or_init(set_interaction_allowed) {
            Ok(())
        } else {
            Err(KeychainReadError::InteractionUnavailable)
        }
    }

    fn ensure_interaction_disabled() -> Result<(), KeychainReadError> {
        ensure_interaction_disabled_with(&INTERACTION_DISABLED, || {
            // This checked process-lifetime policy is the first native
            // credential API operation. It is never restored or re-enabled.
            unsafe { SecKeychainSetUserInteractionAllowed(0) == errSecSuccess }
        })
    }

    struct NativeFind {
        status: OSStatus,
        length: u32,
        data: *mut c_void,
        item: SecKeychainItemRef,
    }

    trait ClaudeNativeOperations {
        fn set_interaction_disabled(&self) -> bool;
        fn find(&self, service: &str, account: &str, request_data: bool) -> NativeFind;
        unsafe fn copy_data(&self, data: *mut c_void, length: usize) -> Option<Vec<u8>>;
        unsafe fn free_content(&self, data: *mut c_void) -> OSStatus;
        unsafe fn release_item(&self, item: SecKeychainItemRef);
        unsafe fn modify(&self, item: SecKeychainItemRef, replacement: &[u8]) -> OSStatus;
    }

    fn find_with_operations(
        operations: &impl ClaudeNativeOperations,
        service: &str,
        account: &str,
        request_data: bool,
    ) -> NativeFind {
        operations.find(service, account, request_data)
    }

    fn verify_metadata_with_operations(
        operations: &impl ClaudeNativeOperations,
        account: &ClaudeAccount,
    ) -> Result<(), KeychainReadError> {
        let found = find_with_operations(operations, CLAUDE_SERVICE, account.as_str(), false);
        if !found.item.is_null() {
            unsafe { operations.release_item(found.item) };
        }
        if found.status == errSecSuccess && !found.item.is_null() {
            Ok(())
        } else {
            Err(map_status(found.status))
        }
    }

    fn read_claude_item_with_operations(
        operations: &impl ClaudeNativeOperations,
        interaction_cache: &OnceLock<bool>,
        account: &ClaudeAccount,
    ) -> Result<(SecretBytes, SecKeychainItemRef), KeychainReadError> {
        ensure_interaction_disabled_with(interaction_cache, || {
            operations.set_interaction_disabled()
        })?;
        verify_metadata_with_operations(operations, account)?;
        let found = find_with_operations(operations, CLAUDE_SERVICE, account.as_str(), true);
        let has_data = !found.data.is_null() || found.length != 0;
        let mut bytes = if found.status == errSecSuccess {
            if found.length as usize > MAX_CLAUDE_CREDENTIAL_BYTES {
                None
            } else if found.length == 0 {
                Some(Vec::new())
            } else if found.data.is_null() {
                None
            } else {
                unsafe { operations.copy_data(found.data, found.length as usize) }
            }
        } else {
            None
        };
        let free_status = if found.data.is_null() {
            errSecSuccess
        } else {
            unsafe { operations.free_content(found.data) }
        };
        if free_status != errSecSuccess {
            if let Some(bytes) = bytes.as_mut() {
                bytes.fill(0);
            }
            if !found.item.is_null() {
                unsafe { operations.release_item(found.item) };
            }
            return Err(KeychainReadError::ReadFailed);
        }
        if found.status != errSecSuccess {
            if !found.item.is_null() {
                unsafe { operations.release_item(found.item) };
            }
            return if has_data {
                Err(KeychainReadError::ReadFailed)
            } else {
                Err(map_status(found.status))
            };
        }
        let Some(bytes) = bytes else {
            if !found.item.is_null() {
                unsafe { operations.release_item(found.item) };
            }
            return Err(if found.length as usize > MAX_CLAUDE_CREDENTIAL_BYTES {
                KeychainReadError::CredentialTooLarge
            } else {
                KeychainReadError::ReadFailed
            });
        };
        if found.item.is_null() {
            let mut bytes = bytes;
            bytes.fill(0);
            return Err(KeychainReadError::ReadFailed);
        }
        Ok((SecretBytes::from_bytes(bytes), found.item))
    }

    fn read_claude_with_operations(
        operations: &impl ClaudeNativeOperations,
        interaction_cache: &OnceLock<bool>,
        account: &ClaudeAccount,
    ) -> Result<SecretBytes, KeychainReadError> {
        let (bytes, item) =
            read_claude_item_with_operations(operations, interaction_cache, account)?;
        unsafe { operations.release_item(item) };
        Ok(bytes)
    }

    fn replace_claude_with_operations(
        operations: &impl ClaudeNativeOperations,
        interaction_cache: &OnceLock<bool>,
        account: &ClaudeAccount,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<ClaudeWriteOutcome, KeychainReadError> {
        if replacement.len() > MAX_CLAUDE_CREDENTIAL_BYTES {
            return Err(KeychainReadError::CredentialTooLarge);
        }
        let (current, item) =
            read_claude_item_with_operations(operations, interaction_cache, account)?;
        if current.expose() != expected {
            unsafe { operations.release_item(item) };
            return Ok(ClaudeWriteOutcome::Changed(current));
        }
        let status = unsafe { operations.modify(item, replacement) };
        unsafe { operations.release_item(item) };
        if status != errSecSuccess {
            // The API error does not prove the write left the item unchanged.
            // Treat every modify error as terminal; never rewrite from a reload.
            return Err(KeychainReadError::ReadFailed);
        }
        let actual = read_claude_with_operations(operations, interaction_cache, account)
            .map_err(|_| KeychainReadError::ReadFailed)?;
        Ok(ClaudeWriteOutcome::Updated(actual))
    }

    struct SystemClaudeNativeOperations;

    impl ClaudeNativeOperations for SystemClaudeNativeOperations {
        fn set_interaction_disabled(&self) -> bool {
            unsafe { SecKeychainSetUserInteractionAllowed(0) == errSecSuccess }
        }

        fn find(&self, service: &str, account: &str, request_data: bool) -> NativeFind {
            let service_length = u32::try_from(service.len()).unwrap_or(u32::MAX);
            let account_length = u32::try_from(account.len()).unwrap_or(u32::MAX);
            let mut length = 0_u32;
            let mut data = ptr::null_mut();
            let mut item: SecKeychainItemRef = ptr::null_mut();
            let status = unsafe {
                SecKeychainFindGenericPassword(
                    ptr::null_mut(),
                    service_length,
                    service.as_ptr().cast::<c_char>(),
                    account_length,
                    account.as_ptr().cast::<c_char>(),
                    if request_data {
                        &mut length
                    } else {
                        ptr::null_mut()
                    },
                    if request_data {
                        &mut data
                    } else {
                        ptr::null_mut()
                    },
                    &mut item,
                )
            };
            NativeFind {
                status,
                length,
                data,
                item,
            }
        }

        unsafe fn copy_data(&self, data: *mut c_void, length: usize) -> Option<Vec<u8>> {
            Some(unsafe { slice::from_raw_parts(data.cast::<u8>(), length) }.to_vec())
        }

        unsafe fn free_content(&self, data: *mut c_void) -> OSStatus {
            unsafe { SecKeychainItemFreeContent(ptr::null_mut(), data) }
        }

        unsafe fn release_item(&self, item: SecKeychainItemRef) {
            unsafe { CFRelease(item.cast::<c_void>()) };
        }

        unsafe fn modify(&self, item: SecKeychainItemRef, replacement: &[u8]) -> OSStatus {
            let Ok(length) = u32::try_from(replacement.len()) else {
                return -1;
            };
            unsafe {
                security_framework_sys::keychain_item::SecKeychainItemModifyAttributesAndData(
                    item,
                    ptr::null(),
                    length,
                    replacement.as_ptr().cast::<c_void>(),
                )
            }
        }
    }

    pub(super) fn read(target: KeychainTarget) -> Result<SecretBytes, KeychainReadError> {
        ensure_interaction_disabled()?;
        let (service, account) = target.service_account();
        let service_length =
            u32::try_from(service.len()).map_err(|_| KeychainReadError::ReadFailed)?;
        let account_length =
            u32::try_from(account.len()).map_err(|_| KeychainReadError::ReadFailed)?;
        let mut length = 0_u32;
        let mut data = ptr::null_mut();
        let mut item: SecKeychainItemRef = ptr::null_mut();
        let status = unsafe {
            // Null selects the default search list, but the query is restricted
            // to this one compile-time service/account pair; it never enumerates.
            SecKeychainFindGenericPassword(
                ptr::null_mut(),
                service_length,
                service.as_ptr().cast::<c_char>(),
                account_length,
                account.as_ptr().cast::<c_char>(),
                &mut length,
                &mut data,
                &mut item,
            )
        };
        let has_data = !data.is_null() || length != 0;
        let mut copied = if status == errSecSuccess {
            if length as usize > MAX_CREDENTIAL_BYTES {
                None
            } else if length == 0 {
                Some(Vec::new())
            } else if data.is_null() {
                None
            } else {
                Some(unsafe { slice::from_raw_parts(data.cast::<u8>(), length as usize) }.to_vec())
            }
        } else {
            None
        };

        // Free native content and the returned item reference on every path,
        // including oversized, malformed and failed reads.
        let free_status = if data.is_null() {
            errSecSuccess
        } else {
            unsafe { SecKeychainItemFreeContent(ptr::null_mut(), data) }
        };
        if !item.is_null() {
            unsafe { CFRelease(item.cast::<c_void>()) };
        }
        if free_status != errSecSuccess {
            if let Some(bytes) = copied.as_mut() {
                bytes.fill(0);
            }
            return Err(KeychainReadError::ReadFailed);
        }

        if status == errSecSuccess {
            return copied.take().map(SecretBytes::from_bytes).ok_or(
                if length as usize > MAX_CREDENTIAL_BYTES {
                    KeychainReadError::CredentialTooLarge
                } else {
                    KeychainReadError::ReadFailed
                },
            );
        }
        if has_data {
            return Err(KeychainReadError::ReadFailed);
        }
        if status == errSecItemNotFound {
            Err(KeychainReadError::ItemNotFound)
        } else if status == ERR_SEC_AUTH_FAILED || status == ERR_SEC_INTERACTION_NOT_ALLOWED {
            Err(KeychainReadError::InteractionUnavailable)
        } else {
            Err(KeychainReadError::ReadFailed)
        }
    }

    fn map_status(status: OSStatus) -> KeychainReadError {
        if status == errSecItemNotFound {
            KeychainReadError::ItemNotFound
        } else if status == ERR_SEC_AUTH_FAILED || status == ERR_SEC_INTERACTION_NOT_ALLOWED {
            KeychainReadError::InteractionUnavailable
        } else {
            KeychainReadError::ReadFailed
        }
    }

    pub(super) fn read_claude(account: &ClaudeAccount) -> Result<SecretBytes, KeychainReadError> {
        read_claude_with_operations(
            &SystemClaudeNativeOperations,
            &INTERACTION_DISABLED,
            account,
        )
    }

    pub(super) fn replace_claude_if_unchanged(
        account: &ClaudeAccount,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<ClaudeWriteOutcome, KeychainReadError> {
        replace_claude_with_operations(
            &SystemClaudeNativeOperations,
            &INTERACTION_DISABLED,
            account,
            expected,
            replacement,
        )
    }

    #[cfg(test)]
    mod adapter_tests {
        use super::*;
        use std::collections::HashMap;
        use std::sync::Mutex;

        const DENIED: OSStatus = -25293;

        struct FakeOperations {
            setter_ok: bool,
            metadata_status: OSStatus,
            secret_status: OSStatus,
            modify_status: OSStatus,
            bytes: Mutex<Vec<u8>>,
            reported_length: Option<u32>,
            calls: Mutex<Vec<&'static str>>,
            allocations: Mutex<HashMap<usize, usize>>,
            content_frees: Mutex<usize>,
            item_releases: Mutex<usize>,
            modifications: Mutex<usize>,
            mutate_before_modify_error: bool,
        }

        impl FakeOperations {
            fn new(bytes: &[u8]) -> Self {
                Self {
                    setter_ok: true,
                    metadata_status: errSecSuccess,
                    secret_status: errSecSuccess,
                    modify_status: errSecSuccess,
                    bytes: Mutex::new(bytes.to_vec()),
                    reported_length: None,
                    calls: Mutex::new(Vec::new()),
                    allocations: Mutex::new(HashMap::new()),
                    content_frees: Mutex::new(0),
                    item_releases: Mutex::new(0),
                    modifications: Mutex::new(0),
                    mutate_before_modify_error: false,
                }
            }

            fn item(&self) -> SecKeychainItemRef {
                let pointer: SecKeychainItemRef = Box::into_raw(Box::new(1_u8)).cast();
                pointer
            }

            fn content(&self, bytes: &[u8]) -> *mut c_void {
                let boxed = bytes.to_vec().into_boxed_slice();
                let pointer = Box::into_raw(boxed) as *mut u8 as *mut c_void;
                self.allocations
                    .lock()
                    .unwrap()
                    .insert(pointer as usize, bytes.len());
                pointer
            }
        }

        impl ClaudeNativeOperations for FakeOperations {
            fn set_interaction_disabled(&self) -> bool {
                self.calls.lock().unwrap().push("suppress");
                self.setter_ok
            }

            fn find(&self, service: &str, account: &str, request_data: bool) -> NativeFind {
                assert_eq!(service, CLAUDE_SERVICE);
                assert_eq!(account, "synthetic-login");
                if request_data {
                    self.calls.lock().unwrap().push("secret-query");
                    let bytes = self.bytes.lock().unwrap().clone();
                    let data = self.content(&bytes);
                    NativeFind {
                        status: self.secret_status,
                        length: self.reported_length.unwrap_or(bytes.len() as u32),
                        data,
                        item: self.item(),
                    }
                } else {
                    self.calls.lock().unwrap().push("metadata-null-outputs");
                    NativeFind {
                        status: self.metadata_status,
                        length: 0,
                        data: ptr::null_mut(),
                        item: self.item(),
                    }
                }
            }

            unsafe fn copy_data(&self, data: *mut c_void, length: usize) -> Option<Vec<u8>> {
                Some(unsafe { slice::from_raw_parts(data.cast::<u8>(), length) }.to_vec())
            }

            unsafe fn free_content(&self, data: *mut c_void) -> OSStatus {
                let length = self
                    .allocations
                    .lock()
                    .unwrap()
                    .remove(&(data as usize))
                    .unwrap();
                let slice = ptr::slice_from_raw_parts_mut(data.cast::<u8>(), length);
                drop(unsafe { Box::from_raw(slice) });
                *self.content_frees.lock().unwrap() += 1;
                errSecSuccess
            }

            unsafe fn release_item(&self, item: SecKeychainItemRef) {
                drop(unsafe { Box::from_raw(item.cast::<u8>()) });
                *self.item_releases.lock().unwrap() += 1;
            }

            unsafe fn modify(&self, _item: SecKeychainItemRef, replacement: &[u8]) -> OSStatus {
                *self.modifications.lock().unwrap() += 1;
                if self.modify_status == errSecSuccess || self.mutate_before_modify_error {
                    *self.bytes.lock().unwrap() = replacement.to_vec();
                }
                self.modify_status
            }
        }

        fn account() -> ClaudeAccount {
            ClaudeAccount::from_login_name("synthetic-login".to_owned()).unwrap()
        }

        #[test]
        fn suppression_failure_precedes_every_lookup_and_write() {
            let mut operations = FakeOperations::new(b"old");
            operations.setter_ok = false;
            let cache = OnceLock::new();
            assert_eq!(
                replace_claude_with_operations(&operations, &cache, &account(), b"old", b"new")
                    .err()
                    .unwrap(),
                KeychainReadError::InteractionUnavailable
            );
            assert_eq!(*operations.calls.lock().unwrap(), ["suppress"]);
            assert_eq!(*operations.modifications.lock().unwrap(), 0);
        }

        #[test]
        fn metadata_failure_stops_secret_query_and_releases_metadata_reference() {
            let mut operations = FakeOperations::new(b"old");
            operations.metadata_status = DENIED;
            let cache = OnceLock::new();
            assert_eq!(
                read_claude_with_operations(&operations, &cache, &account())
                    .err()
                    .unwrap(),
                KeychainReadError::InteractionUnavailable
            );
            assert_eq!(
                *operations.calls.lock().unwrap(),
                ["suppress", "metadata-null-outputs"]
            );
            assert_eq!(*operations.item_releases.lock().unwrap(), 1);
            assert_eq!(*operations.content_frees.lock().unwrap(), 0);
        }

        #[test]
        fn locked_secret_read_stops_before_modify_and_cleans_native_outputs() {
            let mut operations = FakeOperations::new(b"old");
            operations.secret_status = DENIED;
            assert_eq!(
                replace_claude_with_operations(
                    &operations,
                    &OnceLock::new(),
                    &account(),
                    b"old",
                    b"new"
                )
                .err()
                .unwrap(),
                KeychainReadError::ReadFailed
            );
            assert_eq!(*operations.bytes.lock().unwrap(), b"old");
            assert_eq!(*operations.modifications.lock().unwrap(), 0);
            assert_eq!(*operations.content_frees.lock().unwrap(), 1);
            assert_eq!(*operations.item_releases.lock().unwrap(), 2);
            assert!(operations.allocations.lock().unwrap().is_empty());
        }

        #[test]
        fn oversized_secret_is_not_copied_and_native_content_and_items_are_freed() {
            let mut operations = FakeOperations::new(b"x");
            operations.reported_length = Some((MAX_CLAUDE_CREDENTIAL_BYTES + 1) as u32);
            let cache = OnceLock::new();
            assert_eq!(
                read_claude_with_operations(&operations, &cache, &account())
                    .err()
                    .unwrap(),
                KeychainReadError::CredentialTooLarge
            );
            assert_eq!(*operations.content_frees.lock().unwrap(), 1);
            assert_eq!(*operations.item_releases.lock().unwrap(), 2);
            assert!(operations.allocations.lock().unwrap().is_empty());
        }

        #[test]
        fn denied_and_missing_updates_never_mutate_or_create_records() {
            let mut denied = FakeOperations::new(b"old");
            denied.modify_status = DENIED;
            assert_eq!(
                replace_claude_with_operations(
                    &denied,
                    &OnceLock::new(),
                    &account(),
                    b"old",
                    b"new"
                )
                .err()
                .unwrap(),
                KeychainReadError::ReadFailed
            );
            assert_eq!(*denied.bytes.lock().unwrap(), b"old");
            assert_eq!(*denied.modifications.lock().unwrap(), 1);
            assert_eq!(*denied.item_releases.lock().unwrap(), 2);
            assert_eq!(*denied.content_frees.lock().unwrap(), 1);

            let mut uncertain = FakeOperations::new(b"old");
            uncertain.modify_status = DENIED;
            uncertain.mutate_before_modify_error = true;
            assert_eq!(
                replace_claude_with_operations(
                    &uncertain,
                    &OnceLock::new(),
                    &account(),
                    b"old",
                    b"possibly-committed"
                )
                .err()
                .unwrap(),
                KeychainReadError::ReadFailed
            );
            assert_eq!(*uncertain.bytes.lock().unwrap(), b"possibly-committed");
            assert_eq!(*uncertain.modifications.lock().unwrap(), 1);

            let mut missing = FakeOperations::new(b"old");
            missing.metadata_status = errSecItemNotFound;
            assert_eq!(
                replace_claude_with_operations(
                    &missing,
                    &OnceLock::new(),
                    &account(),
                    b"old",
                    b"new"
                )
                .err()
                .unwrap(),
                KeychainReadError::ItemNotFound
            );
            assert_eq!(*missing.bytes.lock().unwrap(), b"old");
            assert_eq!(*missing.modifications.lock().unwrap(), 0);
        }

        #[test]
        fn status_mapping_and_replacement_bounds_are_fail_closed() {
            assert_eq!(
                map_status(errSecItemNotFound),
                KeychainReadError::ItemNotFound
            );
            assert_eq!(
                map_status(DENIED),
                KeychainReadError::InteractionUnavailable
            );
            assert_eq!(map_status(-50), KeychainReadError::ReadFailed);

            let oversized = FakeOperations::new(b"old");
            assert_eq!(
                replace_claude_with_operations(
                    &oversized,
                    &OnceLock::new(),
                    &account(),
                    b"old",
                    &vec![b'x'; MAX_CLAUDE_CREDENTIAL_BYTES + 1]
                )
                .err()
                .unwrap(),
                KeychainReadError::CredentialTooLarge
            );
            assert!(oversized.calls.lock().unwrap().is_empty());
            assert_eq!(*oversized.modifications.lock().unwrap(), 0);
        }

        #[test]
        fn changed_snapshot_skips_modify_and_success_uses_exact_readback() {
            let changed = FakeOperations::new(b"cli-replacement");
            let outcome = replace_claude_with_operations(
                &changed,
                &OnceLock::new(),
                &account(),
                b"stale",
                b"should-not-write",
            )
            .unwrap();
            assert!(matches!(outcome, ClaudeWriteOutcome::Changed(_)));
            assert_eq!(*changed.modifications.lock().unwrap(), 0);

            let success = FakeOperations::new(b"old");
            let outcome = replace_claude_with_operations(
                &success,
                &OnceLock::new(),
                &account(),
                b"old",
                b"new",
            )
            .unwrap();
            assert!(
                matches!(outcome, ClaudeWriteOutcome::Updated(actual) if actual.expose() == b"new")
            );
            assert_eq!(*success.modifications.lock().unwrap(), 1);
            assert!(success.allocations.lock().unwrap().is_empty());
            assert_eq!(
                *success.calls.lock().unwrap(),
                [
                    "suppress",
                    "metadata-null-outputs",
                    "secret-query",
                    "metadata-null-outputs",
                    "secret-query"
                ]
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CredentialReader, KeychainReadError, KeychainTarget, SecretBytes};

    struct FakeReader;

    impl CredentialReader for FakeReader {
        fn read(&self, target: KeychainTarget) -> Result<SecretBytes, KeychainReadError> {
            let value = match target {
                KeychainTarget::CursorAccess => b"synthetic-cursor-token".as_slice(),
                KeychainTarget::Muse => b"synthetic-muse-token".as_slice(),
            };
            Ok(SecretBytes::from_bytes(value.to_vec()))
        }
    }

    #[test]
    fn typed_targets_map_only_to_the_two_approved_service_account_pairs() {
        assert_eq!(
            KeychainTarget::CursorAccess.service_account(),
            ("cursor-access-token", "cursor-user")
        );
        assert_eq!(
            KeychainTarget::Muse.service_account(),
            ("ai.meta.dev.credentials", "meta")
        );
    }

    #[test]
    fn ordinary_provider_tests_use_the_fake_reader_seam() {
        let reader = FakeReader;
        assert_eq!(
            reader.read(KeychainTarget::CursorAccess).unwrap().expose(),
            b"synthetic-cursor-token"
        );
        assert_eq!(
            reader.read(KeychainTarget::Muse).unwrap().expose(),
            b"synthetic-muse-token"
        );
    }
}
