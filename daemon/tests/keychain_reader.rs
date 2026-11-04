use quotabard::providers::keychain::{
    CredentialReader, KeychainReadError, KeychainTarget, SecretBytes,
};

struct FakeReader;

impl CredentialReader for FakeReader {
    fn read(&self, target: KeychainTarget) -> Result<SecretBytes, KeychainReadError> {
        let synthetic = match target {
            KeychainTarget::CursorAccess => b"synthetic-cursor-token".as_slice(),
            KeychainTarget::Muse => b"synthetic-muse-token".as_slice(),
        };
        Ok(SecretBytes::from_bytes(synthetic.to_vec()))
    }
}

#[test]
fn external_fake_reader_constructs_successful_opaque_bytes_for_both_targets() {
    let reader = FakeReader;
    for (target, expected) in [
        (
            KeychainTarget::CursorAccess,
            b"synthetic-cursor-token".as_slice(),
        ),
        (KeychainTarget::Muse, b"synthetic-muse-token".as_slice()),
    ] {
        let secret = reader
            .read(target)
            .expect("synthetic credential is available");
        assert_eq!(secret.expose(), expected);
    }
}
