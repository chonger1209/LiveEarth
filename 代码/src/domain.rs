use anyhow::{Result, bail, ensure};
use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub const PRODUCT: &str = "D531106";
pub const TILE: u32 = 550;
pub const ORIGIN: &str = "https://himawari8-dl.nict.go.jp/himawari8/img/D531106";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Observation {
    pub time: DateTime<Utc>,
    pub grid: u32,
    pub satellite: String,
}
impl Observation {
    pub fn new(time: DateTime<Utc>, grid: u32) -> Result<Self> {
        let value = Self {
            time,
            grid,
            satellite: "Hunknown".into(),
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [1, 2, 4, 8, 16].contains(&self.grid),
            "分片级别必须为 1、2、4、8、16"
        );
        ensure!(
            self.time.minute().is_multiple_of(10)
                && self.time.second() == 0
                && self.time.nanosecond() == 0,
            "观测时间须精确落在 10 分钟边界"
        );
        ensure!(
            ["H08", "H09", "Hunknown"].contains(&self.satellite.as_str()),
            "卫星编号无效"
        );
        Ok(())
    }
    pub fn side(&self) -> u32 {
        self.grid * TILE
    }
    pub fn stem(&self) -> String {
        format!(
            "{}_{}_{}_{}x{}",
            self.time.format("%Y%m%dT%H%M%SZ"),
            self.satellite,
            PRODUCT,
            self.side(),
            self.side()
        )
    }
    pub fn tile_path(&self, x: u32, y: u32) -> Result<String> {
        ensure!(x < self.grid && y < self.grid, "分片坐标越界");
        Ok(format!(
            "/{}d/550/{}_{}_{}.png",
            self.grid,
            self.time.format("%Y/%m/%d/%H%M%S"),
            x,
            y
        ))
    }
}

pub fn parse_time(value: &str, beijing: bool) -> Result<DateTime<Utc>> {
    if let Ok(time) = DateTime::parse_from_rfc3339(value.trim()) {
        return Ok(time.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M")?;
    let zone = FixedOffset::east_opt(if beijing { 8 * 3600 } else { 0 }).unwrap();
    Ok(zone
        .from_local_datetime(&naive)
        .single()
        .unwrap()
        .with_timezone(&Utc))
}

pub fn range(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: u32,
    grid: u32,
    now: DateTime<Utc>,
) -> Result<Vec<Observation>> {
    ensure!(
        [10, 20, 30, 60].contains(&step),
        "间隔须为 10、20、30、60 分钟"
    );
    Observation::new(start, grid)?;
    Observation::new(end, grid)?;
    ensure!(start <= end && end <= now, "历史区间不能倒置或包含未来时间");
    let count = (end - start).num_minutes() / i64::from(step) + 1;
    ensure!(count <= 10_000, "单批最多 10000 帧，请缩小区间");
    (0..count)
        .map(|i| Observation::new(start + Duration::minutes(i * i64::from(step)), grid))
        .collect()
}

pub fn metadata(bytes: &[u8], grid: u32) -> Result<Observation> {
    #[derive(Deserialize)]
    struct Metadata {
        date: String,
        file: Option<String>,
    }
    let raw: Metadata = serde_json::from_slice(bytes)?;
    let time = NaiveDateTime::parse_from_str(&raw.date, "%Y-%m-%d %H:%M:%S")?.and_utc();
    let mut observation = Observation::new(time, grid)?;
    if let Some(file) = raw.file {
        let parts: Vec<_> = file.rsplit('/').next().unwrap_or("").split('_').collect();
        // NICT: HS_H09_20261023_0700_B13_FLDK_R20_S0101.DAT
        if parts.len() >= 4
            && ["HS", "PI"].contains(&parts[0])
            && ["H08", "H09"].contains(&parts[1])
        {
            let stamp =
                NaiveDateTime::parse_from_str(&format!("{}{}", parts[2], parts[3]), "%Y%m%d%H%M")?
                    .and_utc();
            ensure!(stamp == time, "元数据 date 与 file 的观测时间冲突");
            observation.satellite = parts[1].into();
        }
    }
    ensure!(observation.time <= Utc::now(), "源站返回了未来观测时间");
    Ok(observation)
}

#[derive(Clone, Default)]
pub struct Stop {
    token: CancellationToken,
    commit_gate: Arc<Mutex<()>>,
}
impl Stop {
    pub fn cancel(&self) {
        let _guard = self.commit_gate.lock().unwrap_or_else(|e| e.into_inner());
        self.token.cancel();
    }
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("已取消")
        }
        Ok(())
    }
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
    pub fn publish<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self.commit_gate.lock().unwrap_or_else(|e| e.into_inner());
        self.check()?;
        action()
    }
}
