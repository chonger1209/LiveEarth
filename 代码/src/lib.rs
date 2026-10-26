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
    format!("V{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests;
