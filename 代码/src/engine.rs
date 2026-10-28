use crate::{
    domain::{Observation, Stop, TILE},
    imaging::{Assembly, decode},
    net::{HttpFailure, Source, SourceDataError},
    storage,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    io::Cursor,
    path::{Path, PathBuf},
};
use tokio::task::JoinSet;

#[derive(Clone)]
pub enum Selection {
    Latest(u32),
    History(Vec<Observation>),
}
#[derive(Clone, Debug)]
pub enum Notice {
    Stage(Phase),
    Planned(usize),
    Frame(Observation),
    Tiles { done: usize, total: usize },
    Saved { path: PathBuf, existed: bool },
    Missing(Observation),
    WallpaperUpdated,
    Pruned,
    ArchiveMode { degraded: bool },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Checking,
    Downloading,
    Saving,
    Reading,
    Wallpaper,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Checking => "检查最新影像",
            Self::Downloading => "下载与拼接影像",
            Self::Saving => "压缩并保存原图",
            Self::Reading => "读取并校验原图",
            Self::Wallpaper => "生成并应用壁纸",
        }
    }
}
#[derive(Clone, Default, Debug, Serialize)]
pub struct Report {
    pub planned: usize,
    pub saved: usize,
    pub existed: usize,
    pub missing: usize,
    pub last: Option<PathBuf>,
}

pub async fn run(
    source: Source,
    selection: Selection,
    directory: &Path,
    refresh: bool,
    stop: &Stop,
    notify: &(dyn Fn(Notice) + Sync),
) -> Result<Report> {
    run_as(
        source,
        selection,
        directory,
        refresh,
        storage::ArchiveFormat::Webp,
        stop,
        notify,
    )
    .await
}
pub async fn run_as(
    source: Source,
    selection: Selection,
    directory: &Path,
    refresh: bool,
    format: storage::ArchiveFormat,
    stop: &Stop,
    notify: &(dyn Fn(Notice) + Sync),
) -> Result<Report> {
    let _lease = storage::lock(directory)?;
    if let Some(degraded) = crate::archive_index::begin_batch(directory, stop)? {
        notify(Notice::ArchiveMode { degraded });
    }
    // Check destination before any download.
    tempfile::NamedTempFile::new_in(directory).context("照片目录不可写")?;
    let frames = match selection {
        Selection::Latest(grid) => {
            notify(Notice::Stage(Phase::Checking));
            vec![source.latest(grid, stop).await?]
        }
        Selection::History(frames) => frames,
    };
    ensure!(
        !frames.is_empty() && frames.len() <= 10000,
        "任务帧数须为 1–10000"
    );
    let mut report = Report {
        planned: frames.len(),
        ..Default::default()
    };
    notify(Notice::Planned(report.planned));
    for (index, observation) in frames.into_iter().enumerate() {
        stop.check()?;
        observation.validate()?;
        notify(Notice::Frame(observation.clone()));
        if !refresh && let Some(path) = storage::existing(directory, &observation, stop)? {
            report.existed += 1;
            report.last = Some(path.clone());
            notify(Notice::Saved {
                path,
                existed: true,
            });
            continue;
        }
        notify(Notice::Stage(Phase::Downloading));
        match fetch_frame(&source, observation.clone(), stop, notify).await {
            Ok(frame) => {
                notify(Notice::Stage(Phase::Saving));
                let saved = storage::save_as(directory, &frame, format, stop)?;
                if saved.existing {
                    report.existed += 1;
                } else {
                    report.saved += 1;
                }
                report.last = Some(saved.path.clone());
                notify(Notice::Saved {
                    path: saved.path,
                    existed: saved.existing,
                });
            }
            Err(error)
                if error
                    .downcast_ref::<HttpFailure>()
                    .is_some_and(|e| e.0 == 404) =>
            {
                report.missing += 1;
                notify(Notice::Missing(observation));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "任务停止；本批已新增 {}、已有 {}、缺失 {}，未完成 {}。已存照片保留",
                        report.saved,
                        report.existed,
                        report.missing,
                        report.planned - index
                    )
                });
            }
        }
    }
    if let Some(degraded) = crate::archive_index::take_mode_change() {
        notify(Notice::ArchiveMode { degraded });
    }
    stop.check()?;
    Ok(report)
}

async fn fetch_frame(
    source: &Source,
    observation: Observation,
    stop: &Stop,
    notify: &(dyn Fn(Notice) + Sync),
) -> Result<crate::imaging::Frame> {
    let count = (observation.grid * observation.grid) as usize;
    let mut assembly = Assembly::new(observation.clone())?;
    let mut workers = JoinSet::new();
    let mut next = 0;
    let mut completed = 0;
    loop {
        while workers.len() < 4 && next < count {
            let x = next as u32 % observation.grid;
            let y = next as u32 / observation.grid;
            let source = source.clone();
            let frame = observation.clone();
            let stop = stop.clone();
            workers.spawn(async move {
                Ok::<_, anyhow::Error>((x, y, source.tile(&frame, x, y, &stop).await?))
            });
            next += 1;
        }
        let Some(result) = workers.join_next().await else {
            break;
        };
        let (x, y, bytes) = result.context("下载工作任务异常")??;
        stop.check()?;
        let tile = decode(Cursor::new(bytes), TILE)
            .context(SourceDataError("源站分片图像无效"))
            .with_context(|| format!("分片 ({x},{y}) 校验失败"))?;
        assembly
            .insert(x, y, tile, stop)
            .context(SourceDataError("源站分片尺寸或色彩不一致"))?;
        completed += 1;
        notify(Notice::Tiles {
            done: completed,
            total: count,
        });
    }
    stop.check()?;
    assembly.finish()
}
