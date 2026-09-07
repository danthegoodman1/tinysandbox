#![deny(unsafe_code)]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

#[cfg(feature = "js")]
mod js;
pub mod prompts;
pub mod sandbox;
pub mod shell;
pub mod vfs;

mod wasm;
