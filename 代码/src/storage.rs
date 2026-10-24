use crate::{
    domain::{ORIGIN, Observation, PRODUCT, Stop},
    imaging::{Frame, decode},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveFormat {
    #[default]
    Png,
    Webp,
}
impl ArchiveFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Webp => "WebP 无损",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Provenance {
    schema: u8,
    product: String,
    observation: Observation,
    source: String,
    received_utc: String,
    sha256: String,
}
pub struct Saved {
    pub path: PathBuf,
    pub existing: bool,
}

pub fn lock(directory: &Path) -> Result<File> {
    fs::create_dir_all(directory).context("创建照片目录失败")?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(".archive.lock"))?;
    file.try_lock().context("另一个任务正在使用此照片目录")?;
    Ok(file)
}

pub fn load(path: &Path, stop: &Stop) -> Result<Frame> {
    stop.check()?;
    ensure!(
        fs::metadata(path)?.len() <= 320 * 1024 * 1024,
        "归档文件异常大"
    );
    let image = if path
        .extension()
        .is_some_and(|v| v.eq_ignore_ascii_case("webp"))
    {
        crate::webp_archive::decode(&fs::read(path)?, stop)?
    } else {
        decode(BufReader::new(File::open(path)?), 8800)?
    };
    let raw = image.note.context("不是 V2 归档：缺少校验信息")?;
    ensure!(raw.len() <= 65536, "归档说明过长");
    let note: Provenance = serde_json::from_str(&raw)?;
    ensure!(
        note.schema == 2 && note.product == PRODUCT && note.source == ORIGIN,
        "归档来源或版本不匹配"
    );
    note.observation.validate()?;
    ensure!(
        image.pixels.dimensions() == (note.observation.side(), note.observation.side()),
        "归档尺寸与说明不匹配"
    );
    let frame = Frame {
        observation: note.observation,
        pixels: image.pixels,
        colors: image.colors,
    };
    ensure!(
        frame.fingerprint(stop)? == note.sha256,
        "归档像素或色彩校验失败"
    );
    Ok(frame)
}

fn candidates(directory: &Path, observation: &Observation) -> Result<Vec<PathBuf>> {
    let prefix = format!("{}_", observation.time.format("%Y%m%dT%H%M%SZ"));
    let size = format!("_{}_{}x{}", PRODUCT, observation.side(), observation.side());
    let mut paths = Vec::new();
    for path in crate::archive_index::files(directory)? {
        let name = path.file_name().unwrap().to_string_lossy();
        if name.starts_with(&prefix) && name.contains(&size) {
            paths.push(path);
        }
    }
    paths
        .sort_by_cached_key(|p| std::cmp::Reverse(fs::metadata(p).and_then(|m| m.modified()).ok()));
    Ok(paths)
}
pub fn existing(
    directory: &Path,
    observation: &Observation,
    stop: &Stop,
) -> Result<Option<PathBuf>> {
    for path in candidates(directory, observation)? {
        stop.check()?;
        if let Ok(frame) = load(&path, stop)
            && frame.observation.time == observation.time
            && frame.observation.grid == observation.grid
        {
            return Ok(Some(path));
        }
    }
    stop.check()?;
    Ok(None)
}

pub fn save(directory: &Path, frame: &Frame, stop: &Stop) -> Result<Saved> {
    save_as(directory, frame, ArchiveFormat::Png, stop)
}
pub fn save_as(
    directory: &Path,
    frame: &Frame,
    format: ArchiveFormat,
    stop: &Stop,
) -> Result<Saved> {
    fs::create_dir_all(directory)?;
    let digest = frame.fingerprint(stop)?;
    for path in candidates(directory, &frame.observation)? {
        stop.check()?;
        if let Ok(old) = load(&path, stop)
            && old.observation.time == frame.observation.time
            && old.observation.grid == frame.observation.grid
            && old.fingerprint(stop)? == digest
        {
            return Ok(Saved {
                path,
                existing: true,
            });
        }
    }
    let note = Provenance {
        schema: 2,
        product: PRODUCT.into(),
        observation: frame.observation.clone(),
        source: ORIGIN.into(),
        received_utc: chrono::Utc::now().to_rfc3339(),
        sha256: digest.clone(),
    };
    let mut temporary = tempfile::Builder::new()
        .prefix(".incomplete-")
        .tempfile_in(directory)?;
    if format == ArchiveFormat::Webp {
        crate::webp_archive::encode(
            temporary.as_file_mut(),
            frame,
            &serde_json::to_string(&note)?,
            stop,
        )?;
    } else {
        let mut encoder = png::Encoder::new(
            temporary.as_file_mut(),
            frame.pixels.width(),
            frame.pixels.height(),
        );
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Balanced);
        frame.colors.apply(&mut encoder);
        encoder.add_itxt_chunk("EarthLiveV2".into(), serde_json::to_string(&note)?)?;
        let mut writer = encoder.write_header()?;
        {
            let mut stream = writer.stream_writer()?;
            for row in frame
                .pixels
                .as_raw()
                .chunks_exact(frame.pixels.width() as usize * 3)
            {
                stop.check()?;
                stream
                    .write_all(row)
                    .context("写入原图失败，检查磁盘空间")?;
            }
            stream.finish()?;
        }
        writer.finish()?;
    }
    temporary.as_file().sync_all()?;
    stop.publish(|| {
        // persist_noclobber is the final arbiter even if another process races us.
        for number in 0..10000 {
            let suffix = match number {
                0 => String::new(),
                1 => format!("__{}", &digest[..16]),
                n => format!("__{}_{n}", &digest[..16]),
            };
            let path = directory.join(format!(
                "{}{suffix}.{}",
                frame.observation.stem(),
                format.extension()
            ));
            match temporary.persist_noclobber(&path) {
                Ok(_) => {
                    crate::archive_index::record(&path, true);
                    return Ok(Saved {
                        path,
                        existing: false,
                    });
                }
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    temporary = error.file;
                }
                Err(error) => return Err(error.error).context("提交原图失败"),
            }
        }
        anyhow::bail!("同名文件冲突过多")
    })
}

pub fn write_bmp(path: &Path, pixels: &image::RgbImage, cache: bool, stop: &Stop) -> Result<()> {
    ensure!(
        path.extension()
            .is_some_and(|v| v.eq_ignore_ascii_case("bmp")),
        "壁纸输出须为 BMP"
    );
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    stop.check()?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".wallpaper-")
        .tempfile_in(parent)?;
    image::codecs::bmp::BmpEncoder::new(temporary.as_file_mut()).encode(
        pixels.as_raw(),
        pixels.width(),
        pixels.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    temporary.as_file().sync_all()?;
    stop.publish(|| {
        if cache {
            if path.exists() {
                let mut backup = tempfile::NamedTempFile::new_in(parent)?;
                std::io::copy(&mut File::open(path)?, backup.as_file_mut())?;
                backup.as_file().sync_all()?;
                backup
                    .persist(path.with_file_name("wallpaper-B.bmp"))
                    .map_err(|e| e.error)?;
            }
            temporary.persist(path).map_err(|e| e.error)?;
        } else {
            temporary
                .persist_noclobber(path)
                .map_err(|e| e.error)
                .context("目标已存在或不可写，未覆盖")?;
        }
        Ok(())
    })
}

/// Keep the successfully saved frame; remove oldest verified archives only.
/// Called under the directory lease, never for historical requests.
pub fn trim(
    directory: &Path,
    limit: u32,
    keep: &Path,
    stop: &Stop,
    removed: impl Fn(),
) -> Result<()> {
    if limit == 0 {
        return Ok(());
    }
    let _lease = lock(directory)?;
    let files = crate::archive_index::files(directory)?;
    let mut count = files.len();
    for path in files {
        if count <= limit as usize {
            break;
        }
        stop.check()?;
        if path == keep {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_file() => m,
            _ => {
                crate::archive_index::record(&path, false);
                count -= 1;
                continue;
            }
        };
        // Read provenance again immediately before deletion; never decode full pixels here.
        if !is_archive(&path) {
            count -= 1;
            continue;
        }
        stop.publish(|| -> Result<()> {
            let current = fs::symlink_metadata(&path)?;
            ensure!(
                current.file_type().is_file()
                    && current.len() == metadata.len()
                    && current.modified()? == metadata.modified()?,
                "待清理照片已被其他程序修改"
            );
            fs::remove_file(&path).context("无法清理旧照片")?;
            Ok(())
        })?;
        crate::archive_index::record(&path, false);
        count -= 1;
        removed();
    }
    Ok(())
}

/// Read only container metadata, not image pixels, when building the inventory.
pub(crate) fn is_archive(path: &Path) -> bool {
    (|| -> Result<()> {
        let file = BufReader::new(File::open(path)?);
        let raw = if path.extension().is_some_and(|e| e == "webp") {
            crate::webp_archive::read_note(file)?
        } else {
            let reader = png::Decoder::new(file).read_info()?;
            reader
                .info()
                .utf8_text
                .iter()
                .find(|t| t.keyword == "EarthLiveV2")
                .context("缺少归档说明")?
                .get_text()?
        };
        ensure!(raw.len() <= 65536, "归档说明过大");
        let note: Provenance = serde_json::from_str(&raw)?;
        ensure!(
            note.schema == 2 && note.product == PRODUCT && note.source == ORIGIN,
            "非本程序归档"
        );
        note.observation.validate()?;
        ensure!(
            path.file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with(&note.observation.stem()),
            "归档文件名不匹配"
        );
        Ok(())
    })()
    .is_ok()
}
