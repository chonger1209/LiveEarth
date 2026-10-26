use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    pub schema: u8,
    #[serde(default)]
    pub show_guide_on_start: bool,
    #[serde(default)]
    pub photo_limit: u32,
    pub grid: u32,
    pub zoom: u16,
    pub interval_minutes: u32,
    pub automatic: bool,
    pub apply_automatic: bool,
    pub beijing: bool,
    pub photos: PathBuf,
    pub last: Option<PathBuf>,
    #[serde(default = "yes")]
    pub minimize_to_tray: bool,
    #[serde(default = "yes")]
    pub close_to_tray: bool,
    #[serde(default = "yes", alias = "start_hidden")]
    pub start_minimized: bool,
    #[serde(default)]
    pub autostart: bool,
}
fn yes() -> bool {
    true
}
impl Preferences {
    pub fn capture_differs(&self, other: &Self) -> bool {
        self.photo_limit != other.photo_limit
            || self.grid != other.grid
            || self.zoom != other.zoom
            || self.interval_minutes != other.interval_minutes
            || self.apply_automatic != other.apply_automatic
            || self.beijing != other.beijing
            || self.photos != other.photos
    }
    pub fn new(data: &Path) -> Self {
        Self {
            schema: 4,
            show_guide_on_start: true,
            photo_limit: 99,
            grid: 4,
            zoom: 100,
            interval_minutes: 10,
            automatic: false,
            apply_automatic: true,
            beijing: true,
            photos: data.join("照片"),
            last: None,
            minimize_to_tray: true,
            close_to_tray: true,
            start_minimized: true,
            autostart: false,
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!([2, 3, 4].contains(&self.schema), "配置版本不兼容");
        ensure!(
            [1, 2, 4, 8, 16].contains(&self.grid)
                && (10..=200).contains(&self.zoom)
                && (1..=120).contains(&self.interval_minutes),
            "配置数值超出范围"
        );
        ensure!(self.photo_limit <= 1_000_000, "照片数量上限过大");
        ensure!(!self.photos.as_os_str().is_empty(), "照片目录不能为空");
        Ok(())
    }
}
pub fn validate_photo_directory(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "请填写照片目录的完整路径");
    fs::create_dir_all(path).context("无法创建照片目录")?;
    let mut probe = tempfile::NamedTempFile::new_in(path).context("照片目录无法写入")?;
    probe.write_all(b"LiveEarth directory check")?;
    probe.as_file().sync_all()?;
    Ok(())
}
fn parse(bytes: &[u8]) -> Result<Preferences> {
    let mut value: Preferences = serde_json::from_slice(bytes)?;
    value.validate()?;
    if value.schema == 2 {
        value.apply_automatic = true;
    }
    value.schema = 4;
    Ok(value)
}
pub fn load(directory: &Path) -> (Preferences, Option<String>) {
    let a = directory.join("settings.json");
    if !a.exists() && !directory.join("settings-B.json").exists() {
        return (Preferences::new(directory), None);
    }
    match fs::read(&a)
        .map_err(anyhow::Error::from)
        .and_then(|b| parse(&b))
    {
        Ok(value) => (value, None),
        Err(error) => match fs::read(directory.join("settings-B.json"))
            .map_err(anyhow::Error::from)
            .and_then(|b| parse(&b))
        {
            Ok(value) => (value, Some(format!("配置 A 读取失败，已恢复 B：{error}"))),
            Err(_) => (
                Preferences::new(directory),
                Some(format!("配置无法读取，使用默认值：{error}")),
            ),
        },
    }
}
fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("配置路径无父目录")?)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn save(directory: &Path, preferences: &Preferences) -> Result<()> {
    preferences.validate()?;
    fs::create_dir_all(directory)?;
    let path = directory.join("settings.json");
    let bytes = serde_json::to_vec_pretty(preferences)?;
    if let Ok(old) = fs::read(&path) {
        if old == bytes {
            return Ok(());
        }
        if parse(&old).is_ok() {
            replace(&directory.join("settings-B.json"), &old)?;
        }
    }
    replace(&path, &bytes)
}
