#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
use anyhow::{Context, Result, ensure};
use chrono::{Timelike, Utc};
use clap::Parser;
use earthlive_v2::{
    APP_NAME, SOURCE_SITE, desktop, display_version,
    domain::{Observation, parse_time, range},
    engine::Selection,
    journal,
    platform::{self, Tray},
    service::{AfterSave, Command, ConfigReply, Preview, Service, Snapshot},
    settings::{self, Preferences},
};
use eframe::egui;
use raw_window_handle::HasWindowHandle;
use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

mod gui;
mod widgets;
use widgets::{Tone, button};

#[derive(Parser)]
struct Options {
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[arg(long)]
    startup: bool,
    #[arg(long, hide = true)]
    open: Option<PathBuf>,
    #[arg(long, hide = true)]
    smoke_screenshot: Option<PathBuf>,
    #[arg(long, hide = true, default_value = "latest")]
    smoke_view: String,
    #[arg(long, hide = true)]
    smoke_compact: bool,
}
struct App {
    _instance: File,
    prefs: Preferences,
    photos: String,
    mode: u8,
    from: String,
    until: String,
    step: u32,
    refresh: bool,
    settings_open: bool,
    about_open: bool,
    dirty: bool,
    pending: Option<std::sync::mpsc::Receiver<ConfigReply>>,
    picker: Option<std::sync::mpsc::Receiver<Result<Option<PathBuf>>>>,
    main_window: usize,
    tray: Option<Tray>,
    service: Service,
    preview: Option<Arc<Preview>>,
    texture: Option<egui::TextureHandle>,
    screen: (u32, u32),
    startup_minimized: bool,
    first_frame: bool,
    closing: bool,
    smoke: Option<PathBuf>,
    smoke_sent: bool,
    started: Instant,
}
impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        data: PathBuf,
        instance: File,
        options: Options,
    ) -> Self {
        let ctx = &cc.egui_ctx;
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "MiSans".into(),
            egui::FontData::from_static(include_bytes!("../assets/fonts/MiSans-Regular.ttf"))
                .into(),
        );
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .insert(0, "MiSans".into());
        fonts
            .families
            .get_mut(&egui::FontFamily::Monospace)
            .unwrap()
            .push("MiSans".into());
        ctx.set_fonts(fonts);
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(20, 25, 31);
        visuals.window_fill = egui::Color32::from_rgb(24, 30, 37);
        visuals.selection.bg_fill = egui::Color32::from_rgb(42, 88, 114);
        ctx.set_visuals(visuals);
        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(9., 5.);
            s.spacing.button_padding = egui::vec2(9., 5.);
            s.override_font_id = Some(egui::FontId::proportional(15.));
        });
        widgets::style(ctx);
        let (mut prefs, mut notice) = settings::load(&data);
        match std::env::current_exe().and_then(|p| {
            platform::migrate_autostart(&p, &data)
                .and_then(|()| platform::autostart_enabled(&p, &data))
                .map_err(std::io::Error::other)
        }) {
            Ok(v) => prefs.autostart = v,
            Err(e) => notice = Some(format!("自启动状态读取失败：{e}")),
        }
        if options.smoke_screenshot.is_some() {
            prefs.automatic = false;
        }
        let now = Utc::now() - chrono::Duration::minutes(30);
        let now = now
            .with_minute(now.minute() / 10 * 10)
            .unwrap()
            .with_second(0)
            .unwrap();
        let time = (now + chrono::Duration::hours(if prefs.beijing { 8 } else { 0 }))
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let wake = ctx.clone();
        let service = Service::spawn(
            data.clone(),
            prefs.clone(),
            options.open.or(prefs.last.clone()),
            Arc::new(move || wake.request_repaint()),
        );
        let main = cc.window_handle().ok().and_then(|h| match h.as_raw() {
            raw_window_handle::RawWindowHandle::Win32(w) => Some(w.hwnd.get() as usize),
            _ => None,
        });
        let tray = main
            .context("主窗口句柄不可用")
            .and_then(|h| Tray::start(h, service.handle.clone(), ctx.clone()));
        let tray = match tray {
            Ok(v) => Some(v),
            Err(e) => {
                notice = Some(format!("托盘不可用，使用普通窗口：{e:#}"));
                None
            }
        };
        if let Some(n) = notice {
            service.handle.send(Command::Record(n, true));
        }
        let startup_minimized =
            options.startup && prefs.start_minimized && options.smoke_screenshot.is_none();
        Self {
            _instance: instance,
            photos: prefs.photos.to_string_lossy().into(),
            prefs,
            mode: if options.smoke_view == "history" {
                2
            } else {
                0
            },
            from: time.clone(),
            until: time,
            step: 10,
            refresh: false,
            settings_open: options.smoke_view == "settings",
            about_open: options.smoke_view == "about",
            dirty: false,
            pending: None,
            picker: None,
            main_window: main.unwrap_or(0),
            tray,
            service,
            preview: None,
            texture: None,
            screen: desktop::primary_size().unwrap_or((1920, 1080)),
            startup_minimized,
            first_frame: true,
            closing: false,
            smoke: options.smoke_screenshot,
            smoke_sent: false,
            started: Instant::now(),
        }
    }
    fn feedback(&self, message: impl Into<String>, error: bool) {
        self.service
            .handle
            .send(Command::Record(message.into(), error));
    }
    fn choose_directory(&mut self, ctx: &egui::Context) {
        let wake = ctx.clone();
        match platform::pick_directory(self.main_window, self.prefs.photos.clone(), move || {
            wake.request_repaint()
        }) {
            Ok(receiver) => self.picker = Some(receiver),
            Err(e) => self.feedback(format!("目录选择失败：{e:#}"), true),
        }
    }
    fn receive_directory(&mut self) {
        let Some(receiver) = &self.picker else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.picker = None;
                match result {
                    Ok(Some(path)) => {
                        self.photos = path.to_string_lossy().into_owned();
                        self.dirty = true;
                        self.submit_settings(None);
                    }
                    Ok(None) => {}
                    Err(e) => self.feedback(format!("目录选择失败：{e:#}"), true),
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.picker = None;
                self.feedback("目录选择窗口异常关闭", true);
            }
        }
    }
    fn submit_settings(&mut self, then: Option<AfterSave>) {
        if self.pending.is_some() {
            return;
        }
        let mut proposed = self.prefs.clone();
        proposed.photos = PathBuf::from(self.photos.trim());
        if !self.dirty && proposed.photos == self.prefs.photos {
            if let Some(action) = then {
                self.service.handle.send(match action {
                    AfterSave::Start => Command::SetEnabled(true),
                    AfterSave::Capture(selection, refresh) => Command::Capture(selection, refresh),
                });
            }
            return;
        }
        self.pending = Some(self.service.handle.configure(proposed, then));
        self.dirty = false;
    }
    fn receive_settings(&mut self) {
        let Some(receiver) = &self.pending else {
            return;
        };
        match receiver.try_recv() {
            Ok(reply) => {
                self.prefs = reply.preferences;
                self.photos = self.prefs.photos.to_string_lossy().into_owned();
                // The service records the actual error; restore the accepted values on rejection.
                if reply.error.is_some() {
                    self.dirty = false;
                }
                self.pending = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                self.feedback("后台未确认设置保存，请查看运行记录", true);
            }
        }
    }
    fn selection(&self) -> Result<Selection> {
        match self.mode {
            0 => Ok(Selection::Latest(self.prefs.grid)),
            1 => {
                let o =
                    Observation::new(parse_time(&self.from, self.prefs.beijing)?, self.prefs.grid)?;
                ensure!(o.time <= Utc::now(), "不能抓取未来时间");
                Ok(Selection::History(vec![o]))
            }
            _ => Ok(Selection::History(range(
                parse_time(&self.from, self.prefs.beijing)?,
                parse_time(&self.until, self.prefs.beijing)?,
                self.step,
                self.prefs.grid,
                Utc::now(),
            )?)),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.receive_settings();
        self.receive_directory();
        let s = self.service.handle.snapshot();
        self.prefs.automatic = s.enabled;
        if let Some(p) = &s.preview
            && !self.preview.as_ref().is_some_and(|old| Arc::ptr_eq(old, p))
        {
            let color = egui::ColorImage::from_rgb(
                [p.pixels.width() as usize, p.pixels.height() as usize],
                p.pixels.as_raw(),
            );
            self.texture = Some(ctx.load_texture("earth", color, egui::TextureOptions::LINEAR));
            self.preview = Some(p.clone());
            self.prefs.last = Some(p.path.clone());
            self.screen = desktop::primary_size().unwrap_or(self.screen);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !s.exited {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.prefs.close_to_tray
                && self.tray.is_some()
                && !s.exiting
                && !self.closing
                && self.smoke.is_none()
            {
                self.submit_settings(None);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.submit_settings(None);
                self.closing = true;
                self.service.handle.send(Command::Shutdown);
            }
        }
        if s.exited {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(APP_NAME);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.small(display_version());
                    if let Some(f) = &s.feedback
                        && f.at.elapsed() < Duration::from_secs(if f.error { 6 } else { 3 })
                    {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&f.message).color(if f.error {
                                widgets::ERROR
                            } else {
                                widgets::SUCCESS
                            }))
                            .truncate(),
                        )
                        .on_hover_text(&f.message);
                    }
                });
            });
        });
        let controls_height = widgets::CONTROLS_HEIGHT[self.mode as usize];
        let max_log =
            (ctx.available_rect().height() - controls_height).max(widgets::LOG_MIN_HEIGHT);
        egui::TopBottomPanel::bottom("status")
            .resizable(true)
            .default_height(240.)
            .min_height(widgets::LOG_MIN_HEIGHT)
            .max_height(max_log)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "本次运行｜已保存 {} 张｜源站缺失 {} 张",
                        s.saved, s.missing
                    ));
                    if let Some(e) = &s.log_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, "日志未落盘")
                            .on_hover_text(e);
                    }
                });
                ui.strong("运行记录");
                egui::ScrollArea::vertical()
                    .id_salt("run-log")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &s.logs {
                            ui.add(egui::Label::new(egui::RichText::new(line).size(13.)).wrap());
                        }
                    });
            });
        egui::SidePanel::left("controls")
            .exact_width(widgets::CONTROLS_WIDTH)
            .resizable(false)
            .show(ctx, |ui| self.controls(ui, &s));
        egui::CentralPanel::default().show(ctx, |ui| {
            if let (Some(p), Some(texture)) = (&self.preview, &self.texture) {
                let o = &p.observation;
                let metadata = format!(
                    "完整原图 ｜ {} UTC ｜ {} ｜ {}×{} ｜ {} {:.2} MiB",
                    o.time.format("%Y-%m-%d %H:%M"),
                    o.satellite,
                    o.side(),
                    o.side(),
                    p.format,
                    p.bytes as f64 / 1048576.
                );
                ui.add(egui::Label::new(&metadata).truncate())
                    .on_hover_text(&metadata);
                let space = ui.available_size();
                let side = space.x.min(space.y).max(1.);
                ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(side, side)));
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("选择“抓取本次”下载一批\n或选择“开始抓取”定时获取最新影像");
                });
            }
        });
        self.settings_window(ctx);
        self.about_window(ctx);
        if self.dirty && !ctx.input(|i| i.pointer.any_down()) && !ctx.wants_keyboard_input() {
            self.submit_settings(None);
        }
        if self.first_frame {
            self.first_frame = false;
            let tray_hidden =
                self.startup_minimized && self.prefs.minimize_to_tray && self.tray.is_some();
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!tray_hidden));
            if self.startup_minimized && !tray_hidden {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        }
        if self.smoke.is_some()
            && !s.busy
            && !self.smoke_sent
            && self.started.elapsed() > Duration::from_secs(3)
        {
            self.smoke_sent = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        if let Some(path) = &self.smoke {
            let shot = ctx.input(|i| {
                i.events.iter().find_map(|e| {
                    if let egui::Event::Screenshot { image, .. } = e {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });
            if let Some(image) = shot {
                let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Err(e) = image::save_buffer(
                    path,
                    &bytes,
                    image.size[0] as u32,
                    image.size[1] as u32,
                    image::ColorType::Rgba8,
                ) {
                    self.feedback(format!("截图失败：{e}"), true);
                }
                self.submit_settings(None);
                self.closing = true;
                self.service.handle.send(Command::Shutdown);
            }
        }
        // Hidden/minimized windows receive event-driven wakes, not animation ticks.
        if platform::window_visible(self.main_window) {
            ctx.request_repaint_after(Duration::from_millis(
                if s.busy || self.pending.is_some() || self.dirty {
                    125
                } else if s.enabled {
                    200
                } else {
                    1000
                },
            ));
        }
    }
}
fn main() {
    if let Err(error) = run() {
        unsafe {
            use windows::{
                Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
                core::HSTRING,
            };
            MessageBoxW(
                None,
                &HSTRING::from(format!("LiveEarth 无法启动：{error:#}")),
                &HSTRING::from(APP_NAME),
                MB_OK | MB_ICONERROR,
            );
        }
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let options = Options::parse();
    let data = options.data_dir.clone().unwrap_or(
        std::env::current_exe()?
            .parent()
            .context("程序目录不可用")?
            .join("数据"),
    );
    fs::create_dir_all(&data)?;
    let instance = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data.join(".instance.lock"))?;
    instance
        .try_lock()
        .context("同一数据目录的 LiveEarth 已运行")?;
    let icon =
        image::load_from_memory(include_bytes!(concat!(env!("OUT_DIR"), "/window-icon.png")))?
            .into_rgba8();
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(if options.smoke_compact {
                widgets::MIN_WINDOW
            } else {
                widgets::DEFAULT_WINDOW
            })
            .with_min_inner_size(widgets::MIN_WINDOW)
            .with_visible(!options.startup)
            .with_icon(egui::IconData {
                width: icon.width(),
                height: icon.height(),
                rgba: icon.into_raw(),
            }),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        native,
        Box::new(move |cc| Ok(Box::new(App::new(cc, data, instance, options)))),
    )
    .map_err(|e| anyhow::anyhow!("界面启动失败：{e}"))
}
