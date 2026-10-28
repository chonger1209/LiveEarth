use super::*;
use crate::net::RetryDelay;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
#[test]
fn session_totals_survive_cancel_and_error_without_counting_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let runner: Runner = Arc::new(move |_, stop, _, notify| {
        let n = count.fetch_add(1, SeqCst);
        let mut observation = Observation::new("2026-10-23T12:00:00Z".parse().unwrap(), 1).unwrap();
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
    let source =
        PathBuf::from(std::env::var_os("EARTHLIVE_TEST_SOURCE").context("test source not set")?);
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

#[test]
fn activity_text_preserves_phase_cancel_and_exit_semantics() {
    let mut snapshot = Snapshot {
        busy: true,
        status: "下载与拼接分片 3/64".into(),
        ..Default::default()
    };
    assert_eq!(snapshot.activity_text(), "下载与拼接分片 3/64");
    snapshot.status = "正在取消，等待安全退出…".into();
    assert_eq!(snapshot.activity_text(), "正在取消，等待安全退出…");
    snapshot.exiting = true;
    assert_eq!(snapshot.activity_text(), "正在退出");
    assert_eq!(snapshot.activity(), Activity::Working);
    snapshot.exiting = false;
    snapshot.busy = false;
    snapshot.enabled = true;
    snapshot.retrying = true;
    assert_eq!(snapshot.activity_text(), "等待重试");
    snapshot.retrying = false;
    assert_eq!(snapshot.activity_text(), "定时抓取中");
    snapshot.enabled = false;
    assert_eq!(snapshot.activity_text(), "已停止");
}

#[test]
fn autostart_replaces_old_location_and_rolls_back_exact_record_on_save_failure() {
    use crate::platform::{replace_named_autostart, set_startup_value, startup_value};
    let name = format!("LiveEarth.Test.Replace.{}", std::process::id());
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = set_startup_value(&self.0, None);
        }
    }
    let _cleanup = Cleanup(name.clone());
    let old = "\"C:/Old/LiveEarth.exe\" --data-dir \"C:/Old/data\" --startup";
    let new = "\"D:/New/LiveEarth.exe\" --data-dir \"E:/New/data\" --startup";
    set_startup_value(&name, Some(old)).unwrap();
    let change = replace_named_autostart(&name, Some(new.into())).unwrap();
    assert_eq!(startup_value(&name).unwrap().as_deref(), Some(new));
    let blocked = tempfile::tempdir().unwrap();
    std::fs::create_dir(blocked.path().join("settings.json")).unwrap();
    let mut preferences = Preferences::new(blocked.path());
    preferences.autostart = true;
    assert!(persist_settings(blocked.path(), &preferences, Some(change)).is_err());
    assert_eq!(startup_value(&name).unwrap().as_deref(), Some(old));
    let good = tempfile::tempdir().unwrap();
    let change = replace_named_autostart(&name, Some(new.into())).unwrap();
    persist_settings(good.path(), &preferences, Some(change)).unwrap();
    assert_eq!(startup_value(&name).unwrap().as_deref(), Some(new));
    let change = replace_named_autostart(&name, None).unwrap();
    assert!(startup_value(&name).unwrap().is_none());
    change.rollback().unwrap();
    assert_eq!(startup_value(&name).unwrap().as_deref(), Some(new));
    replace_named_autostart(&name, None).unwrap();
    assert!(startup_value(&name).unwrap().is_none());
}

#[test]
fn autostart_rollback_does_not_overwrite_a_newer_choice() {
    use crate::platform::{replace_named_autostart, set_startup_value, startup_value};
    let name = format!("LiveEarth.Test.Newer.{}", std::process::id());
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = set_startup_value(&self.0, None);
        }
    }
    let _cleanup = Cleanup(name.clone());
    let change = replace_named_autostart(&name, Some("first choice".into())).unwrap();
    set_startup_value(&name, Some("newer choice")).unwrap();
    assert!(change.rollback().is_err());
    assert_eq!(
        startup_value(&name).unwrap().as_deref(),
        Some("newer choice")
    );
}

#[test]
fn extreme_retry_after_keeps_scheduler_alive_and_can_be_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let mut prefs = Preferences::new(dir.path());
    prefs.automatic = true;
    let runner: Runner = Arc::new(|_, _, _, _| {
        Err(anyhow::Error::new(HttpFailure(429)).context(RetryDelay(u64::MAX)))
    });
    let mut service = Service::spawn_with(
        dir.path().into(),
        prefs,
        None,
        Arc::new(|| {}),
        runner,
        timing(10000),
    );
    until(|| service.handle.snapshot().retrying);
    let snapshot = service.handle.snapshot();
    assert!(snapshot.enabled && !snapshot.exited);
    let remaining = snapshot.next_check.unwrap() - Utc::now();
    assert!(remaining >= chrono::Duration::hours(23) && remaining <= chrono::Duration::hours(25));
    service.handle.send(Command::SetEnabled(false));
    until(|| !service.handle.snapshot().enabled);
    service.shutdown();
    assert!(service.handle.snapshot().exited);
}

#[test]
fn source_data_fault_recovers_without_disabling_saved_automatic_setting() {
    let dir = tempfile::tempdir().unwrap();
    let mut prefs = Preferences::new(dir.path());
    prefs.automatic = true;
    settings::save(dir.path(), &prefs).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let runner: Runner = Arc::new(move |_, _, _, _| {
        if count.fetch_add(1, SeqCst) == 0 {
            Err(SourceDataError("临时损坏的源站数据").into())
        } else {
            success()
        }
    });
    let mut service = Service::spawn_with(
        dir.path().into(),
        prefs,
        None,
        Arc::new(|| {}),
        runner,
        timing(10000),
    );
    until(|| service.handle.snapshot().last_success.is_some());
    assert_eq!(calls.load(SeqCst), 2);
    assert!(service.handle.snapshot().enabled);
    assert!(settings::load(dir.path()).0.automatic);
    service.shutdown();
}

#[test]
fn scheduler_panic_is_visible_and_dead_channel_still_accepts_exit() {
    let dir = tempfile::tempdir().unwrap();
    let panic_once = Arc::new(AtomicBool::new(true));
    let trigger = panic_once.clone();
    let wake = Arc::new(move || {
        assert!(
            !trigger.swap(false, SeqCst),
            "injected scheduler callback failure"
        );
    });
    let mut service = Service::spawn_with(
        dir.path().into(),
        Preferences::new(dir.path()),
        None,
        wake,
        Arc::new(|_, _, _, _| success()),
        timing(10000),
    );
    until(|| service.join.as_ref().unwrap().is_finished());
    let stopped = service.handle.snapshot();
    assert!(stopped.exited && stopped.error && !stopped.exiting);
    assert!(!stopped.accepts_commands() && !stopped.can_configure_capture());
    assert!(stopped.status.contains("后台已停止"));
    service.handle.send(Command::SetEnabled(true));
    assert!(!service.handle.snapshot().enabled);
    assert_eq!(service.handle.snapshot().logs.len(), stopped.logs.len());
    service.shutdown();
    assert!(service.handle.snapshot().exiting);
}

#[test]
fn invalid_internal_deadline_reports_failure_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let mut prefs = Preferences::new(dir.path());
    prefs.automatic = true;
    let mut clock = timing(10000);
    clock.interval_override = Some(Duration::MAX);
    let mut service = Service::spawn_with(
        dir.path().into(),
        prefs,
        None,
        Arc::new(|| {}),
        Arc::new(|_, _, _, _| success()),
        clock,
    );
    until(|| service.handle.snapshot().exited);
    let stopped = service.handle.snapshot();
    assert!(stopped.error && !stopped.enabled && !stopped.busy);
    assert!(stopped.status.contains("下次检查时间超出系统范围"));
    service.shutdown();
}
