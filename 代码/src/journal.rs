//! Daily UTF-8 logs next to the photos. No long-lived handle: users may delete
//! a log while the application runs; the next event recreates today's file.
use chrono::{DateTime, FixedOffset, Utc};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

pub fn beijing(time: DateTime<Utc>) -> DateTime<FixedOffset> {
    time.with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
}
pub fn line(time: DateTime<Utc>, message: &str, error: bool) -> String {
    format!(
        "{} [{}] {}",
        beijing(time).format("%Y-%m-%d %H:%M:%S"),
        if error { "错误" } else { "信息" },
        message.replace(['\r', '\n'], " ")
    )
}
pub struct DailyLog {
    directory: PathBuf,
}
impl DailyLog {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn set_directory(&mut self, path: PathBuf) {
        self.directory = path;
    }
    pub fn append(&self, time: DateTime<Utc>, record: &str) -> io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let path = self
            .directory
            .join(format!("运行记录-{}.log", beijing(time).format("%Y-%m-%d")));
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(format!("{record}\r\n").as_bytes())?;
        file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    #[test]
    fn midnight_append_restart_delete_and_directory_change() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("photo.webp");
        fs::write(&photo, b"photo").unwrap();
        let mut log = DailyLog::new(dir.path().into());
        let before = Utc.with_ymd_and_hms(2026, 10, 23, 15, 59, 59).unwrap();
        let after = before + chrono::Duration::seconds(1);
        log.append(before, &line(before, "下载完成", false))
            .unwrap();
        log.append(after, &line(after, "网络失败", true)).unwrap();
        let old = dir.path().join("运行记录-2026-10-23.log");
        let today = dir.path().join("运行记录-2026-10-24.log");
        let previous = fs::read(&old).unwrap();
        let restarted = DailyLog::new(dir.path().into());
        restarted.append(after, "再次启动").unwrap();
        let text = fs::read_to_string(&today).unwrap();
        assert!(text.contains("[错误] 网络失败") && text.contains("再次启动"));
        fs::remove_file(&today).unwrap();
        log.append(after, "删除后继续").unwrap();
        assert_eq!(fs::read_to_string(&today).unwrap(), "删除后继续\r\n");
        assert_eq!(fs::read(&old).unwrap(), previous);
        assert_eq!(fs::read(&photo).unwrap(), b"photo");
        let other = dir.path().join("other");
        log.set_directory(other.clone());
        log.append(after, "新目录").unwrap();
        assert!(other.join("运行记录-2026-10-24.log").exists());
    }
    #[test]
    fn write_failure_can_recover_without_removing_photos() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("not-a-folder");
        fs::write(&bad, b"keep").unwrap();
        let mut log = DailyLog::new(bad.clone());
        assert!(log.append(Utc::now(), "event").is_err());
        log.set_directory(dir.path().join("photos"));
        log.append(Utc::now(), "recovered").unwrap();
        assert_eq!(fs::read(bad).unwrap(), b"keep");
    }
}
