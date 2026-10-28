use super::{
    AfterSave, Batch, Command, ConfigReply, Fault, Gate, Job, Outcome, Runner, Shared, Timing,
    accept_settings, capture_editable, classify,
};
use crate::{
    domain::Stop,
    engine::{Phase, Report, Selection},
    net::RetryDelay,
    settings::{self, Preferences},
};
use anyhow::{Context, Result};
use chrono::Utc;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

struct ActiveJob {
    stop: Stop,
    join: JoinHandle<()>,
    automatic: bool,
}

// Only this thread changes scheduling state. The GUI/tray observe Shared snapshots;
// the worker receives immutable Job parameters and the existing wallpaper gate.
pub(super) struct Scheduler {
    data: PathBuf,
    prefs: Preferences,
    shared: Arc<Shared>,
    gate: Arc<Mutex<Gate>>,
    rx: mpsc::Receiver<Command>,
    done_tx: Sender<Result<Outcome>>,
    done_rx: mpsc::Receiver<Result<Outcome>>,
    runner: Runner,
    timing: Timing,
    enabled: bool,
    exiting: bool,
    due: Option<Instant>,
    failures: u32,
    queued: Option<Job>,
    active: Option<ActiveJob>,
    followup: Option<Command>,
}
impl Scheduler {
    pub(super) fn new(
        data: PathBuf,
        prefs: Preferences,
        load: Option<PathBuf>,
        shared: Arc<Shared>,
        rx: mpsc::Receiver<Command>,
        runner: Runner,
        timing: Timing,
    ) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        let enabled = prefs.automatic;
        let gate = Arc::new(Mutex::new(Gate {
            enabled,
            apply: prefs.apply_automatic,
            zoom: prefs.zoom,
            ..Default::default()
        }));
        let queued = load.map(|path| Job::preload(prefs.clone(), data.clone(), path));
        Self {
            data,
            prefs,
            shared,
            gate,
            rx,
            done_tx,
            done_rx,
            runner,
            timing,
            enabled,
            exiting: false,
            due: enabled.then(Instant::now),
            failures: 0,
            queued,
            active: None,
            followup: None,
        }
    }

    pub(super) fn run(&mut self) -> Result<()> {
        self.shared.record(
            format!(
                "程序启动 · {} {}",
                crate::APP_NAME,
                crate::display_version()
            ),
            false,
        );
        loop {
            while let Ok(result) = self.done_rx.try_recv() {
                self.complete_job(result)?;
            }
            if self.exiting && self.active.is_none() {
                return Ok(());
            }
            match self.next_command() {
                Ok(command) => self.handle_command(command),
                Err(mpsc::RecvTimeoutError::Disconnected) => self.begin_shutdown(),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.start_queued();
            self.publish_state();
        }
    }

    fn next_command(&mut self) -> std::result::Result<Command, mpsc::RecvTimeoutError> {
        if let Some(command) = self.followup.take() {
            return Ok(command);
        }
        // An overdue automatic check must not spin while a manual job owns the worker.
        let wait = if self.active.is_some() {
            self.timing.poll
        } else {
            self.due.map_or(self.timing.poll, |due| {
                due.saturating_duration_since(Instant::now())
                    .min(self.timing.poll)
            })
        };
        self.rx.recv_timeout(wait)
    }

    fn save_preferences(&self) {
        if let Err(error) = settings::save(&self.data, &self.prefs) {
            self.shared
                .feedback(format!("设置保存失败：{error:#}"), true);
        }
    }

    fn interval(&self) -> Duration {
        self.timing.interval_override.unwrap_or(Duration::from_secs(
            u64::from(self.prefs.interval_minutes) * 60,
        ))
    }

    fn schedule_after(&mut self, delay: Duration) -> Result<()> {
        self.due = Some(
            Instant::now()
                .checked_add(delay)
                .context("下次检查时间超出系统范围")?,
        );
        Ok(())
    }

    fn complete_job(&mut self, result: Result<Outcome>) -> Result<()> {
        let active = self.active.take().context("完成通知与当前任务状态不一致")?;
        let cancelled = active.stop.is_cancelled();
        let automatic = active.automatic;
        active
            .join
            .join()
            .map_err(|_| anyhow::anyhow!("后台工作线程异常结束"))?;
        let batch = self
            .shared
            .accounting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .batch
            .clone();
        self.shared.edit(|s| {
            s.busy = false;
            if automatic {
                s.last_check = Some(Utc::now());
            }
        });
        if cancelled {
            self.shared
                .log(batch.summary("已取消，已完成照片保留"), false);
            if automatic {
                self.failures = 0;
            }
            if automatic && self.enabled && self.due.is_none() {
                self.schedule_after(self.interval())?;
            }
            return Ok(());
        }
        match result {
            Ok(outcome) => self.complete_success(outcome, batch, automatic),
            Err(error) => self.complete_failure(error, &batch, automatic),
        }
    }

    fn complete_success(
        &mut self,
        outcome: Outcome,
        mut batch: Batch,
        automatic: bool,
    ) -> Result<()> {
        if let Some(preview) = outcome.preview {
            self.prefs.last = Some(preview.path.clone());
            self.save_preferences();
            self.shared.edit(|s| s.preview = Some(preview));
        }
        if let Some(report) = outcome.report {
            self.record_report(report, &mut batch, outcome.warning.as_deref(), automatic);
        } else {
            self.shared.edit(|s| s.status = "原图校验通过".into());
        }
        if let Some(warning) = outcome.warning {
            self.shared.edit(|s| {
                s.status = warning;
                s.error = true;
            });
        }
        if automatic {
            self.failures = 0;
            if self.enabled {
                self.schedule_after(self.interval())?;
            }
        }
        Ok(())
    }

    fn record_report(
        &self,
        report: Report,
        batch: &mut Batch,
        warning: Option<&str>,
        automatic: bool,
    ) {
        let quiet = automatic
            && report.saved == 0
            && report.missing == 0
            && !batch.wallpaper_updated
            && warning.is_none();
        let successful = report.saved + report.existed > 0;
        let missing = report.missing > 0;
        batch.report = report;
        if !quiet {
            self.shared.record(
                batch.summary(warning.unwrap_or("抓取完成")),
                missing || warning.is_some(),
            );
        }
        self.shared.edit(|s| {
            if successful {
                s.last_success = Some(Utc::now());
            }
            s.status = if missing {
                "本次抓取完成，存在缺失帧"
            } else {
                "本次抓取完成"
            }
            .into();
            s.error = missing;
        });
    }

    fn complete_failure(
        &mut self,
        error: anyhow::Error,
        batch: &Batch,
        automatic: bool,
    ) -> Result<()> {
        let mut message = format!("任务失败：{error:#}");
        if automatic && self.enabled && classify(&error) == Fault::Transient {
            self.failures = self.failures.saturating_add(1);
            let delay = retry_delay(&error, self.failures, self.timing.retry);
            self.schedule_after(delay)?;
            message.push_str(&format!("；{} 秒后自动重试", delay.as_secs()));
        } else if automatic {
            self.enabled = false;
            self.prefs.automatic = false;
            self.due = None;
            self.gate.lock().unwrap_or_else(|p| p.into_inner()).enabled = false;
            self.save_preferences();
            message.push_str("；定时已停止，处理后点击开始抓取");
        }
        self.shared.log(batch.summary(&message), true);
        Ok(())
    }

    fn configure(
        &mut self,
        next: Preferences,
        then: Option<AfterSave>,
        reply: Sender<ConfigReply>,
    ) {
        let editable = capture_editable(
            self.enabled,
            self.active.is_some() || self.queued.is_some(),
            self.exiting,
        );
        let result = if self.exiting {
            Err(anyhow::anyhow!("程序正在退出，未接受修改"))
        } else {
            accept_settings(&self.data, &self.prefs, next, editable)
        };
        let error = match result {
            Ok(next) => {
                if next.zoom != self.prefs.zoom
                    || next.apply_automatic != self.prefs.apply_automatic
                {
                    let mut gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
                    gate.apply = next.apply_automatic;
                    gate.zoom = next.zoom;
                    gate.revision += 1;
                }
                self.prefs = next;
                self.shared.set_log_directory(&self.prefs.photos);
                self.followup = then.map(|action| match action {
                    AfterSave::Start => Command::SetEnabled(true),
                    AfterSave::Capture(selection, refresh) => Command::Capture(selection, refresh),
                });
                self.shared.feedback("设置已保存", false);
                None
            }
            Err(error) => {
                let message = format!("设置未保存：{error:#}");
                self.shared.feedback(&message, true);
                Some(message)
            }
        };
        self.shared
            .edit(|s| s.preferences = Some(self.prefs.clone()));
        let _ = reply.send(ConfigReply {
            preferences: self.prefs.clone(),
            error,
        });
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.prefs.automatic = enabled;
        self.failures = 0;
        self.due = enabled.then(Instant::now);
        {
            let mut gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
            gate.enabled = enabled;
            gate.revision += 1;
        }
        self.save_preferences();
        self.shared.log(
            if enabled {
                "定时抓取已开始"
            } else {
                "定时抓取已停止；当前任务可单独取消"
            },
            false,
        );
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Configure(next, then, reply) => self.configure(next, then, reply),
            Command::Shutdown => self.begin_shutdown(),
            Command::Record(message, error) => self.shared.feedback(message, error),
            _ if self.exiting => {}
            Command::SetEnabled(enabled) => self.set_enabled(enabled),
            Command::Toggle => self.set_enabled(!self.enabled),
            Command::Capture(selection, refresh) => {
                if capture_editable(
                    self.enabled,
                    self.active.is_some() || self.queued.is_some(),
                    false,
                ) {
                    self.queued = Some(Job::manual(
                        self.prefs.clone(),
                        self.data.clone(),
                        selection,
                        refresh,
                    ));
                } else {
                    self.shared
                        .log("已有任务执行中，请等待或取消当前任务", false);
                }
            }
            Command::CheckNow => {
                if self.enabled {
                    self.due = Some(Instant::now());
                } else if self.active.is_none() {
                    self.queued = Some(Job::manual(
                        self.prefs.clone(),
                        self.data.clone(),
                        Selection::Latest(self.prefs.grid),
                        false,
                    ));
                }
            }
            Command::Cancel => {
                if let Some(active) = &self.active {
                    active.stop.cancel();
                    self.shared
                        .edit(|s| s.status = "正在取消，等待安全退出…".into());
                }
            }
            Command::Resume => {
                if self.enabled {
                    self.due = Some(Instant::now());
                }
                if let Some(active) = &self.active
                    && active.automatic
                {
                    active.stop.cancel();
                }
                self.shared
                    .log("系统已唤醒，等待网络恢复后检查最新影像", false);
            }
        }
    }

    fn begin_shutdown(&mut self) {
        if self.exiting {
            return;
        }
        self.shared.record("正在退出程序", false);
        self.save_preferences();
        self.exiting = true;
        self.enabled = false;
        self.due = None;
        self.queued = None;
        self.followup = None;
        self.gate.lock().unwrap_or_else(|p| p.into_inner()).enabled = false;
        if let Some(active) = &self.active {
            active.stop.cancel();
        }
        self.shared.edit(|s| s.exiting = true);
    }

    fn start_queued(&mut self) {
        if self.exiting || self.active.is_some() {
            return;
        }
        if self.queued.is_none()
            && self.enabled
            && self.due.is_some_and(|due| Instant::now() >= due)
        {
            self.queued = Some(Job::automatic(self.prefs.clone(), self.data.clone()));
            self.due = None;
        }
        let Some(job) = self.queued.take() else {
            return;
        };
        let automatic = job.automatic;
        let stop = Stop::default();
        let worker_stop = stop.clone();
        let runner = self.runner.clone();
        let gate = self.gate.clone();
        let shared = self.shared.clone();
        let done = self.done_tx.clone();
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
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runner(job, &worker_stop, &gate, &|notice| shared.notice(notice))
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("后台任务异常退出；旧照片保留")));
            let _ = done.send(result);
            (shared.wake)();
        });
        self.active = Some(ActiveJob {
            stop,
            join,
            automatic,
        });
    }

    fn publish_state(&self) {
        let mut state = self.shared.value.lock().unwrap_or_else(|p| p.into_inner());
        state.enabled = self.enabled;
        state.retrying = self.enabled && self.failures > 0;
        state.preferences = Some(self.prefs.clone());
        state.next_check = self.due.and_then(|due| {
            let wait =
                chrono::Duration::from_std(due.saturating_duration_since(Instant::now())).ok()?;
            Utc::now().checked_add_signed(wait)
        });
    }

    pub(super) fn finish(&mut self, result: Result<()>) {
        self.gate.lock().unwrap_or_else(|p| p.into_inner()).enabled = false;
        if let Some(active) = self.active.take() {
            active.stop.cancel();
            let _ = active.join.join();
        }
        self.exiting |= self
            .rx
            .try_iter()
            .any(|command| matches!(command, Command::Shutdown));
        if let Err(error) = result {
            if self.exiting {
                self.shared.edit(|s| s.exiting = true);
            }
            self.shared
                .failed(format!("后台已停止：{error:#}；请退出后重启程序"));
            return;
        }
        self.shared.record("程序已退出", false);
        self.shared.edit(|s| {
            s.enabled = false;
            s.busy = false;
            s.exiting = true;
            s.exited = true;
            s.next_check = None;
        });
    }
}

fn retry_delay(error: &anyhow::Error, failures: u32, base: Duration) -> Duration {
    let factor = 1u32 << failures.saturating_sub(1).min(5);
    let backoff = base.saturating_mul(factor).min(Duration::from_secs(300));
    error
        .downcast_ref::<RetryDelay>()
        .map_or(backoff, |delay| backoff.max(delay.duration()))
}
