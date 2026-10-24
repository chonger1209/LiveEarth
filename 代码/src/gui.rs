use super::*;

impl App {
    pub(super) fn controls(&mut self, ui: &mut egui::Ui, s: &Snapshot) {
        let editable = s.can_configure_capture() && self.pending.is_none() && self.picker.is_none();
        ui.heading("影像与历史");
        ui.add_enabled_ui(editable, |ui| {
            ui.horizontal(|ui| {
                let width = widgets::third_width(ui);
                for (mode, label) in [(0, "最新"), (1, "指定时刻"), (2, "历史区间")] {
                    if button(
                        ui,
                        label,
                        width,
                        if self.mode == mode {
                            Tone::Selected
                        } else {
                            Tone::Neutral
                        },
                    )
                    .clicked()
                    {
                        self.mode = mode;
                    }
                }
            });
            if self.mode > 0 {
                self.dirty |= ui
                    .checkbox(&mut self.prefs.beijing, "输入使用北京时间")
                    .on_hover_text("勾选为 UTC+8，未勾选为 UTC。")
                    .changed();
                ui.horizontal(|ui| {
                    ui.label(if self.mode == 1 {
                        "观测时刻"
                    } else {
                        "开始时刻"
                    });
                    ui.add(egui::TextEdit::singleline(&mut self.from).desired_width(210.))
                        .on_hover_text("例如 2026-10-23 15:30，须落在 10 分钟边界。");
                });
                if self.mode == 2 {
                    ui.horizontal(|ui| {
                        ui.label("结束时刻");
                        ui.add(egui::TextEdit::singleline(&mut self.until).desired_width(210.))
                            .on_hover_text("包含结束时刻，须落在 10 分钟边界。");
                    });
                    ui.horizontal(|ui| {
                        ui.label("抓取步长");
                        egui::ComboBox::from_id_salt("step")
                            .selected_text(format!("{} 分钟", self.step))
                            .show_ui(ui, |ui| {
                                for n in [10, 20, 30, 60] {
                                    ui.selectable_value(&mut self.step, n, format!("{n} 分钟"));
                                }
                            });
                    });
                }
            }
            ui.horizontal(|ui| {
                ui.label("抓取分辨率");
                egui::ComboBox::from_id_salt("grid")
                    .selected_text(format!(
                        "{}×{}",
                        self.prefs.grid * 550,
                        self.prefs.grid * 550
                    ))
                    .show_ui(ui, |ui| {
                        for n in [1, 2, 4, 8, 16] {
                            self.dirty |= ui
                                .selectable_value(
                                    &mut self.prefs.grid,
                                    n,
                                    format!("{}×{}", n * 550, n * 550),
                                )
                                .changed();
                        }
                    });
            });
            ui.checkbox(&mut self.refresh, "重新下载并核对已有帧")
                .on_hover_text("仅用于“抓取本次”；相同内容不重复保存，源站修订另存文件。");
        });
        ui.label("照片目录");
        egui::Frame::new()
            .inner_margin(4.)
            .stroke(egui::Stroke::new(1_f32, widgets::STOPPED))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(egui::Label::new(&self.photos).truncate())
                    .on_hover_text(&self.photos);
            });
        ui.horizontal(|ui| {
            let width = widgets::third_width(ui);
            if button(ui, "打开目录", width, Tone::Neutral).clicked()
                && let Err(e) = platform::open_directory(&self.prefs.photos)
            {
                self.feedback(format!("{e:#}"), true);
            }
            if ui
                .add_enabled_ui(editable, |ui| button(ui, "修改目录", width, Tone::Neutral))
                .inner
                .clicked()
            {
                self.choose_directory(ui.ctx());
            }
            if ui
                .add_enabled_ui(editable, |ui| button(ui, "抓取本次", width, Tone::Neutral))
                .inner
                .clicked()
            {
                match self.selection() {
                    Ok(selection) => {
                        self.submit_settings(Some(AfterSave::Capture(selection, self.refresh)))
                    }
                    Err(e) => self.feedback(format!("输入错误：{e:#}"), true),
                }
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("定时抓取");
            widgets::activity(ui, s.activity());
            let state = if s.exiting {
                "正在退出"
            } else if s.busy {
                &s.status
            } else if s.retrying && s.enabled {
                "等待重试"
            } else if s.enabled {
                "定时抓取中"
            } else {
                "已停止"
            };
            ui.add(
                egui::Label::new(egui::RichText::new(state).size(13.).color(if s.error {
                    widgets::ERROR
                } else {
                    widgets::SUCCESS
                }))
                .truncate(),
            );
        });
        ui.horizontal(|ui| {
            let width = if s.busy {
                (ui.available_width() - ui.spacing().item_spacing.x) / 2.
            } else {
                ui.available_width()
            };
            if ui
                .add_enabled_ui(
                    !s.exiting && self.pending.is_none() && self.picker.is_none(),
                    |ui| {
                        button(
                            ui,
                            if s.enabled {
                                "停止抓取"
                            } else {
                                "开始抓取"
                            },
                            width,
                            if s.enabled {
                                Tone::Running
                            } else {
                                Tone::Stopped
                            },
                        )
                    },
                )
                .inner
                .clicked()
            {
                if s.enabled {
                    self.service.handle.send(Command::SetEnabled(false));
                } else {
                    self.submit_settings(Some(AfterSave::Start));
                }
            }
            if s.busy
                && ui
                    .add_enabled_ui(!s.exiting, |ui| {
                        button(ui, "取消当前任务", width, Tone::Neutral)
                    })
                    .inner
                    .clicked()
            {
                self.service.handle.send(Command::Cancel);
            }
        });
        ui.add_enabled_ui(editable, |ui| {
            ui.horizontal(|ui| {
                ui.label("检查间隔");
                self.dirty |= ui.add(egui::DragValue::new(&mut self.prefs.interval_minutes).range(1..=120)).changed();
                ui.label("分钟");
            });
            self.dirty |= ui.checkbox(&mut self.prefs.apply_automatic, "自动抓取成功后应用壁纸").changed();
            let side = s.preview.as_ref().map_or(self.prefs.grid * 550, |p| p.observation.side());
            let tip = format!("100% = 原图像素尺寸；主屏 {}×{}。\n完整地球适配主屏约 {:.1}%。\n下一次自动抓取成功后应用，完整原图不缩放。",
                self.screen.0, self.screen.1, 100. * self.screen.0.min(self.screen.1) as f32 / side as f32);
            ui.horizontal(|ui| {
                ui.label("壁纸缩放").on_hover_text(&tip);
                self.dirty |= ui.add(egui::Slider::new(&mut self.prefs.zoom, 10..=200).suffix("%"))
                    .on_hover_text(&tip).changed();
            });
        });
        let clock = |time: Option<chrono::DateTime<Utc>>| {
            time.map_or("—".into(), |t| {
                journal::beijing(t).format("%H:%M:%S").to_string()
            })
        };
        ui.small(format!(
            "下次 {}  ｜ 最近检查 {}",
            clock(s.next_check),
            clock(s.last_check)
        ));
        ui.add_space(2.);
        ui.separator();
        ui.horizontal(|ui| {
            let width = widgets::third_width(ui);
            if button(ui, "设置", width, Tone::Neutral).clicked() {
                self.settings_open = true;
            }
            if button(ui, "项目说明", width, Tone::Neutral).clicked() {
                self.about_open = true;
            }
            if button(ui, "查看更新", width, Tone::Neutral).clicked() {
                ui.ctx()
                    .open_url(egui::OpenUrl::new_tab(earthlive_v2::RELEASES_SITE));
            }
        });
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        let capture_editable =
            self.service.handle.snapshot().can_configure_capture() && self.picker.is_none();
        egui::Window::new("设置").open(&mut self.settings_open).collapsible(false).resizable(false)
            .default_width(360.).show(ctx, |ui| {
                ui.add_enabled_ui(self.pending.is_none(), |ui| {
                    ui.heading("照片保留");
                    ui.add_enabled_ui(capture_editable, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("最多保留");
                            self.dirty |= ui.add(egui::DragValue::new(&mut self.prefs.photo_limit).range(0..=1_000_000).suffix(" 张")).changed();
                        });
                        widgets::secondary(ui, |ui| {
                            ui.label("0 表示不限量；最新影像新增后清理最旧归档。");
                            ui.label("历史抓取不触发清理，其照片仍参与后续清理。");
                        });
                    });
                    ui.separator();
                    ui.heading("窗口与托盘");
                    ui.add_enabled_ui(self.tray.is_some(), |ui| {
                        self.dirty |= ui.checkbox(&mut self.prefs.minimize_to_tray, "最小化时收起到托盘").changed();
                        self.dirty |= ui.checkbox(&mut self.prefs.close_to_tray, "关闭窗口时收起到托盘").changed();
                    });
                    ui.add_space(8.);
                    ui.separator();
                    ui.heading("启动");
                    self.dirty |= ui.checkbox(&mut self.prefs.autostart, "开机自启动").changed();
                    self.dirty |= ui.add_enabled(self.prefs.autostart,
                        egui::Checkbox::new(&mut self.prefs.start_minimized, "开机自启动&最小化"))
                        .on_hover_text("自启动时最小化；是否收起到托盘由上方“最小化时收起到托盘”决定。手动打开程序正常显示窗口。").changed();
                });
            });
    }

    pub(super) fn about_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("项目说明").open(&mut self.about_open).collapsible(false).resizable(true)
            .default_width(600.).min_width(300.).show(ctx, |ui| {
                ui.heading(format!("{APP_NAME} {}", display_version()));
                ui.add_space(8.);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.;
                    ui.label("从 ");
                    ui.hyperlink_to("NICT 向日葵卫星影像网站", SOURCE_SITE);
                    ui.label("获取地球图像，自动更新桌面壁纸，并保存完整原图。支持抓取最新影像、指定时刻，以及历史时间区间的影像。");
                });
                ui.add_space(8.);
                widgets::secondary(ui, |ui| {
                    ui.label("卫星观测数据：日本气象厅（JMA）。");
                    ui.hyperlink_to("提供：情報通信研究機構（NICT）", "https://himawari8.nict.go.jp/ja/himawari8-about.htm");
                    ui.hyperlink_to("参考项目：EarthLiveSharp · bitdust 及贡献者", "https://github.com/bitdust/EarthLiveSharp");
                    ui.label("本项目独立维护，非上述机构或原项目的官方版本。");
                    ui.label("本软件使用 MiSans 字体（小米科技有限责任公司）。");
                    ui.hyperlink_to("MiSans 字体许可协议", "https://hyperos.mi.com/font-download/MiSans字体知识产权许可协议.pdf");
                });
            });
    }
}
