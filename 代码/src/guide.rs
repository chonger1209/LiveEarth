use super::*;

impl App {
    pub(super) fn guide_window(&mut self, ctx: &egui::Context) {
        let mut open = self.guide_open;
        let mut confirm = false;
        egui::Window::new("快速上手")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(600.)
            .min_width(420.)
            .show(ctx, |ui| {
                ui.heading("四步设置你的地球壁纸");
                for (title, text) in [
                    ("① 选择影像分辨率", "若正在抓取，先停止并等待任务结束。选择“最新”和分辨率，不确定时先用默认值；分辨率越高，细节越多，下载和存储占用也越大。"),
                    ("② 选择保存目录", "点击“修改目录”，选择照片位置，运行日志也保存在这里。新配置默认最多保留 99 张，超限直接清理最旧归档，不进入回收站；设置为 0 可不限量。长期收藏请另存目录。"),
                    ("③ 开始抓取与更新壁纸", "保持“自动抓取成功后应用壁纸”勾选，点击“开始抓取”，立即获取最新影像，之后定时更新。“抓取本次”只下载所选影像，不自动更换壁纸。"),
                    ("④ 调整壁纸上的地球大小", "停止抓取 → 等当前任务结束 → 调整“壁纸缩放” → 再次开始抓取。已有有效原图会被复用；100% 是原图像素尺寸，缩放不改变保存的原图。"),
                ] {
                    ui.add_space(8.);
                    ui.label(egui::RichText::new(title).strong());
                    widgets::secondary(ui, |ui| {
                        ui.add(egui::Label::new(text).wrap());
                    });
                }
                ui.add_space(12.);
                ui.separator();
                ui.add_enabled_ui(self.service.handle.snapshot().accepts_commands() && self.pending.is_none(), |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.guide_no_repeat, "不再自动显示");
                        confirm = button(ui, "知道了，开始使用", 170., Tone::Neutral).clicked();
                    });
                });
                widgets::secondary(ui, |ui| {
                    ui.label("以后可从“项目说明 → 快速上手”重新打开。确认只关闭引导，不会启动抓取。");
                });
            });
        if confirm {
            self.prefs.show_guide_on_start = !self.guide_no_repeat;
            self.dirty = true;
            self.submit_settings(None);
            open = false;
        }
        self.guide_open = open;
    }
}
