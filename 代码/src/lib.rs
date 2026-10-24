mod archive_index;
pub mod desktop;
pub mod domain;
pub mod engine;
pub mod imaging;
pub mod journal;
pub mod net;
pub mod platform;
pub mod service;
pub mod settings;
pub mod storage;
mod webp_archive;

pub const APP_NAME: &str = "LiveEarth";
pub const RELEASES_SITE: &str = "https://github.com/chonger1209/LiveEarth/releases/latest";
pub const SOURCE_SITE: &str = "https://himawari8.nict.go.jp/";
pub fn display_version() -> String {
    // Cargo 0.m.p maps to release 10*m+p; build.rs rejects ambiguous patch values.
    let minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap();
    let patch: u32 = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap();
    format!("V2.{:03}", minor * 10 + patch)
}

#[cfg(test)]
mod tests;
