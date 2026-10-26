use earthlive_v2::service::Activity;
use eframe::egui::{self, Color32, Response, Stroke, Ui, Vec2};

pub const ERROR: Color32 = Color32::from_rgb(230, 160, 130);
pub const SUCCESS: Color32 = Color32::from_rgb(127, 188, 181);
pub const WAITING: Color32 = Color32::from_rgb(90, 149, 123);
pub const WORKING: Color32 = Color32::from_rgb(90, 160, 180);
pub const RETRYING: Color32 = Color32::from_rgb(173, 149, 85);
pub const STOPPED: Color32 = Color32::from_rgb(65, 78, 84);
const SELECTED: Color32 = Color32::from_rgb(40, 73, 94);
pub const SECONDARY: Color32 = Color32::from_rgb(145, 153, 161);
pub const CONTROLS_WIDTH: f32 = 340.;
pub const LOG_MIN_HEIGHT: f32 = 145.;
pub const MIN_WINDOW: [f32; 2] = [1000., 800.];
pub const DEFAULT_WINDOW: [f32; 2] = [1180., 960.];

/// Shared style for explanatory copy and license text.
pub fn secondary(ui: &mut Ui, content: impl FnOnce(&mut Ui)) {
    ui.scope(|ui| {
        ui.style_mut().override_font_id = Some(egui::FontId::proportional(12.));
        ui.visuals_mut().override_text_color = Some(SECONDARY);
        ui.visuals_mut().hyperlink_color = SECONDARY;
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

fn palette(widgets: &mut egui::style::Widgets, tone: Tone) {
    let (fill, edge) = match tone {
        Tone::Neutral => (
            Color32::from_rgb(38, 46, 55),
            Color32::from_rgb(94, 109, 121),
        ),
        Tone::Selected => (SELECTED, Color32::from_rgb(100, 150, 180)),
        Tone::Running => (
            Color32::from_rgb(35, 76, 65),
            Color32::from_rgb(90, 149, 123),
        ),
        Tone::Stopped => (
            Color32::from_rgb(83, 45, 46),
            Color32::from_rgb(158, 101, 100),
        ),
    };
    for (w, factor) in [
        (&mut widgets.inactive, 1.0),
        (&mut widgets.hovered, 1.25),
        (&mut widgets.active, 0.78),
    ] {
        w.corner_radius = 4.into();
        w.bg_fill = fill.gamma_multiply(factor);
        w.weak_bg_fill = w.bg_fill;
        w.bg_stroke = Stroke::new(1.5_f32, edge.gamma_multiply(factor));
        w.fg_stroke = Stroke::new(1_f32, Color32::from_rgb(221, 226, 230));
        w.expansion = 0.;
    }
}

pub fn style(ctx: &egui::Context) {
    ctx.style_mut(|s| {
        palette(&mut s.visuals.widgets, Tone::Neutral);
        s.visuals.selection.bg_fill = SELECTED;
    });
}

pub fn button(ui: &mut Ui, label: &str, width: f32, tone: Tone) -> Response {
    ui.scope(|ui| {
        palette(&mut ui.visuals_mut().widgets, tone);
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
    let time = ui.input(|i| i.time);
    let phase = (time * 8.) as usize % 8;
    let breath = 0.55 + 0.45 * ((time * std::f64::consts::TAU / 3.).sin() as f32 + 1.) / 2.;
    for (index, (x, y)) in positions.into_iter().enumerate() {
        let age = (phase + 8 - index) % 8;
        let color = match activity {
            Activity::Working => WORKING.gamma_multiply(match age {
                0 => 1.,
                1 => 0.7,
                2 => 0.4,
                _ => 0.2,
            }),
            Activity::Waiting => WAITING.gamma_multiply(breath),
            Activity::Retrying => RETRYING.gamma_multiply(breath),
            Activity::Stopped => STOPPED,
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
