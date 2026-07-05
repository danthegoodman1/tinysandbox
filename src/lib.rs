#![deny(unsafe_code)]

#[cfg(feature = "js")]
pub mod js;
pub mod machine;
pub mod shell;
pub mod vfs;
