//! The native and browser converters share this parsing and rendering engine.
#[macro_use]
extern crate lopdf;

mod converter;
pub mod pdf;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[cfg(not(target_arch = "wasm32"))]
pub use converter::run_cli;
pub use converter::{Converter, OutputFormat, PdfBackground, PdfBackgrounds};
