pub mod claude;
pub mod codex;
pub mod credential_file;
pub mod cursor;
pub(crate) mod http;
pub mod keychain;
// Composed with the other phase-two probes only after every provider is ready.
#[allow(dead_code)]
pub mod muse;
pub mod opencode;
pub mod opencode_http;
