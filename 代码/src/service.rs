use crate::{
    desktop,
    domain::{Observation, Stop},
    engine::{self, Notice, Phase, Report, Selection},
    imaging, journal,
    net::{HttpFailure, Source},
    settings::{self, Preferences},
    storage,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Preview {
    pub pixels: image::RgbImage,
    pub observation: Observation,
    pub path: PathBuf,
    pub bytes: u64,
    pub format: String,
}
#[derive(Clone)]
pub struct Feedback {
    pub message: String,
    pub error: bool,
    pub at: Instant,
}
#[derive(Clone)]
pub struct Snapshot {
    pub preferences: Option<Preferences>,
    pub enabled: bool,
    pub busy: bool,
    pub exiting: bool,
    pub exited: bool,
    pub status: String,
    pub error: bool,
    pub phase: Phase,
    pub retrying: bool,
    pub saved: u64,
    pub missing: usize,
    pub next_check: Option<DateTime<Utc>>,
    pub last_check: Option<DateTime<Utc>>,
    pub last_success: Option<DateTime<Utc>>,
    pub preview: Option<Arc<Preview>>,
    pub logs: VecDeque<String>,
    pub feedback: Option<Feedback>,
    pub log_error: Option<String>,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            preferences: None,
            enabled: false,
            busy: false,
            exiting: false,
            exited: false,
            status: "就绪".into(),
            error: false,
            phase: Phase::default(),
            retrying: false,
            saved: 0,
            missing: 0,
            next_check: None,
            last_check: None,
            last_success: None,
            preview: None,
            logs: VecDeque::new(),
            feedback: None,
            log_error: None,
        }
    }
}
impl Snapshot {
    pub fn can_configure_capture(&self) -> bool {
        capture_editable(self.enabled, self.busy, self.exiting)
    }
    pub fn activity(&self) -> Activity {
        if self.busy {
            Activity::Working
        } else if self.enabled && self.retrying {
            Activity::Retrying
        } else if self.enabled {
            Activity::Waiting
        } else {
            Activity::Stopped
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Stopped,
    Waiting,
    Working,
    Retrying,
}
fn capture_editable(enabled: bool, busy: bool, exiting: bool) -> bool {
    !enabled && !busy && !exiting
}
pub enum AfterSave {
    Start,
    Capture(Selection, bool),
}
pub struct ConfigReply {
    pub preferences: Preferences,
    pub error: Option<String>,
}
pub enum Command {
    Configure(Preferences, Option<AfterSave>, Sender<ConfigReply>),
    Capture(Selection, bool),
    SetEnabled(bool),
    Toggle,
    CheckNow,
    Cancel,
    Resume,
    Record(String, bool),
    Shutdown,
}
#[derive(Clone)]
struct Batch {
    report: Report,
    observation: Option<Observation>,
    wallpaper_updated: bool,
    pruned: usize,
    started: Instant,
}
impl Default for Batch {
    fn default() -> Self {
        Self {
            report: Report::default(),
            observation: None,
            wallpaper_updated: false,
            pruned: 0,
            started: Instant::now(),
        }
    }
}
impl Batch {
    fn summary(&self, result: &str) -> String {
        let time = self.observation.as_ref().map_or(String::new(), |o| {
            format!("｜影像 {} UTC", o.time.format("%m-%d %H:%M"))
        });
        format!(
            "{result}{time}｜新增 {}｜已有 {}｜缺失 {}{}{}｜耗时 {:.1} 秒",
            self.report.saved,
            self.report.existed,
            self.report.missing,
            if self.wallpaper_updated {
                "｜壁纸已更新"
            } else {
                ""
            },
            if self.pruned > 0 {
                format!("｜清理旧图 {}", self.pruned)
            } else {
                String::new()
            },
            self.started.elapsed().as_secs_f32()
        )
    }
}
#[derive(Default)]
struct Accounting {
    // Product is fixed by this application. Satellite labels can be unknown in history.
    missing: HashSet<(DateTime<Utc>, u32)>,
    batch: Batch,
}
struct Shared {
    value: Mutex<Snapshot>,
    journal: Mutex<journal::DailyLog>,
    accounting: Mutex<Accounting>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
impl Shared {
    fn set_log_directory(&self, directory: &std::path::Path) {
        let mut log = self.journal.lock().unwrap_or_else(|p| p.into_inner());
        if log.directory() != directory {
            log.set_directory(directory.to_path_buf());
        }
    }
    fn edit(&self, f: impl FnOnce(&mut Snapshot)) {
        f(&mut self.value.lock().unwrap_or_else(|p| p.into_inner()));
        (self.wake)();
    }
    fn record(&self, message: impl Into<String>, error: bool) {
        let message = message.into();
        let time = Utc::now();
        let line = journal::line(time, &message, error);
        let failure = self
            .journal
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .append(time, &line)
            .err()
            .map(|e| format!("日志未写入文件：{e}"));
        self.edit(|s| {
            s.logs.push_back(line);
            if failure != s.log_error {
                if let Some(e) = &failure {
                    s.logs.push_back(journal::line(time, e, true));
                } else if s.log_error.is_some() {
                    s.logs
                        .push_back(journal::line(time, "日志文件写入已恢复", false));
                }
            }
            s.log_error = failure;
            while s.logs.len() > 300 {
                s.logs.pop_front();
            }
        });
    }
    fn log(&self, message: impl Into<String>, error: bool) {
        let message = message.into();
        self.record(&message, error);
        self.edit(|s| {
            s.status = message;
            s.error = error;
        });
    }
    fn feedback(&self, message: impl Into<String>, error: bool) {
        let message = message.into();
        self.record(&message, error);
        self.edit(|s| {
            s.feedback = Some(Feedback {
                message,
                error,
                at: Instant::now(),
            })
        });
    }
    fn notice(&self, notice: Notice) {
        match notice {
            Notice::Stage(phase) => self.edit(|s| {
                s.phase = phase;
                s.status = phase.label().into();
            }),
            Notice::Planned(count) => {
                self.accounting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .batch
                    .report
                    .planned = count;
            }
            Notice::Frame(observation) => {
                self.accounting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .batch
                    .observation = Some(observation);
            }
            Notice::Missing(observation) => {
                let mut a = self.accounting.lock().unwrap_or_else(|p| p.into_inner());
                a.batch.report.missing += 1;
                a.missing.insert((observation.time, observation.grid));
                self.edit(|s| s.missing = a.missing.len());
            }
            Notice::Tiles { done, total } => self.edit(|s| {
                s.phase = Phase::Downloading;
                s.status = format!("下载与拼接分片 {done}/{total}");
            }),
            Notice::Saved { path, existed } => {
                let mut a = self.accounting.lock().unwrap_or_else(|p| p.into_inner());
                a.batch.report.last = Some(path);
                if existed {
                    a.batch.report.existed += 1;
                } else {
                    a.batch.report.saved += 1;
                    self.edit(|s| s.saved += 1);
                }
            }
            Notice::Pruned => {
                self.accounting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .batch
                    .pruned += 1;
            }
            Notice::WallpaperUpdated => {
                self.accounting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .batch
                    .wallpaper_updated = true;
            }
        }
    }
}
#[derive(Default)]
struct Gate {
    enabled: bool,
    apply: bool,
    zoom: u16,
    revision: u64,
    last_applied: Option<(String, u16, (u32, u32))>,
}
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Command>,
    shared: Arc<Shared>,
}
impl Handle {
    pub fn configure(
        &self,
        preferences: Preferences,
        then: Option<AfterSave>,
    ) -> mpsc::Receiver<ConfigReply> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::Configure(preferences, then, tx));
        rx
    }
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
    pub fn snapshot(&self) -> Snapshot {
        self.shared
            .value
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}
pub struct Service {
    pub handle: Handle,
    join: Option<JoinHandle<()>>,
}
impl Service {
    pub fn spawn(
        data: PathBuf,
        prefs: Preferences,
        load: Option<PathBuf>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self::spawn_with(
            data,
            prefs,
            load,
            wake,
            Arc::new(real_job),
            Timing::default(),
        )
    }
    pub fn shutdown(&mut self) {
        self.handle.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}
struct Job {
    prefs: Preferences,
    data: PathBuf,
    selection: Option<Selection>,
    refresh: bool,
    automatic: bool,
    load: Option<PathBuf>,
}
#[derive(Default)]
struct Outcome {
    report: Option<Report>,
    preview: Option<Arc<Preview>>,
    warning: Option<String>,
}
type Runner = Arc<
    dyn Fn(Job, &Stop, &Arc<Mutex<Gate>>, &(dyn Fn(Notice) + Sync)) -> Result<Outcome>
        + Send
        + Sync,
>;
#[derive(Clone, Copy)]
struct Timing {
    retry: Duration,
    poll: Duration,
    interval_override: Option<Duration>,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            retry: Duration::from_secs(15),
            poll: Duration::from_millis(100),
            interval_override: None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    Transient,
    Storage,
    Permanent,
}
pub(crate) fn classify(error: &anyhow::Error) -> Fault {
    if error.downcast_ref::<reqwest::Error>().is_some()
        || error
            .downcast_ref::<HttpFailure>()
            .is_some_and(|e| e.0 == 429 || e.0 >= 500)
    {
        Fault::Transient
    } else if error.downcast_ref::<std::io::Error>().is_some() {
        Fault::Storage
    } else {
        Fault::Permanent
    }
}
fn accept_settings(
    data: &std::path::Path,
    current: &Preferences,
    mut next: Preferences,
    editable: bool,
) -> Result<Preferences> {
    next.automatic = current.automatic;
    next.last = current.last.clone();
    next.validate()?;
    ensure!(
        !next.capture_differs(current) || editable,
        "请先停止定时抓取，等待当前任务结束后再修改抓取设置"
    );
    if next.photos != current.photos {
        settings::validate_photo_directory(&next.photos)?;
    }
    let startup_changed = next.autostart != current.autostart;
    if startup_changed {
        crate::platform::set_autostart(next.autostart, &std::env::current_exe()?, data)?;
    }
    if let Err(error) = settings::save(data, &next) {
        if startup_changed {
            crate::platform::set_autostart(current.autostart, &std::env::current_exe()?, data)
                .context("配置保存失败，恢复 Windows 自启动也失败；请检查启动设置")?;
        }
        return Err(error.context("设置保存失败"));
    }
    Ok(next)
}
impl Service {
    fn spawn_with(
        data: PathBuf,
        mut prefs: Preferences,
        load: Option<PathBuf>,
        wake: Arc<dyn Fn() + Send + Sync>,
        runner: Runner,
        timing: Timing,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            value: Mutex::new(Snapshot {
                preferences: Some(prefs.clone()),
                enabled: prefs.automatic,
                busy: load.is_some(),
                ..Snapshot::default()
            }),
            journal: Mutex::new(journal::DailyLog::new(prefs.photos.clone())),
            accounting: Mutex::new(Accounting::default()),
            wake,
        });
        let handle = Handle {
            tx,
            shared: shared.clone(),
        };
        let join = thread::spawn(move || {
            shared.record(
                format!(
                    "程序启动 · {} {}",
                    crate::APP_NAME,
                    crate::display_version()
                ),
                false,
            );
            let gate = Arc::new(Mutex::new(Gate {
                enabled: prefs.automatic,
                apply: prefs.apply_automatic,
                zoom: prefs.zoom,
                ..Default::default()
            }));
            let mut enabled = prefs.automatic;
            let mut exiting = false;
            let mut due = enabled.then(Instant::now);
            let mut failures = 0u32;
            let mut queued = load.map(|path| Job {
                prefs: prefs.clone(),
                data: data.clone(),
                selection: None,
                refresh: false,
                automatic: false,
                load: Some(path),
            });
            let (done_tx, done_rx) = mpsc::channel::<(Result<Outcome>, bool)>();
            let mut active: Option<(Stop, JoinHandle<()>)> = None;
            let mut active_automatic = false;
            let mut followup: Option<Command> = None;
            let save = |p: &Preferences| {
                if let Err(e) = settings::save(&data, p) {
                    shared.feedback(format!("设置保存失败：{e:#}"), true);
                    false
                } else {
                    true
                }
            };
            loop {
                while let Ok((result, automatic)) = done_rx.try_recv() {
                    let cancelled = active.as_ref().is_some_and(|a| a.0.is_cancelled());
                    if let Some((_, join)) = active.take() {
                        let _ = join.join();
                    }
                    let mut batch = shared
                        .accounting
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .batch
                        .clone();
                    shared.edit(|s| {
                        s.busy = false;
                        if automatic {
                            s.last_check = Some(Utc::now());
                        }
                    });
                    let interval = timing
                        .interval_override
                        .unwrap_or(Duration::from_secs(prefs.interval_minutes as u64 * 60));
                    if cancelled {
                        shared.log(batch.summary("已取消，已完成照片保留"), false);
                        if automatic {
                            failures = 0;
                        }
                        if automatic && enabled && due.is_none() {
                            due = Some(Instant::now() + interval);
                        }
                    } else {
                        match result {
                            Ok(outcome) => {
                                if let Some(preview) = outcome.preview {
                                    prefs.last = Some(preview.path.clone());
                                    save(&prefs);
                                    shared.edit(|s| s.preview = Some(preview));
                                }
                                if let Some(report) = outcome.report {
                                    shared.edit(|s| {
                                        if report.saved + report.existed > 0 {
                                            s.last_success = Some(Utc::now());
                                        }
                                    });
                                    let quiet = automatic
                                        && report.saved == 0
                                        && report.missing == 0
                                        && !batch.wallpaper_updated
                                        && outcome.warning.is_none();
                                    batch.report = report.clone();
                                    if !quiet {
                                        let result =
                                            outcome.warning.as_deref().unwrap_or("抓取完成");
                                        shared.record(
                                            batch.summary(result),
                                            report.missing > 0 || outcome.warning.is_some(),
                                        );
                                    }
                                    shared.edit(|s| {
                                        s.status = if report.missing > 0 {
                                            "本次抓取完成，存在缺失帧"
                                        } else {
                                            "本次抓取完成"
                                        }
                                        .into();
                                        s.error = report.missing > 0;
                                    });
                                } else {
                                    shared.edit(|s| s.status = "原图校验通过".into());
                                }
                                if let Some(warning) = outcome.warning {
                                    shared.edit(|s| {
                                        s.status = warning;
                                        s.error = true;
                                    });
                                }
                                if automatic {
                                    failures = 0;
                                    if enabled {
                                        due = Some(Instant::now() + interval);
                                    }
                                }
                            }
                            Err(error) => {
                                let fault = classify(&error);
                                let mut message = format!("任务失败：{error:#}");
                                if automatic && enabled && fault == Fault::Transient {
                                    failures = failures.saturating_add(1);
                                    let delay = (timing.retry
                                        * 2u32.pow(failures.saturating_sub(1).min(5)))
                                    .min(Duration::from_secs(300));
                                    let delay = error
                                        .downcast_ref::<crate::net::RetryDelay>()
                                        .map_or(delay, |v| delay.max(Duration::from_secs(v.0)));
                                    due = Some(Instant::now() + delay);
                                    message
                                        .push_str(&format!("；{} 秒后自动重试", delay.as_secs()));
                                } else if automatic {
                                    enabled = false;
                                    prefs.automatic = false;
                                    due = None;
                                    gate.lock().unwrap_or_else(|p| p.into_inner()).enabled = false;
                                    save(&prefs);
                                    message.push_str("；定时已停止，处理后点击开始抓取");
                                }
                                shared.log(batch.summary(&message), true);
                            }
                        }
                    }
                }
                if exiting && active.is_none() {
                    break;
                }
                let wait = due.map_or(timing.poll, |t| {
                    t.saturating_duration_since(Instant::now()).min(timing.poll)
                });
                // Avoid a busy loop while a manual/history job owns the single worker.
                let wait = if active.is_some() { timing.poll } else { wait };
                let incoming = followup
                    .take()
                    .map(Ok)
                    .unwrap_or_else(|| rx.recv_timeout(wait));
                match incoming {
                    Ok(command) => match match command {
                        Command::Toggle => Command::SetEnabled(!enabled),
                        other => other,
                    } {
                        Command::Configure(p, then, reply) => {
                            let editable = capture_editable(
                                enabled,
                                active.is_some() || queued.is_some(),
                                exiting,
                            );
                            let result = if exiting {
                                Err(anyhow::anyhow!("程序正在退出，未接受修改"))
                            } else {
                                accept_settings(&data, &prefs, p, editable)
                            };
                            let error = match result {
                                Ok(next) => {
                                    let wallpaper_changed = next.zoom != prefs.zoom
                                        || next.apply_automatic != prefs.apply_automatic;
                                    prefs = next;
                                    shared.set_log_directory(&prefs.photos);
                                    if wallpaper_changed {
                                        let mut g = gate.lock().unwrap_or_else(|p| p.into_inner());
                                        g.apply = prefs.apply_automatic;
                                        g.zoom = prefs.zoom;
                                        g.revision += 1;
                                    }
                                    followup = then.map(|action| match action {
                                        AfterSave::Start => Command::SetEnabled(true),
                                        AfterSave::Capture(selection, refresh) => {
                                            Command::Capture(selection, refresh)
                                        }
                                    });
                                    shared.feedback("设置已保存", false);
                                    None
                                }
                                Err(e) => {
                                    let error = format!("设置未保存：{e:#}");
                                    shared.feedback(&error, true);
                                    Some(error)
                                }
                            };
                            shared.edit(|s| s.preferences = Some(prefs.clone()));
                            let _ = reply.send(ConfigReply {
                                preferences: prefs.clone(),
                                error,
                            });
                        }
                        Command::SetEnabled(value) => {
                            enabled = value;
                            prefs.automatic = value;
                            failures = 0;
                            due = value.then(Instant::now);
                            {
                                let mut g = gate.lock().unwrap_or_else(|p| p.into_inner());
                                g.enabled = value;
                                g.revision += 1;
                            }
                            save(&prefs);
                            shared.log(
                                if value {
                                    "定时抓取已开始"
                                } else {
                                    "定时抓取已停止；当前任务可单独取消"
                                },
                                false,
                            );
                        }
                        Command::Toggle => unreachable!("toggle is normalized above"),
                        Command::Capture(selection, refresh) => {
                            if capture_editable(
                                enabled,
                                active.is_some() || queued.is_some(),
                                exiting,
                            ) {
                                queued = Some(Job {
                                    prefs: prefs.clone(),
                                    data: data.clone(),
                                    selection: Some(selection),
                                    refresh,
                                    automatic: false,
                                    load: None,
                                });
                            } else {
                                shared.log("已有任务执行中，请等待或取消当前任务", false);
                            }
                        }
                        Command::CheckNow => {
                            if enabled {
                                due = Some(Instant::now());
                            } else if active.is_none() {
                                queued = Some(Job {
                                    prefs: prefs.clone(),
                                    data: data.clone(),
                                    selection: Some(Selection::Latest(prefs.grid)),
                                    refresh: false,
                                    automatic: false,
                                    load: None,
                                });
                            }
                        }
                        Command::Cancel => {
                            if let Some((s, _)) = &active {
                                s.cancel();
                                shared.edit(|s| s.status = "正在取消，等待安全退出…".into());
                            }
                        }
                        Command::Resume => {
                            // Wake only once; do not enqueue missed observations.
                            if enabled {
                                due = Some(Instant::now());
                            }
                            if active_automatic && let Some((s, _)) = &active {
                                s.cancel();
                            }
                            shared.log("系统已唤醒，等待网络恢复后检查最新影像", false);
                        }
                        Command::Record(message, error) => shared.feedback(message, error),
                        Command::Shutdown => {
                            shared.record("正在退出程序", false);
                            save(&prefs);
                            exiting = true;
                            enabled = false;
                            due = None;
                            queued = None;
                            gate.lock().unwrap_or_else(|p| p.into_inner()).enabled = false;
                            if let Some((s, _)) = &active {
                                s.cancel();
                            }
                            shared.edit(|s| s.exiting = true);
                        }
                    },
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        exiting = true;
                        enabled = false;
                        queued = None;
                        if let Some((s, _)) = &active {
                            s.cancel();
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if !exiting && active.is_none() {
                    if queued.is_none() && enabled && due.is_some_and(|t| Instant::now() >= t) {
                        queued = Some(Job {
                            prefs: prefs.clone(),
                            data: data.clone(),
                            selection: Some(Selection::Latest(prefs.grid)),
                            refresh: false,
                            automatic: true,
                            load: None,
                        });
                        due = None;
                    }
                    if let Some(job) = queued.take() {
                        let automatic = job.automatic;
                        let stop = Stop::default();
                        let ws = stop.clone();
                        active_automatic = automatic;
                        let runner = runner.clone();
                        let gate = gate.clone();
                        let shared = shared.clone();
                        let done = done_tx.clone();
                        shared
                            .accounting
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .batch = Batch::default();
                        shared.edit(|s| {
                            s.busy = true;
                            s.error = false;
                            s.phase = if job.load.is_some() {
                                Phase::Reading
                            } else {
                                Phase::Checking
                            };
                            s.status = s.phase.label().into();
                        });
                        let join = thread::spawn(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    runner(job, &ws, &gate, &|n| shared.notice(n))
                                }));
                            let result = match result {
                                Ok(v) => v,
                                Err(_) => Err(anyhow::anyhow!("后台任务异常退出；旧照片保留")),
                            };
                            let _ = done.send((result, automatic));
                            (shared.wake)();
                        });
                        active = Some((stop, join));
                    }
                }
                {
                    let mut s = shared.value.lock().unwrap_or_else(|p| p.into_inner());
                    s.enabled = enabled;
                    s.retrying = enabled && failures > 0;
                    s.preferences = Some(prefs.clone());
                    s.next_check = due.map(|d| {
                        Utc::now()
                            + chrono::Duration::from_std(
                                d.saturating_duration_since(Instant::now()),
                            )
                            .unwrap_or_default()
                    });
                }
            }
            shared.record("程序已退出", false);
            shared.edit(|s| {
                s.enabled = false;
                s.busy = false;
                s.exiting = true;
                s.exited = true;
                s.next_check = None;
            });
        });
        Self {
            handle,
            join: Some(join),
        }
    }
}
fn real_job(
    job: Job,
    stop: &Stop,
    gate: &Arc<Mutex<Gate>>,
    notify: &(dyn Fn(Notice) + Sync),
) -> Result<Outcome> {
    let mut out = Outcome::default();
    let path = if let Some(selection) = job.selection {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let trim_latest = matches!(selection, Selection::Latest(_));
        let report = runtime.block_on(engine::run(
            Source::new()?,
            selection,
            &job.prefs.photos,
            job.refresh,
            stop,
            notify,
        ))?;
        if trim_latest
            && report.saved > 0
            && job.prefs.photo_limit > 0
            && let Some(keep) = &report.last
            && let Err(e) =
                storage::trim(&job.prefs.photos, job.prefs.photo_limit, keep, stop, || {
                    notify(Notice::Pruned)
                })
        {
            stop.check()?;
            out.warning = Some(format!("原图已保存，清理旧图失败：{e:#}"));
        }
        let last = report.last.clone();
        out.report = Some(report);
        last
    } else {
        job.load
    };
    let Some(path) = path else { return Ok(out) };
    notify(Notice::Stage(Phase::Reading));
    let frame = storage::load(&path, stop)?;
    if job.automatic {
        let (allowed, zoom, revision) = {
            let g = gate.lock().unwrap_or_else(|p| p.into_inner());
            (g.enabled && g.apply, g.zoom, g.revision)
        };
        if allowed {
            let result = (|| -> Result<()> {
                let screen = desktop::primary_size()?;
                let key = (frame.fingerprint(stop)?, zoom, screen);
                if gate
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .last_applied
                    .as_ref()
                    == Some(&key)
                {
                    return Ok(());
                }
                notify(Notice::Stage(Phase::Wallpaper));
                let pixels = imaging::compose(&frame, screen.0, screen.1, zoom, stop)?;
                let mut g = gate.lock().unwrap_or_else(|p| p.into_inner());
                if !g.enabled || !g.apply || g.revision != revision {
                    return Ok(());
                }
                let bmp = job.data.join("wallpaper-A.bmp");
                storage::write_bmp(&bmp, &pixels, true, stop)?;
                stop.publish(|| desktop::apply(&bmp))
                    .context("设置桌面壁纸失败")?;
                g.last_applied = Some(key);
                notify(Notice::WallpaperUpdated);
                Ok(())
            })();
            if let Err(e) = result {
                stop.check()?;
                out.warning = Some(format!(
                    "{}原图已保存，壁纸未更新：{e:#}",
                    out.warning
                        .take()
                        .map(|w| format!("{w}；"))
                        .unwrap_or_default()
                ));
            }
        }
    }
    let pixels = image::imageops::thumbnail(frame.pixels(), 1100, 1100);
    stop.check()?;
    out.preview = Some(Arc::new(Preview {
        pixels,
        observation: frame.observation().clone(),
        bytes: std::fs::metadata(&path)?.len(),
        format: if path.extension().is_some_and(|e| e == "webp") {
            "WebP 无损"
        } else {
            "PNG"
        }
        .into(),
        path,
    }));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    #[test]
    fn session_totals_survive_cancel_and_error_without_counting_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let runner: Runner = Arc::new(move |_, stop, _, notify| {
            let n = count.fetch_add(1, SeqCst);
            let mut observation =
                Observation::new("2026-10-23T12:00:00Z".parse().unwrap(), 1).unwrap();
            notify(Notice::Frame(observation.clone()));
            notify(Notice::Stage(Phase::Downloading));
            notify(Notice::Saved {
                path: format!("frame-{n}.webp").into(),
                existed: false,
            });
            notify(Notice::Saved {
                path: "old.png".into(),
                existed: true,
            });
            notify(Notice::Missing(observation.clone()));
            observation.satellite = "H09".into();
            notify(Notice::Missing(observation));
            if n == 0 {
                while !stop.is_cancelled() {
                    thread::sleep(Duration::from_millis(2));
                }
                stop.check()?;
            }
            Err(std::io::Error::other("test write failure").into())
        });
        let mut service = Service::spawn_with(
            dir.path().into(),
            Preferences::new(dir.path()),
            None,
            Arc::new(|| {}),
            runner,
            timing(10000),
        );
        service
            .handle
            .send(Command::Capture(Selection::Latest(1), false));
        until(|| service.handle.snapshot().missing == 1);
        assert_eq!(service.handle.snapshot().saved, 1);
        assert_eq!(service.handle.snapshot().activity(), Activity::Working);
        assert_eq!(
            service.handle.snapshot().logs.len(),
            1,
            "stages must not append logs"
        );
        service.handle.send(Command::Cancel);
        until(|| service.handle.snapshot().status.contains("已取消"));
        service
            .handle
            .send(Command::Capture(Selection::Latest(1), false));
        until(|| service.handle.snapshot().error);
        let snap = service.handle.snapshot();
        assert_eq!((snap.saved, snap.missing), (2, 1));
        assert_eq!(snap.activity(), Activity::Stopped);
        assert_eq!(
            snap.logs.len(),
            3,
            "startup plus one line per finished attempt"
        );
        assert!(snap.logs.back().unwrap().contains("新增 1｜已有 1｜缺失 2"));
        service.shutdown();
        let mut restarted = Service::spawn_with(
            dir.path().into(),
            Preferences::new(dir.path()),
            None,
            Arc::new(|| {}),
            Arc::new(|_, _, _, _| success()),
            timing(10000),
        );
        assert_eq!(
            (
                restarted.handle.snapshot().saved,
                restarted.handle.snapshot().missing
            ),
            (0, 0)
        );
        restarted.shutdown();
    }
    #[test]
    fn automatic_checks_are_quiet_but_wallpaper_updates_are_logged() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let runner: Runner = Arc::new(move |_, _, _, notify| {
            let n = c.fetch_add(1, SeqCst);
            notify(Notice::Stage(Phase::Reading));
            notify(Notice::Saved {
                path: "existing.webp".into(),
                existed: true,
            });
            if n == 1 {
                notify(Notice::WallpaperUpdated);
            }
            Ok(Outcome {
                report: Some(Report {
                    planned: 1,
                    existed: 1,
                    ..Default::default()
                }),
                ..Default::default()
            })
        });
        let mut prefs = Preferences::new(dir.path());
        prefs.automatic = true;
        let mut service = Service::spawn_with(
            dir.path().into(),
            prefs,
            None,
            Arc::new(|| {}),
            runner,
            timing(10000),
        );
        until(|| service.handle.snapshot().last_check.is_some());
        assert_eq!(service.handle.snapshot().logs.len(), 1);
        service.handle.send(Command::CheckNow);
        until(|| {
            service
                .handle
                .snapshot()
                .logs
                .iter()
                .any(|s| s.contains("壁纸已更新"))
        });
        assert_eq!(service.handle.snapshot().logs.len(), 2);
        assert_eq!(service.handle.snapshot().saved, 0);
        service
            .handle
            .send(Command::Capture(Selection::Latest(1), false));
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            calls.load(SeqCst),
            2,
            "timer waiting must reject manual capture"
        );
        service.handle.send(Command::SetEnabled(false));
        until(|| !service.handle.snapshot().enabled);
        service.shutdown();
    }
    #[test]
    fn retry_indicator_clears_after_recovery_and_stop() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let runner: Runner = Arc::new(move |_, _, _, _| {
            if c.fetch_add(1, SeqCst) == 0 {
                Err(HttpFailure(503).into())
            } else {
                success()
            }
        });
        let mut prefs = Preferences::new(dir.path());
        prefs.automatic = true;
        let mut schedule = timing(10000);
        schedule.retry = Duration::from_millis(150);
        let mut service = Service::spawn_with(
            dir.path().into(),
            prefs,
            None,
            Arc::new(|| {}),
            runner,
            schedule,
        );
        until(|| service.handle.snapshot().activity() == Activity::Retrying);
        until(|| service.handle.snapshot().last_success.is_some());
        until(|| service.handle.snapshot().activity() == Activity::Waiting);
        service.handle.send(Command::SetEnabled(false));
        until(|| service.handle.snapshot().activity() == Activity::Stopped);
        assert!(!service.handle.snapshot().retrying);
        service.shutdown();
    }
    #[test]
    fn settings_commit_locks_capture_but_allows_window_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let original = Preferences::new(dir.path());
        let release = Arc::new(AtomicBool::new(false));
        let worker_release = release.clone();
        let (job_tx, job_rx) = mpsc::channel();
        let runner: Runner = Arc::new(move |job, stop, _, _| {
            job_tx.send(job.prefs).unwrap();
            while !worker_release.load(SeqCst) {
                stop.check()?;
                thread::sleep(Duration::from_millis(2));
            }
            success()
        });
        let mut service = Service::spawn_with(
            dir.path().into(),
            original.clone(),
            None,
            Arc::new(|| {}),
            runner,
            timing(10000),
        );
        let mut proposed = original.clone();
        proposed.zoom = 75;
        let reply = service
            .handle
            .configure(proposed.clone(), Some(AfterSave::Start))
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(reply.error.is_none());
        assert_eq!(
            job_rx.recv_timeout(Duration::from_secs(3)).unwrap().zoom,
            75
        );
        until(|| service.handle.snapshot().enabled && service.handle.snapshot().busy);
        assert_eq!(settings::load(dir.path()).0.zoom, 75);

        proposed.zoom = 80;
        let reply = service
            .handle
            .configure(proposed.clone(), None)
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(reply.error.is_some());
        assert_eq!(reply.preferences.zoom, 75);
        assert_eq!(settings::load(dir.path()).0.zoom, 75);

        proposed = reply.preferences;
        proposed.close_to_tray = false;
        let reply = service
            .handle
            .configure(proposed.clone(), None)
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(reply.error.is_none());
        assert!(!reply.preferences.close_to_tray);
        // Tray's Toggle and window's SetEnabled must reach the same transition.
        service.handle.send(Command::Toggle);
        until(|| !service.handle.snapshot().enabled);
        assert!(service.handle.snapshot().busy);
        assert!(!service.handle.snapshot().can_configure_capture());
        proposed.zoom = 80;
        assert!(
            service
                .handle
                .configure(proposed.clone(), None)
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .error
                .is_some()
        );
        release.store(true, SeqCst);
        until(|| service.handle.snapshot().can_configure_capture());
        let reply = service
            .handle
            .configure(proposed, Some(AfterSave::Start))
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(reply.error.is_none());
        assert_eq!(
            job_rx.recv_timeout(Duration::from_secs(3)).unwrap().zoom,
            80
        );
        until(|| service.handle.snapshot().enabled && !service.handle.snapshot().busy);
        assert!(!service.handle.snapshot().can_configure_capture());
        let mut forbidden = reply.preferences;
        forbidden.grid = 8;
        assert!(
            service
                .handle
                .configure(forbidden, None)
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .error
                .is_some()
        );
        service.handle.send(Command::SetEnabled(false));
        until(|| service.handle.snapshot().can_configure_capture());
        service.shutdown();
        let (saved, warning) = settings::load(dir.path());
        assert!(warning.is_none());
        assert_eq!(saved.zoom, 80);
        assert!(!saved.close_to_tray && !saved.automatic);
    }

    #[test]
    fn rejected_settings_never_start_followup_or_replace_accepted_values() {
        let dir = tempfile::tempdir().unwrap();
        let original = Preferences::new(dir.path());
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let mut service = Service::spawn_with(
            dir.path().into(),
            original.clone(),
            None,
            Arc::new(|| {}),
            Arc::new(move |_, _, _, _| {
                n.fetch_add(1, SeqCst);
                success()
            }),
            timing(10000),
        );
        let bad_directory = dir.path().join("a-file");
        std::fs::write(&bad_directory, b"keep").unwrap();
        let mut invalid = original.clone();
        invalid.photos = bad_directory.clone();
        let reply = service
            .handle
            .configure(invalid, Some(AfterSave::Start))
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(reply.error.is_some());
        assert_eq!(reply.preferences, original);
        assert_eq!(std::fs::read(&bad_directory).unwrap(), b"keep");
        std::fs::create_dir(dir.path().join("settings.json")).unwrap();
        let mut changed = original.clone();
        changed.zoom = 50;
        let reply = service
            .handle
            .configure(changed, Some(AfterSave::Start))
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(reply.error.unwrap().contains("保存失败"));
        assert_eq!(reply.preferences, original);
        service.shutdown();
        assert_eq!(calls.load(SeqCst), 0);
    }

    #[test]
    fn settings_commit_then_immediate_shutdown_preserves_new_values() {
        let dir = tempfile::tempdir().unwrap();
        let original = Preferences::new(dir.path());
        let mut service = Service::spawn_with(
            dir.path().into(),
            original.clone(),
            None,
            Arc::new(|| {}),
            Arc::new(|_, _, _, _| success()),
            timing(10000),
        );
        let mut proposed = original;
        proposed.zoom = 65;
        proposed.interval_minutes = 20;
        let result = service.handle.configure(proposed.clone(), None);
        service.shutdown();
        assert!(
            result
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .error
                .is_none()
        );
        assert_eq!(settings::load(dir.path()).0, proposed);
    }
    fn until(mut test: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !test() {
            assert!(Instant::now() < end, "background condition timed out");
            thread::sleep(Duration::from_millis(3));
        }
    }
    fn timing(interval: u64) -> Timing {
        Timing {
            retry: Duration::from_millis(15),
            poll: Duration::from_millis(3),
            interval_override: Some(Duration::from_millis(interval)),
        }
    }
    fn success() -> Result<Outcome> {
        Ok(Outcome {
            report: Some(Report {
                planned: 1,
                saved: 1,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    #[test]
    fn feedback_confirms_disk_save_and_surfaces_failure() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let p = Preferences::new(&data);
        let mut service = Service::spawn_with(
            data.clone(),
            p.clone(),
            None,
            Arc::new(|| {}),
            Arc::new(|_, _, _, _| success()),
            timing(10000),
        );
        let _ = service.handle.configure(p.clone(), None);
        until(|| service.handle.snapshot().feedback.is_some());
        let feedback = service.handle.snapshot().feedback.unwrap();
        assert!(!feedback.error);
        assert_eq!(feedback.message, "设置已保存");
        assert_eq!(settings::load(&data).0.zoom, p.zoom);
        // A directory at the target file models a failed atomic replacement.
        std::fs::remove_file(data.join("settings.json")).unwrap();
        std::fs::create_dir(data.join("settings.json")).unwrap();
        let _ = service.handle.configure(p, None);
        until(|| service.handle.snapshot().feedback.is_some_and(|f| f.error));
        assert!(
            service
                .handle
                .snapshot()
                .feedback
                .unwrap()
                .message
                .contains("保存失败")
        );
        service.shutdown();
    }
    #[test]
    fn journal_failure_does_not_stop_capture_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Preferences::new(dir.path());
        p.automatic = true;
        p.photos = dir.path().join("blocked");
        std::fs::write(&p.photos, b"preserve").unwrap();
        let mut service = Service::spawn_with(
            dir.path().into(),
            p.clone(),
            None,
            Arc::new(|| {}),
            Arc::new(|_, _, _, _| success()),
            timing(10000),
        );
        until(|| service.handle.snapshot().last_success.is_some());
        assert!(service.handle.snapshot().enabled);
        assert!(service.handle.snapshot().log_error.is_some());
        until(|| !service.handle.snapshot().busy);
        service.handle.send(Command::SetEnabled(false));
        until(|| service.handle.snapshot().can_configure_capture());
        p.photos = dir.path().join("photos");
        let _ = service.handle.configure(p.clone(), None);
        until(|| service.handle.snapshot().log_error.is_none());
        let path = p.photos.join(format!(
            "运行记录-{}.log",
            journal::beijing(Utc::now()).format("%Y-%m-%d")
        ));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("设置已保存")
        );
        std::fs::remove_file(&path).unwrap();
        service
            .handle
            .send(Command::Record("删除后继续记录".into(), false));
        until(|| path.exists());
        until(|| {
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("删除后继续记录")
        });
        service.shutdown();
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("程序已退出")
        );
        assert_eq!(
            std::fs::read(dir.path().join("blocked")).unwrap(),
            b"preserve"
        );
    }
    #[test]
    fn schedule_runs_without_gui_and_stop_preserves_manual_capture() {
        let dir = tempfile::tempdir().unwrap();
        let p = Preferences::new(dir.path());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let c = calls.clone();
        let runner: Runner = Arc::new(move |job, _, _, _| {
            c.lock().unwrap().push(job.automatic);
            success()
        });
        let mut service = Service::spawn_with(
            dir.path().into(),
            p,
            None,
            Arc::new(|| {}),
            runner,
            timing(30),
        );
        service.handle.send(Command::SetEnabled(true));
        until(|| calls.lock().unwrap().len() >= 3);
        service.handle.send(Command::SetEnabled(false));
        until(|| {
            let s = service.handle.snapshot();
            !s.enabled && !s.busy
        });
        let count = calls.lock().unwrap().len();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.lock().unwrap().len(), count);
        service
            .handle
            .send(Command::Capture(Selection::Latest(1), false));
        until(|| calls.lock().unwrap().len() > count);
        assert!(!calls.lock().unwrap().last().unwrap());
        service.shutdown();
        assert!(service.handle.snapshot().exited);
    }
    #[test]
    fn manual_job_serializes_auto_and_stop_closes_wallpaper_gate() {
        let dir = tempfile::tempdir().unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let jobs = Arc::new(AtomicUsize::new(0));
        let gate_seen = Arc::new(Mutex::new(None));
        let r = release.clone();
        let j = jobs.clone();
        let seen = gate_seen.clone();
        let runner: Runner = Arc::new(move |_, stop, gate, _| {
            j.fetch_add(1, SeqCst);
            while !r.load(SeqCst) {
                stop.check()?;
                thread::sleep(Duration::from_millis(2));
            }
            *seen.lock().unwrap() = Some(gate.lock().unwrap().enabled);
            success()
        });
        let mut service = Service::spawn_with(
            dir.path().into(),
            Preferences::new(dir.path()),
            None,
            Arc::new(|| {}),
            runner,
            timing(10),
        );
        service
            .handle
            .send(Command::Capture(Selection::Latest(1), false));
        until(|| jobs.load(SeqCst) == 1);
        service.handle.send(Command::SetEnabled(true));
        until(|| service.handle.snapshot().enabled);
        thread::sleep(Duration::from_millis(80));
        assert_eq!(jobs.load(SeqCst), 1);
        service.handle.send(Command::SetEnabled(false));
        until(|| !service.handle.snapshot().enabled);
        release.store(true, SeqCst);
        until(|| !service.handle.snapshot().busy);
        assert_eq!(*gate_seen.lock().unwrap(), Some(false));
        thread::sleep(Duration::from_millis(60));
        assert_eq!(jobs.load(SeqCst), 1);
        service.shutdown();
    }
    #[test]
    fn transient_failures_retry_and_recover() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Preferences::new(dir.path());
        p.automatic = true;
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let runner: Runner = Arc::new(move |_, _, _, _| {
            if c.fetch_add(1, SeqCst) < 2 {
                Err(HttpFailure(503).into())
            } else {
                success()
            }
        });
        let mut s = Service::spawn_with(
            dir.path().into(),
            p,
            None,
            Arc::new(|| {}),
            runner,
            timing(10000),
        );
        until(|| s.handle.snapshot().last_success.is_some());
        assert_eq!(calls.load(SeqCst), 3);
        assert!(s.handle.snapshot().enabled);
        assert!(
            s.handle
                .snapshot()
                .logs
                .iter()
                .any(|l| l.contains("自动重试"))
        );
        s.shutdown();
    }
    #[test]
    fn disk_error_stops_automatic_without_retry_storm() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Preferences::new(dir.path());
        p.automatic = true;
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let runner: Runner = Arc::new(move |_, _, _, _| {
            c.fetch_add(1, SeqCst);
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "disk denied").into())
        });
        let mut s = Service::spawn_with(
            dir.path().into(),
            p,
            None,
            Arc::new(|| {}),
            runner,
            timing(10),
        );
        until(|| {
            let v = s.handle.snapshot();
            v.error && !v.enabled && !v.busy
        });
        thread::sleep(Duration::from_millis(80));
        assert_eq!(calls.load(SeqCst), 1);
        assert!(!s.handle.snapshot().enabled);
        s.shutdown();
    }
    #[test]
    fn missing_preload_does_not_disable_automatic() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Preferences::new(dir.path());
        p.automatic = true;
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let runner: Runner = Arc::new(move |job, _, _, _| {
            c.fetch_add(1, SeqCst);
            if job.load.is_some() {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound).into())
            } else {
                success()
            }
        });
        let mut s = Service::spawn_with(
            dir.path().into(),
            p,
            Some(dir.path().join("missing.png")),
            Arc::new(|| {}),
            runner,
            timing(10000),
        );
        until(|| s.handle.snapshot().last_success.is_some());
        assert_eq!(calls.load(SeqCst), 2);
        assert!(s.handle.snapshot().enabled);
        s.shutdown();
    }
    #[test]
    fn resume_cancels_stale_auto_then_checks_once_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Preferences::new(dir.path());
        p.automatic = true;
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let runner: Runner = Arc::new(move |job, stop, _, _| {
            assert!(matches!(job.selection, Some(Selection::Latest(_))));
            if c.fetch_add(1, SeqCst) == 0 {
                while !stop.is_cancelled() {
                    thread::sleep(Duration::from_millis(2));
                }
                stop.check()?;
            }
            success()
        });
        let mut s = Service::spawn_with(
            dir.path().into(),
            p,
            None,
            Arc::new(|| {}),
            runner,
            timing(10000),
        );
        until(|| calls.load(SeqCst) == 1);
        s.handle.send(Command::Resume);
        until(|| s.handle.snapshot().last_success.is_some());
        thread::sleep(Duration::from_millis(80));
        assert_eq!(calls.load(SeqCst), 2);
        s.shutdown();
        assert!(
            settings::load(dir.path()).0.automatic,
            "exit must preserve enabled preference"
        );
    }
    #[test]
    fn shutdown_cancels_and_joins_worker() {
        let dir = tempfile::tempdir().unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let a = started.clone();
        let b = finished.clone();
        let runner: Runner = Arc::new(move |_, stop, _, _| {
            a.store(true, SeqCst);
            while !stop.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            b.store(true, SeqCst);
            stop.check()?;
            success()
        });
        let mut s = Service::spawn_with(
            dir.path().into(),
            Preferences::new(dir.path()),
            None,
            Arc::new(|| {}),
            runner,
            timing(20),
        );
        s.handle.send(Command::Capture(Selection::Latest(1), false));
        until(|| started.load(SeqCst));
        s.shutdown();
        assert!(finished.load(SeqCst));
        assert!(s.handle.snapshot().exited);
    }

    #[test]
    #[ignore = "Changes the current desktop briefly, restores its paths and position; requires EARTHLIVE_TEST_SOURCE"]
    fn real_wallpaper_pipeline_restores_desktop() -> Result<()> {
        use windows::{
            Win32::{
                System::Com::*,
                UI::Shell::{DESKTOP_WALLPAPER_POSITION, DesktopWallpaper, IDesktopWallpaper},
            },
            core::{HSTRING, PCWSTR, PWSTR},
        };
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe {
                    CoUninitialize();
                }
            }
        }
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        }
        let _apartment = Apartment;
        unsafe fn owned(value: PWSTR) -> Result<String> {
            let result = unsafe { value.to_string() };
            unsafe {
                CoTaskMemFree(Some(value.0.cast()));
            }
            Ok(result?)
        }
        struct Restore {
            desktop: IDesktopWallpaper,
            position: DESKTOP_WALLPAPER_POSITION,
            monitors: Vec<(String, String)>,
        }
        impl Restore {
            fn restore(&self) -> Result<()> {
                unsafe {
                    self.desktop.SetPosition(self.position)?;
                    for (id, path) in &self.monitors {
                        self.desktop
                            .SetWallpaper(&HSTRING::from(id), &HSTRING::from(path))?;
                    }
                }
                Ok(())
            }
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = self.restore();
            }
        }
        let desktop: IDesktopWallpaper =
            unsafe { CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)? };
        let mut monitors = Vec::new();
        unsafe {
            for n in 0..desktop.GetMonitorDevicePathCount()? {
                let id = owned(desktop.GetMonitorDevicePathAt(n)?)?;
                let path = owned(desktop.GetWallpaper(&HSTRING::from(&id))?)?;
                monitors.push((id, path));
            }
        }
        let restore = Restore {
            position: unsafe { desktop.GetPosition()? },
            desktop,
            monitors,
        };
        let source = PathBuf::from(
            std::env::var_os("EARTHLIVE_TEST_SOURCE").context("test source not set")?,
        );
        let dir = tempfile::tempdir()?;
        let mut prefs = Preferences::new(dir.path());
        prefs.zoom = 17;
        let gate = Arc::new(Mutex::new(Gate {
            enabled: true,
            apply: true,
            zoom: 17,
            ..Default::default()
        }));
        let out = real_job(
            Job {
                prefs,
                data: dir.path().into(),
                selection: None,
                refresh: false,
                automatic: true,
                load: Some(source),
            },
            &Stop::default(),
            &gate,
            &|_| {},
        )?;
        anyhow::ensure!(out.warning.is_none(), "{:?}", out.warning);
        let current = unsafe { owned(restore.desktop.GetWallpaper(PCWSTR::null())?)? };
        anyhow::ensure!(
            std::fs::canonicalize(current)?
                == std::fs::canonicalize(dir.path().join("wallpaper-A.bmp"))?,
            "wallpaper path mismatch"
        );
        anyhow::ensure!(
            gate.lock().unwrap().last_applied.is_some(),
            "application not recorded"
        );
        restore.restore()?;
        unsafe {
            anyhow::ensure!(
                restore.desktop.GetPosition()? == restore.position,
                "position not restored"
            );
            for (id, path) in &restore.monitors {
                anyhow::ensure!(
                    owned(restore.desktop.GetWallpaper(&HSTRING::from(id))?)? == *path,
                    "monitor wallpaper not restored"
                );
            }
        }
        println!(
            "Real automatic wallpaper composition and Windows application passed; original desktop paths and position restored."
        );
        Ok(())
    }
}
