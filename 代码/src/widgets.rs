use earthlive_v2::service::Activity;
use eframe::egui::{self, Color32, Response, Stroke, Ui, Vec2};

#[derive(Clone, Copy)]
pub struct ThemeColors {
    pub error: Color32,
    pub success: Color32,
    pub waiting: Color32,
    pub working: Color32,
    pub retrying: Color32,
    pub stopped: Color32,
    pub secondary: Color32,
    pub border: Color32,
    dark: bool,
    foreground: Color32,
    panel: Color32,
    window: Color32,
    neutral: (Color32, Color32),
    selected: (Color32, Color32),
    running: (Color32, Color32),
    start: (Color32, Color32),
}
impl ThemeColors {
    fn for_theme(dark: bool) -> Self {
        let rgb = Color32::from_rgb;
        if dark {
            Self {
                error: rgb(230, 160, 130),
                success: rgb(127, 188, 181),
                waiting: rgb(90, 149, 123),
                working: rgb(90, 160, 180),
                retrying: rgb(173, 149, 85),
                stopped: rgb(65, 78, 84),
                secondary: rgb(145, 153, 161),
                border: rgb(65, 78, 84),
                dark,
                foreground: rgb(221, 226, 230),
                panel: rgb(20, 25, 31),
                window: rgb(24, 30, 37),
                neutral: (rgb(38, 46, 55), rgb(94, 109, 121)),
                selected: (rgb(40, 73, 94), rgb(100, 150, 180)),
                running: (rgb(35, 76, 65), rgb(90, 149, 123)),
                start: (rgb(83, 45, 46), rgb(158, 101, 100)),
            }
        } else {
            Self {
                error: rgb(166, 60, 45),
                success: rgb(35, 111, 99),
                waiting: rgb(42, 119, 87),
                working: rgb(25, 107, 139),
                retrying: rgb(130, 96, 28),
                stopped: rgb(99, 109, 119),
                secondary: rgb(90, 97, 105),
                border: rgb(133, 144, 154),
                dark,
                foreground: rgb(40, 46, 54),
                panel: rgb(244, 246, 248),
                window: rgb(250, 251, 252),
                neutral: (rgb(234, 238, 242), rgb(133, 144, 154)),
                selected: (rgb(215, 232, 242), rgb(65, 113, 145)),
                running: (rgb(214, 235, 222), rgb(65, 124, 91)),
                start: (rgb(244, 219, 215), rgb(162, 82, 69)),
            }
        }
    }
}
pub fn colors(ui: &Ui) -> ThemeColors {
    ThemeColors::for_theme(ui.visuals().dark_mode)
}
pub const CONTROLS_WIDTH: f32 = 340.;
pub const LOG_MIN_HEIGHT: f32 = 145.;
pub const MIN_WINDOW: [f32; 2] = [1000., 800.];
pub const DEFAULT_WINDOW: [f32; 2] = [1180., 960.];

/// Shared style for explanatory copy and license text.
pub fn secondary(ui: &mut Ui, content: impl FnOnce(&mut Ui)) {
    ui.scope(|ui| {
        ui.style_mut().override_font_id = Some(egui::FontId::proportional(12.));
        let secondary = colors(ui).secondary;
        ui.visuals_mut().override_text_color = Some(secondary);
        ui.visuals_mut().hyperlink_color = secondary;
        content(ui);
    });
}

pub fn third_width(ui: &Ui) -> f32 {
    (ui.available_width() - 2. * ui.spacing().item_spacing.x) / 3.
}

#[derive(Clone, Copy)]
pub enum Tone {
    Neutral,
    Selected,
    Running,
    Stopped,
}

fn palette(widgets: &mut egui::style::Widgets, tone: Tone, colors: ThemeColors) {
    let (fill, edge) = match tone {
        Tone::Neutral => colors.neutral,
        Tone::Selected => colors.selected,
        Tone::Running => colors.running,
        Tone::Stopped => colors.start,
    };
    for (w, factor) in [
        (&mut widgets.inactive, 1.0),
        (&mut widgets.hovered, if colors.dark { 1.25 } else { 0.96 }),
        (&mut widgets.active, if colors.dark { 0.78 } else { 0.90 }),
    ] {
        w.corner_radius = 4.into();
        w.bg_fill = fill.gamma_multiply(factor);
        w.weak_bg_fill = w.bg_fill;
        w.bg_stroke = Stroke::new(1.5_f32, edge.gamma_multiply(factor));
        w.fg_stroke = Stroke::new(1_f32, colors.foreground);
        w.expansion = 0.;
    }
}

pub fn style(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let colors = ThemeColors::for_theme(theme == egui::Theme::Dark);
        ctx.style_mut_of(theme, |s| {
            s.visuals = if colors.dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            };
            s.visuals.panel_fill = colors.panel;
            s.visuals.window_fill = colors.window;
            s.visuals.widgets.noninteractive.fg_stroke.color = colors.foreground;
            s.visuals.widgets.noninteractive.bg_stroke.color = colors.border;
            s.visuals.hyperlink_color = colors.working;
            s.visuals.selection.bg_fill = colors.selected.0;
            s.visuals.selection.stroke = Stroke::new(1_f32, colors.foreground);
            s.visuals.error_fg_color = colors.error;
            s.visuals.warn_fg_color = colors.retrying;
            s.spacing.item_spacing = egui::vec2(9., 5.);
            s.spacing.button_padding = egui::vec2(9., 5.);
            s.override_font_id = Some(egui::FontId::proportional(15.));
            palette(&mut s.visuals.widgets, Tone::Neutral, colors);
        });
    }
    ctx.set_theme(egui::ThemePreference::System);
}

pub fn button(ui: &mut Ui, label: &str, width: f32, tone: Tone) -> Response {
    ui.scope(|ui| {
        let colors = colors(ui);
        palette(&mut ui.visuals_mut().widgets, tone, colors);
        ui.add_sized(Vec2::new(width, 30.), egui::Button::new(label).frame(true))
    })
    .inner
}

pub fn activity(ui: &mut Ui, activity: Activity) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.), egui::Sense::hover());
    let positions = [
        (1., 0.),
        (2., 0.),
        (2., 1.),
        (2., 2.),
        (1., 2.),
        (0., 2.),
        (0., 1.),
        (0., 0.),
    ];
    let colors = colors(ui);
    let time = ui.input(|i| i.time);
    let phase = (time * 8.) as usize % 8;
    let breath = 0.55 + 0.45 * ((time * std::f64::consts::TAU / 3.).sin() as f32 + 1.) / 2.;
    for (index, (x, y)) in positions.into_iter().enumerate() {
        let age = (phase + 8 - index) % 8;
        let color = match activity {
            Activity::Working => colors.working.gamma_multiply(match age {
                0 => 1.,
                1 => 0.7,
                2 => 0.4,
                _ => 0.2,
            }),
            Activity::Waiting => colors.waiting.gamma_multiply(breath),
            Activity::Retrying => colors.retrying.gamma_multiply(breath),
            Activity::Stopped => colors.stopped,
        };
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                rect.min + egui::vec2(2. + x * 6., 2. + y * 6.),
                Vec2::splat(4.),
            ),
            0.,
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_themes_keep_layout_and_readable_colors() {
        fn luminance(color: Color32) -> f64 {
            let channels = [color.r(), color.g(), color.b()].map(|x| {
                let value = f64::from(x) / 255.;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            });
            channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722
        }
        let ctx = egui::Context::default();
        style(&ctx);
        assert_eq!(
            ctx.memory(|m| m.options.theme_preference),
            egui::ThemePreference::System
        );
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            ctx.set_theme(theme);
            let style = ctx.style();
            let colors = ThemeColors::for_theme(theme == egui::Theme::Dark);
            assert_eq!(style.visuals.dark_mode, colors.dark);
            assert_eq!(style.visuals.panel_fill, colors.panel);
            assert_eq!(style.spacing.button_padding, egui::vec2(9., 5.));
            assert_eq!(style.override_font_id.as_ref().unwrap().size, 15.);
            for text in [
                style.visuals.widgets.noninteractive.fg_stroke.color,
                colors.secondary,
                colors.error,
                colors.success,
                colors.retrying,
            ] {
                for background in [colors.panel, colors.window] {
                    let (a, b) = (luminance(text), luminance(background));
                    assert!(
                        (a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5,
                        "insufficient text contrast in {theme:?}"
                    );
                }
            }
        }
    }

    fn pixels(state: Activity, time: f64) -> Vec<egui::epaint::RectShape> {
        let ctx = egui::Context::default();
        let output = ctx.run(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| activity(ui, state));
            },
        );
        output
            .shapes
            .into_iter()
            .filter_map(|s| match s.shape {
                egui::Shape::Rect(r) if r.rect.size() == Vec2::splat(4.) => Some(r),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn activity_states_render_and_only_active_states_animate() {
        let states = [
            Activity::Stopped,
            Activity::Waiting,
            Activity::Working,
            Activity::Retrying,
        ];
        let mut preview = image::RgbaImage::from_pixel(144, 288, image::Rgba([20, 25, 31, 255]));
        for (row, state) in states.into_iter().enumerate() {
            let frames = [pixels(state, 0.), pixels(state, 0.25)];
            assert!(frames.iter().all(|f| f.len() == 8));
            let colors = |frame: &Vec<egui::epaint::RectShape>| {
                frame.iter().map(|p| p.fill).collect::<Vec<_>>()
            };
            assert_eq!(
                colors(&frames[0]) == colors(&frames[1]),
                state == Activity::Stopped
            );
            for (column, frame) in frames.iter().enumerate() {
                let origin = frame
                    .iter()
                    .fold(egui::pos2(f32::MAX, f32::MAX), |p, r| p.min(r.rect.min));
                for r in frame {
                    let x = ((r.rect.min.x - origin.x) * 3.) as u32 + column as u32 * 72 + 12;
                    let y = ((r.rect.min.y - origin.y) * 3.) as u32 + row as u32 * 72 + 12;
                    for dy in 0..12 {
                        for dx in 0..12 {
                            preview.put_pixel(x + dx, y + dy, image::Rgba(r.fill.to_array()));
                        }
                    }
                }
            }
        }
        // Optional review artifact; no extra production flags or runtime work.
        if let Some(path) = std::env::var_os("LIVEEARTH_ACTIVITY_PREVIEW") {
            preview.save(path).unwrap();
        }
    }
}
