use eframe::egui::{
    Color32, Context, CornerRadius, FontData, FontFamily, FontId, Margin, Shadow, Stroke,
    TextStyle, Vec2, Visuals,
    epaint::text::{FontInsert, FontPriority, InsertFontFamily},
};

const fn premultiply(channel: u8, alpha: u8) -> u8 {
    ((channel as u16 * alpha as u16 + 127) / 255) as u8
}

const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Color32 {
    Color32::from_rgba_premultiplied(
        premultiply(red, alpha),
        premultiply(green, alpha),
        premultiply(blue, alpha),
        alpha,
    )
}

pub const TEXT: Color32 = Color32::from_rgb(224, 231, 255);
pub const MUTED: Color32 = Color32::from_rgb(129, 140, 168);
pub const INDIGO: Color32 = Color32::from_rgb(129, 140, 248);
pub const CYAN: Color32 = Color32::from_rgb(56, 189, 248);
pub const VIOLET: Color32 = Color32::from_rgb(167, 139, 250);
pub const ERROR: Color32 = Color32::from_rgb(251, 113, 133);
pub const WARNING: Color32 = Color32::from_rgb(251, 191, 36);
pub const EDITOR_TEXT: Color32 = Color32::from_rgb(221, 230, 255);
pub const EDITOR_MUTED: Color32 = Color32::from_rgb(102, 119, 156);
pub const CURSOR: Color32 = Color32::from_rgb(125, 211, 252);
pub const SELECTION: Color32 = rgba(49, 84, 138, 190);
pub const ACTIVE_LINE: Color32 = rgba(16, 29, 56, 210);

const BACKGROUND: Color32 = rgba(5, 9, 22, 205);
const PANEL: Color32 = rgba(10, 17, 36, 220);
const WINDOW: Color32 = rgba(15, 25, 50, 238);
const SURFACE: Color32 = rgba(20, 32, 61, 225);
const SURFACE_HOVER: Color32 = rgba(35, 49, 88, 235);
const BORDER: Color32 = rgba(99, 116, 163, 80);

pub fn install_material_symbols(context: &Context) {
    let mut data = FontData::from_static(egui_material_icons::FONT_DATA);
    data.tweak.y_offset_factor = 0.05;
    context.add_font(FontInsert::new(
        "material-icons",
        data,
        vec![
            InsertFontFamily {
                family: FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: FontFamily::Monospace,
                priority: FontPriority::Lowest,
            },
        ],
    ));
}

pub fn apply(context: &Context) {
    let mut style = (*context.style()).clone();
    style.visuals = Visuals::dark();
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.panel_fill = PANEL;
    style.visuals.window_fill = WINDOW;
    style.visuals.extreme_bg_color = BACKGROUND;
    style.visuals.faint_bg_color = Color32::from_rgba_unmultiplied(15, 25, 50, 190);
    style.visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.window_corner_radius = CornerRadius::same(12);
    style.visuals.window_shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    style.visuals.popup_shadow = Shadow {
        offset: [0, 5],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(105),
    };
    style.visuals.selection.bg_fill = SELECTION;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, CYAN);
    style.visuals.hyperlink_color = CYAN;
    style.visuals.text_cursor.stroke = Stroke::new(1.6_f32, CURSOR);

    let widgets = &mut style.visuals.widgets;
    for visuals in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        visuals.corner_radius = CornerRadius::same(7);
        visuals.fg_stroke = Stroke::new(1.0_f32, TEXT);
    }
    widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
    widgets.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    widgets.inactive.bg_fill = SURFACE;
    widgets.inactive.weak_bg_fill = SURFACE;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    widgets.hovered.bg_fill = SURFACE_HOVER;
    widgets.hovered.weak_bg_fill = SURFACE_HOVER;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, INDIGO);
    widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    widgets.active.bg_fill = Color32::from_rgb(67, 56, 202);
    widgets.active.weak_bg_fill = Color32::from_rgb(67, 56, 202);
    widgets.active.bg_stroke = Stroke::new(1.0_f32, CYAN);
    widgets.open.bg_fill = SURFACE_HOVER;
    widgets.open.weak_bg_fill = SURFACE_HOVER;
    widgets.open.bg_stroke = Stroke::new(1.0_f32, INDIGO);

    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style.spacing.window_margin = Margin::same(16);
    style.spacing.menu_margin = Margin::same(8);
    style.spacing.scroll.bar_width = 8.0;
    style.text_styles = [
        (TextStyle::Small, FontId::monospace(11.0)),
        (TextStyle::Body, FontId::monospace(13.0)),
        (TextStyle::Monospace, FontId::monospace(14.0)),
        (TextStyle::Button, FontId::monospace(13.0)),
        (TextStyle::Heading, FontId::monospace(20.0)),
    ]
    .into();
    context.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_symbols_are_available_in_monospace_ui() {
        let context = Context::default();
        install_material_symbols(&context);
        let _ = context.run(eframe::egui::RawInput::default(), |context| {
            context.fonts(|fonts| {
                for icon in [
                    egui_material_icons::icons::ICON_FOLDER,
                    egui_material_icons::icons::ICON_CODE,
                ] {
                    assert!(
                        fonts.has_glyph(&FontId::monospace(13.0), icon.chars().next().unwrap(),)
                    );
                }
            });
        });
    }
}
