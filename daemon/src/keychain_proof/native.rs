use super::{
    AdapterError, FixtureDirectory, KeychainAdapter, ProofError, ReadFailure, ReadFailureKind,
    WriteFailure, WriteFailureKind,
};
use core_foundation_sys::array::{CFArrayCreate, CFArrayRef, kCFTypeArrayCallBacks};
use core_foundation_sys::base::{CFRelease, OSStatus, kCFAllocatorDefault};
use core_foundation_sys::string::{CFStringCreateWithCString, kCFStringEncodingUTF8};
use security_framework_sys::base::{
    SecAccessRef, SecKeychainAttribute, SecKeychainAttributeList, SecKeychainItemRef,
    SecKeychainRef, errSecAuthFailed, errSecItemNotFound, errSecSuccess,
};
use security_framework_sys::keychain::{
    SecKeychainAddGenericPassword, SecKeychainCreate, SecKeychainFindGenericPassword,
    SecKeychainSetUserInteractionAllowed,
};
use security_framework_sys::keychain_item::SecKeychainItemFreeContent;
use std::ffi::{CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::{ptr, slice};

const ERR_SEC_AUTH_FAILED: OSStatus = errSecAuthFailed;
const ERR_SEC_INTERACTION_NOT_ALLOWED: OSStatus = -25308;
const ERR_SEC_ITEM_NOT_FOUND: OSStatus = errSecItemNotFound;
const KEYCHAIN_UNLOCKED_STATUS: u32 = 1;
const GENERIC_PASSWORD_CLASS: u32 = u32::from_be_bytes(*b"genp");
const SERVICE_ATTRIBUTE: u32 = u32::from_be_bytes(*b"svce");
const ACCOUNT_ATTRIBUTE: u32 = u32::from_be_bytes(*b"acct");
const KEYCHAIN_PASSWORD: &[u8] = b"synthetic-quotabar-proof-keychain-password";
const ACCESS_DESCRIPTOR: &[u8] = b"quotabar-proof read-restricted item\0";

unsafe extern "C" {
    fn SecKeychainItemCreateFromContent(
        item_class: u32,
        attributes: *mut SecKeychainAttributeList,
        data_length: u32,
        data: *const c_void,
        keychain: SecKeychainRef,
        initial_access: SecAccessRef,
        item: *mut SecKeychainItemRef,
    ) -> OSStatus;
    fn SecKeychainLock(keychain: SecKeychainRef) -> OSStatus;
    fn SecKeychainUnlock(
        keychain: SecKeychainRef,
        password_length: u32,
        password: *const c_void,
        use_password: u8,
    ) -> OSStatus;
    fn SecKeychainGetStatus(keychain: SecKeychainRef, status: *mut u32) -> OSStatus;
    fn SecAccessCreate(
        descriptor: core_foundation_sys::string::CFStringRef,
        trusted_list: CFArrayRef,
        access: *mut SecAccessRef,
    ) -> OSStatus;
}

struct NativeFixture {
    keychain: SecKeychainRef,
    allowed_item: std::cell::Cell<SecKeychainItemRef>,
    restricted_item: std::cell::Cell<SecKeychainItemRef>,
}

impl Drop for NativeFixture {
    fn drop(&mut self) {
        // Release retained item references before their owning private Keychain.
        release_if_nonnull(self.allowed_item.get());
        release_if_nonnull(self.restricted_item.get());
        release_if_nonnull(self.keychain);
    }
}

struct NativeAdapter;

impl KeychainAdapter for NativeAdapter {
    type Fixture = NativeFixture;

    fn disable_user_interaction(&mut self) -> Result<(), AdapterError> {
        let status = unsafe { SecKeychainSetUserInteractionAllowed(0) };
        (status == errSecSuccess)
            .then_some(())
            .ok_or(AdapterError::OperationFailed)
    }

    fn create_fixture(&mut self, path: &Path) -> Result<Self::Fixture, AdapterError> {
        let path =
            CString::new(path.as_os_str().as_bytes()).map_err(|_| AdapterError::OperationFailed)?;
        let mut keychain = ptr::null_mut();
        let status = unsafe {
            SecKeychainCreate(
                path.as_ptr(),
                u32::try_from(KEYCHAIN_PASSWORD.len())
                    .map_err(|_| AdapterError::OperationFailed)?,
                KEYCHAIN_PASSWORD.as_ptr().cast(),
                0,
                ptr::null_mut(),
                &mut keychain,
            )
        };
        if status != errSecSuccess || keychain.is_null() {
            release_if_nonnull(keychain);
            return Err(AdapterError::OperationFailed);
        }
        Ok(NativeFixture {
            keychain,
            allowed_item: std::cell::Cell::new(ptr::null_mut()),
            restricted_item: std::cell::Cell::new(ptr::null_mut()),
        })
    }

    fn add_allowed(
        &mut self,
        fixture: &Self::Fixture,
        service: &str,
        account: &str,
        secret: &[u8],
    ) -> Result<(), AdapterError> {
        let mut item = ptr::null_mut();
        let status = unsafe {
            SecKeychainAddGenericPassword(
                fixture.keychain,
                to_u32(service.len())?,
                service.as_ptr().cast(),
                to_u32(account.len())?,
                account.as_ptr().cast(),
                to_u32(secret.len())?,
                secret.as_ptr().cast(),
                &mut item,
            )
        };
        if status != errSecSuccess || item.is_null() {
            release_if_nonnull(item);
            return Err(AdapterError::OperationFailed);
        }
        fixture.allowed_item.set(item);
        Ok(())
    }

    fn add_restricted(
        &mut self,
        fixture: &Self::Fixture,
        service: &str,
        account: &str,
        secret: &[u8],
    ) -> Result<(), AdapterError> {
        let service_length = to_u32(service.len())?;
        let account_length = to_u32(account.len())?;
        let secret_length = to_u32(secret.len())?;
        let descriptor = create_string(ACCESS_DESCRIPTOR)?;
        let trusted_list =
            unsafe { CFArrayCreate(kCFAllocatorDefault, ptr::null(), 0, &kCFTypeArrayCallBacks) };
        if trusted_list.is_null() {
            release_if_nonnull(descriptor);
            return Err(AdapterError::OperationFailed);
        }
        let mut access = ptr::null_mut();
        let access_status = unsafe { SecAccessCreate(descriptor, trusted_list, &mut access) };
        release_if_nonnull(descriptor);
        release_if_nonnull(trusted_list);
        if access_status != errSecSuccess || access.is_null() {
            release_if_nonnull(access);
            return Err(AdapterError::OperationFailed);
        }

        let mut service_bytes = service.as_bytes().to_vec();
        let mut account_bytes = account.as_bytes().to_vec();
        let mut attributes = [
            SecKeychainAttribute {
                tag: SERVICE_ATTRIBUTE,
                length: service_length,
                data: service_bytes.as_mut_ptr().cast(),
            },
            SecKeychainAttribute {
                tag: ACCOUNT_ATTRIBUTE,
                length: account_length,
                data: account_bytes.as_mut_ptr().cast(),
            },
        ];
        let mut attribute_list = SecKeychainAttributeList {
            count: 2,
            attr: attributes.as_mut_ptr(),
        };
        let mut item = ptr::null_mut();
        let status = unsafe {
            SecKeychainItemCreateFromContent(
                GENERIC_PASSWORD_CLASS,
                &mut attribute_list,
                secret_length,
                secret.as_ptr().cast(),
                fixture.keychain,
                access,
                &mut item,
            )
        };
        release_if_nonnull(access);
        if status != errSecSuccess || item.is_null() {
            release_if_nonnull(item);
            return Err(AdapterError::OperationFailed);
        }
        fixture.restricted_item.set(item);
        Ok(())
    }

    fn modify_control(
        &mut self,
        fixture: &Self::Fixture,
        replacement: &[u8],
    ) -> Result<(), WriteFailure> {
        let item = fixture.allowed_item.get();
        if item.is_null() {
            return Err(WriteFailure {
                kind: WriteFailureKind::Other,
            });
        }
        let status = unsafe {
            security_framework_sys::keychain_item::SecKeychainItemModifyAttributesAndData(
                item,
                ptr::null(),
                u32::try_from(replacement.len()).map_err(|_| WriteFailure {
                    kind: WriteFailureKind::Other,
                })?,
                replacement.as_ptr().cast(),
            )
        };
        if status == errSecSuccess {
            Ok(())
        } else {
            Err(write_failure(status))
        }
    }

    fn read(
        &mut self,
        fixture: &Self::Fixture,
        service: &str,
        account: &str,
    ) -> Result<Vec<u8>, ReadFailure> {
        let mut length = 0_u32;
        let mut data = ptr::null_mut();
        let mut item = ptr::null_mut();
        let status = unsafe {
            SecKeychainFindGenericPassword(
                fixture.keychain.cast(),
                to_u32(service.len()).map_err(|_| read_other(false))?,
                service.as_ptr().cast(),
                to_u32(account.len()).map_err(|_| read_other(false))?,
                account.as_ptr().cast(),
                &mut length,
                &mut data,
                &mut item,
            )
        };
        let data_returned = !data.is_null() || length != 0;
        let copied = if status == errSecSuccess && (length == 0 || !data.is_null()) {
            Some(if length == 0 {
                Vec::new()
            } else {
                unsafe { slice::from_raw_parts(data.cast::<u8>(), length as usize) }.to_vec()
            })
        } else {
            None
        };
        let free_status = if data.is_null() {
            errSecSuccess
        } else {
            unsafe { SecKeychainItemFreeContent(ptr::null_mut(), data) }
        };
        release_if_nonnull(item);
        if free_status != errSecSuccess {
            return Err(read_other(data_returned));
        }
        if let Some(data) = copied {
            return Ok(data);
        }
        Err(ReadFailure {
            kind: match status {
                ERR_SEC_INTERACTION_NOT_ALLOWED | ERR_SEC_AUTH_FAILED => {
                    ReadFailureKind::AccessDenied
                }
                ERR_SEC_ITEM_NOT_FOUND => ReadFailureKind::ItemNotFound,
                _ => ReadFailureKind::Other,
            },
            data_returned,
        })
    }

    fn lock_fixture(&mut self, fixture: &Self::Fixture) -> Result<(), AdapterError> {
        if unsafe { SecKeychainLock(fixture.keychain) } != errSecSuccess
            || !self.fixture_is_locked(fixture)?
        {
            return Err(AdapterError::OperationFailed);
        }
        Ok(())
    }

    fn fixture_is_locked(&mut self, fixture: &Self::Fixture) -> Result<bool, AdapterError> {
        let mut status = 0_u32;
        if unsafe { SecKeychainGetStatus(fixture.keychain, &mut status) } != errSecSuccess {
            return Err(AdapterError::OperationFailed);
        }
        Ok(status & KEYCHAIN_UNLOCKED_STATUS == 0)
    }

    fn unlock_fixture(&mut self, fixture: &Self::Fixture) -> Result<(), AdapterError> {
        let status = unsafe {
            SecKeychainUnlock(
                fixture.keychain,
                to_u32(KEYCHAIN_PASSWORD.len())?,
                KEYCHAIN_PASSWORD.as_ptr().cast(),
                1,
            )
        };
        (status == errSecSuccess)
            .then_some(())
            .ok_or(AdapterError::OperationFailed)
    }
}

fn to_u32(value: usize) -> Result<u32, AdapterError> {
    u32::try_from(value).map_err(|_| AdapterError::OperationFailed)
}

fn create_string(bytes: &[u8]) -> Result<core_foundation_sys::string::CFStringRef, AdapterError> {
    let string = unsafe {
        CFStringCreateWithCString(
            kCFAllocatorDefault,
            bytes.as_ptr().cast(),
            kCFStringEncodingUTF8,
        )
    };
    if string.is_null() {
        Err(AdapterError::OperationFailed)
    } else {
        Ok(string)
    }
}

fn read_other(data_returned: bool) -> ReadFailure {
    ReadFailure {
        kind: ReadFailureKind::Other,
        data_returned,
    }
}

fn write_failure(status: OSStatus) -> WriteFailure {
    WriteFailure {
        kind: if status == ERR_SEC_AUTH_FAILED || status == ERR_SEC_INTERACTION_NOT_ALLOWED {
            WriteFailureKind::RecognizedRefusal
        } else {
            WriteFailureKind::Other
        },
    }
}

fn release_if_nonnull<T>(reference: *const T) {
    if !reference.is_null() {
        unsafe { CFRelease(reference.cast::<c_void>()) };
    }
}

pub fn run() -> Result<(), ProofError> {
    let location = FixtureDirectory::create().map_err(|_| ProofError::UnsafeFixturePath)?;
    let mut adapter = NativeAdapter;
    super::run(&mut adapter, &location)
}
