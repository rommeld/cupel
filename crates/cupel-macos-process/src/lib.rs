#![cfg(target_os = "macos")]

#[allow(unsafe_code)]
mod process_identity;

pub use process_identity::ProcessIdentity;
