//! Daily UTF-8 logs next to the photos. No long-lived handle: users may delete
//! a log while the application runs; the next event recreates today's file.
use chrono::{DateTime, FixedOffset, Utc};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Write},
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
    // Path and length of the log after the last append, with its saved count.
    counted: Option<(PathBuf, u64, u64)>,
}
impl DailyLog {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            counted: None,
        }
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn set_directory(&mut self, path: PathBuf) {
        self.directory = path;
        self.counted = None;
    }
    pub fn record(
        &mut self,
        time: DateTime<Utc>,
        error: bool,
        saved: Option<usize>,
        message: impl FnOnce(Option<u64>) -> String,
    ) -> (String, io::Result<()>) {
        let path = self
            .directory
            .join(format!("运行记录-{}.log", beijing(time).format("%Y-%m-%d")));
        let opened = (|| {
            fs::create_dir_all(&self.directory)?;
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            let length = file.metadata()?.len();
            Ok((file, length))
        })();
        let (mut file, length) = match opened {
            Ok(opened) => opened,
            Err(err) => return (line(time, &message(None), error), Err(err)),
        };
        let cached = self.counted.take().and_then(|(cached_path, bytes, total)| {
            (cached_path == path && bytes == length).then_some(total)
        });
        let (total, count_result) = match saved {
            Some(saved) => {
                let previous = cached.map_or_else(|| count_saved(&path), Ok);
                match previous {
                    Ok(previous) => (Some(previous.saturating_add(saved as u64)), Ok(())),
                    Err(err) => (None, Err(err)),
                }
            }
            None => (cached, Ok(())),
        };
        let record = line(time, &message(saved.and(total)), error);
        let bytes = format!("{record}\r\n");
        let written = file.write_all(bytes.as_bytes()).and_then(|()| file.flush());
        if written.is_ok() {
            self.counted = total.map(|total| (path, length + bytes.len() as u64, total));
        }
        (record, written.and(count_result))
    }
}

fn count_saved(path: &Path) -> io::Result<u64> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut bytes = Vec::new();
    let mut total = 0;
    while reader.read_until(b'\n', &mut bytes)? != 0 {
        if let Some(saved) = saved_in_record(&bytes) {
            total = u64::saturating_add(total, saved);
        }
        bytes.clear();
    }
    Ok(total)
}

fn saved_in_record(bytes: &[u8]) -> Option<u64> {
    let record = std::str::from_utf8(bytes.strip_suffix(b"\n")?).ok()?;
    let (_, fields) = record.rsplit_once("｜新增 ")?;
    let mut fields = fields.split('｜');
    let saved = fields.next()?.parse().ok()?;
    let middle = fields.next()?;
    let missing = fields.next()?;
    ((middle.starts_with("已有 ") || middle.starts_with("今日累计 "))
        && missing.starts_with("缺失 "))
    .then_some(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn batch(log: &mut DailyLog, time: DateTime<Utc>, saved: usize) -> String {
        let (record, written) = log.record(time, false, Some(saved), |total| {
            format!(
                "抓取完成｜新增 {saved}｜今日累计 {}｜缺失 0",
                total.unwrap()
            )
        });
        written.unwrap();
        record
    }

    #[test]
    fn midnight_append_restart_delete_and_directory_change() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("photo.webp");
        fs::write(&photo, b"photo").unwrap();
        let mut log = DailyLog::new(dir.path().into());
        let before = Utc.with_ymd_and_hms(2026, 10, 23, 15, 59, 59).unwrap();
        let after = before + chrono::Duration::seconds(1);
        assert!(batch(&mut log, before, 3).contains("今日累计 3｜"));
        log.record(after, true, None, |_| "网络失败".into())
            .1
            .unwrap();
        log.record(after, false, None, |_| {
            "抓取完成｜新增 12｜已有 7｜缺失 0".into()
        })
        .1
        .unwrap();
        assert!(batch(&mut log, after, 1).contains("今日累计 13｜"));
        let old = dir.path().join("运行记录-2026-10-23.log");
        let today = dir.path().join("运行记录-2026-10-24.log");
        let previous = fs::read(&old).unwrap();
        let mut restarted = DailyLog::new(dir.path().into());
        restarted
            .record(after, false, None, |_| "再次启动".into())
            .1
            .unwrap();
        assert!(batch(&mut restarted, after, 2).contains("今日累计 15｜"));
        let text = fs::read_to_string(&today).unwrap();
        assert!(text.contains("[错误] 网络失败") && text.contains("再次启动"));
        fs::remove_file(&today).unwrap();
        assert!(batch(&mut restarted, after, 1).contains("今日累计 1｜"));
        assert_eq!(fs::read(&old).unwrap(), previous);
        assert_eq!(fs::read(&photo).unwrap(), b"photo");
        let other = dir.path().join("other");
        log.set_directory(other.clone());
        assert!(batch(&mut log, after, 2).contains("今日累计 2｜"));
        assert!(other.join("运行记录-2026-10-24.log").exists());
    }
    #[test]
    fn write_failure_can_recover_without_removing_photos() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("not-a-folder");
        fs::write(&bad, b"keep").unwrap();
        let mut log = DailyLog::new(bad.clone());
        let (record, result) = log.record(Utc::now(), false, Some(1), |total| {
            assert_eq!(total, None);
            "今日累计不可用".into()
        });
        assert!(record.contains("今日累计不可用"));
        assert!(result.is_err());
        log.set_directory(dir.path().join("photos"));
        assert!(batch(&mut log, Utc::now(), 1).contains("今日累计 1｜"));
        assert_eq!(fs::read(bad).unwrap(), b"keep");
    }
}
