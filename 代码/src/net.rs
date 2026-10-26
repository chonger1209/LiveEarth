use crate::domain::{ORIGIN, Observation, Stop, metadata};
use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, StatusCode};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};

#[derive(Debug)]
pub struct HttpFailure(pub u16);
impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "源站 HTTP {}", self.0)
    }
}
impl std::error::Error for HttpFailure {}
#[derive(Debug)]
pub struct RetryDelay(pub u64);
impl std::fmt::Display for RetryDelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "源站要求 {} 秒后重试", self.0)
    }
}
impl std::error::Error for RetryDelay {}

#[derive(Clone)]
pub struct Source {
    client: Client,
    origin: String,
    cooldown: Arc<Mutex<Instant>>,
}
impl Source {
    pub fn new() -> Result<Self> {
        Self::at(ORIGIN)
    }
    fn at(origin: &str) -> Result<Self> {
        let root = reqwest::Url::parse(origin)?;
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!(
                    "LiveEarth/",
                    env!("CARGO_PKG_VERSION"),
                    " (personal satellite wallpaper)"
                ))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt.previous().len() > 3 {
                        attempt.error("源站跳转次数超过 3 次，稍后重试")
                    } else if same_origin_redirect(&root, attempt.url()) {
                        attempt.follow()
                    } else {
                        attempt.error("源站跳转到不受支持的地址，已拒绝跨域或协议变更")
                    }
                }))
                .build()?,
            origin: origin.trim_end_matches('/').into(),
            cooldown: Arc::new(Mutex::new(Instant::now())),
        })
    }
    #[cfg(test)]
    pub(crate) fn local(origin: &str) -> Result<Self> {
        ensure!(
            origin.starts_with("http://127.0.0.1:"),
            "测试只允许回环地址"
        );
        Self::at(origin)
    }
    pub async fn latest(&self, grid: u32, stop: &Stop) -> Result<Observation> {
        metadata(&self.get("/latest.json", 65536, stop).await?, grid)
            .context("解析同产品最新观测失败")
    }
    pub async fn tile(&self, frame: &Observation, x: u32, y: u32, stop: &Stop) -> Result<Vec<u8>> {
        self.get(&frame.tile_path(x, y)?, 4 * 1024 * 1024, stop)
            .await
            .with_context(|| format!("分片 ({x},{y})，{}", frame.time))
    }
    pub async fn get(&self, path: &str, limit: usize, stop: &Stop) -> Result<Vec<u8>> {
        let url = format!("{}{path}", self.origin);
        for attempt in 0..3u32 {
            stop.check()?;
            let deadline = *self.cooldown.lock().await;
            tokio::select! { _ = stop.cancelled() => bail!("已取消"), _ = tokio::time::sleep_until(deadline) => {} }
            let response = tokio::select! {
                _ = stop.cancelled() => bail!("已取消"),
                response = self.client.get(&url).send() => response,
            };
            let mut response = match response {
                Ok(value) => value,
                Err(error) if attempt == 2 => return Err(error).context("请求失败，已尝试 3 次"),
                Err(_) => {
                    self.delay(Duration::from_secs(1 << attempt), stop).await?;
                    continue;
                }
            };
            let status = response.status();
            if !status.is_success() {
                if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    let delay = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok().or_else(|| DateHeader::seconds(v)))
                        .unwrap_or(2u64 << attempt);
                    if attempt == 2 || delay > 60 {
                        return Err(anyhow::Error::new(HttpFailure(status.as_u16()))
                            .context(RetryDelay(delay)));
                    }
                    let mut until = self.cooldown.lock().await;
                    *until = (*until).max(Instant::now() + Duration::from_secs(delay));
                    continue;
                }
                return Err(HttpFailure(status.as_u16()).into());
            }
            ensure!(
                response.content_length().is_none_or(|n| n <= limit as u64),
                "响应超过大小限制"
            );
            let mut bytes = Vec::new();
            loop {
                let chunk = tokio::select! {
                    _ = stop.cancelled() => bail!("已取消"),
                    chunk = response.chunk() => chunk.context("读取响应失败")?,
                };
                let Some(chunk) = chunk else {
                    break;
                };
                ensure!(bytes.len() + chunk.len() <= limit, "响应超过大小限制");
                bytes.extend_from_slice(&chunk);
            }
            stop.check()?;
            return Ok(bytes);
        }
        bail!("请求重试已用尽")
    }
    async fn delay(&self, time: Duration, stop: &Stop) -> Result<()> {
        tokio::select! { _ = stop.cancelled() => bail!("已取消"), _ = tokio::time::sleep(time) => Ok(()) }
    }
}
fn same_origin_redirect(origin: &reqwest::Url, next: &reqwest::Url) -> bool {
    origin.scheme() == next.scheme()
        && origin.host_str() == next.host_str()
        && origin.port_or_known_default() == next.port_or_known_default()
        && next.username().is_empty()
        && next.password().is_none()
}
struct DateHeader;
impl DateHeader {
    fn seconds(value: &str) -> Option<u64> {
        chrono::DateTime::parse_from_rfc2822(value).ok().map(|t| {
            (t.with_timezone(&chrono::Utc) - chrono::Utc::now())
                .num_seconds()
                .max(0) as u64
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redirect_policy_preserves_https_origin_and_port() {
        let root = reqwest::Url::parse("https://example.com/source").unwrap();
        for (url, allowed) in [
            ("https://example.com/next", true),
            ("https://example.com:443/next", true),
            ("http://example.com/next", false),
            ("https://other.example.com/next", false),
            ("https://example.com:444/next", false),
            ("https://user:password@example.com/next", false),
        ] {
            assert_eq!(
                same_origin_redirect(&root, &reqwest::Url::parse(url).unwrap()),
                allowed
            );
        }
    }
}
