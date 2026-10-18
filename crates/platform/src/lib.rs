//! pdfcraft-platform — L7 operating-system services shared by the desktop app (`ui-egui`) and the
//! automation tools (`automation`), which sit on the same layer and can't depend on each other.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod desktop_theme;
#[cfg(not(target_arch = "wasm32"))]
mod office;
pub mod staging;

#[cfg(not(target_arch = "wasm32"))]
pub use office::{is_office_document, office_to_pdf, office_to_pdf_internal};

#[cfg(target_arch = "wasm32")]
pub fn is_office_document(_: &str) -> bool {
    false
}

#[cfg(target_arch = "wasm32")]
pub fn office_to_pdf(name: &str, _: &[u8]) -> Result<Vec<u8>, String> {
    Err(format!("{name}: Office conversion is unavailable in the web build"))
}

#[cfg(target_arch = "wasm32")]
pub fn office_to_pdf_internal(_: &std::path::Path, _: &std::path::Path) -> Result<(), String> {
    Err("Office conversion is unavailable in the web build".into())
}
