//! Visual design: Windows 11 (Fluent) palette, Segoe fonts, egui style.

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Window background (Mica).
    pub bg: Color32,
    /// Content layer behind cards.
    pub layer: Color32,
    pub card: Color32,
    pub stroke: Color32,
    pub text: Color32,
    pub sub: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub nav_sel: Color32,
    pub hover: Color32,
    /// Strong 1px rule under headers (Swiss touch).
    pub rule: Color32,
    pub hair: Color32,
    pub danger: Color32,
    pub ok: Color32,
}

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: Color32::from_rgb(0xF3, 0xF3, 0xF3),
    layer: Color32::from_rgb(0xF9, 0xF9, 0xF9),
    card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    stroke: Color32::from_rgb(0xE5, 0xE5, 0xE5),
    text: Color32::from_rgb(0x1A, 0x1A, 0x1A),
    sub: Color32::from_rgb(0x5C, 0x5C, 0x5C),
    accent: Color32::from_rgb(0x00, 0x5F, 0xB8),
    on_accent: Color32::WHITE,
    nav_sel: Color32::from_rgb(0xEA, 0xEA, 0xEA),
    hover: Color32::from_rgb(0xF2, 0xF2, 0xF2),
    rule: Color32::from_rgb(0x1A, 0x1A, 0x1A),
    hair: Color32::from_rgb(0xEB, 0xEB, 0xEB),
    danger: Color32::from_rgb(0xC4, 0x2B, 0x1C),
    ok: Color32::from_rgb(0x0F, 0x7B, 0x0F),
};

pub const DARK: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(0x20, 0x20, 0x20),
    layer: Color32::from_rgb(0x27, 0x27, 0x27),
    card: Color32::from_rgb(0x2D, 0x2D, 0x2D),
    stroke: Color32::from_rgb(0x3A, 0x3A, 0x3A),
    text: Color32::WHITE,
    sub: Color32::from_rgb(0xC5, 0xC5, 0xC5),
    accent: Color32::from_rgb(0x60, 0xCD, 0xFF),
    on_accent: Color32::BLACK,
    nav_sel: Color32::from_rgb(0x2D, 0x2D, 0x2D),
    hover: Color32::from_rgb(0x32, 0x32, 0x32),
    rule: Color32::from_rgb(0xE0, 0xE0, 0xE0),
    hair: Color32::from_rgb(0x38, 0x38, 0x38),
    danger: Color32::from_rgb(0xFF, 0x99, 0xA4),
    ok: Color32::from_rgb(0x6C, 0xCB, 0x5F),
};

pub fn palette(dark: bool) -> Palette {
    if dark {
        DARK
    } else {
        LIGHT
    }
}

/// (strong color, light-mode tint, dark-mode tint) per package manager.
pub fn provider_colors(id: &str) -> (Color32, Color32, Color32) {
    let c = |r, g, b| Color32::from_rgb(r, g, b);
    match id {
        "uv" => (c(0x87, 0x64, 0xB8), c(0xEC, 0xE5, 0xF5), c(0x4A, 0x3D, 0x5E)),
        "npm" => (c(0xD1, 0x34, 0x38), c(0xF9, 0xDE, 0xDF), c(0x5C, 0x2F, 0x31)),
        "nuget" => (c(0x00, 0x78, 0xD4), c(0xD9, 0xEA, 0xF8), c(0x22, 0x40, 0x5C)),
        "bun" => (c(0xB0, 0x8B, 0x00), c(0xF5, 0xEE, 0xD1), c(0x4D, 0x44, 0x27)),
        "go" => (c(0x03, 0x83, 0x87), c(0xD2, 0xEB, 0xEB), c(0x1E, 0x4A, 0x4B)),
        "pip" => (c(0x49, 0x82, 0x05), c(0xDF, 0xEB, 0xD1), c(0x34, 0x45, 0x2A)),
        "cargo" => (c(0xCA, 0x50, 0x10), c(0xF7, 0xE0, 0xD0), c(0x5A, 0x3A, 0x26)),
        "pnpm" => (c(0xC2, 0x8A, 0x00), c(0xF6, 0xEC, 0xC8), c(0x4F, 0x45, 0x22)),
        "yarn" => (c(0x2C, 0x8E, 0xBB), c(0xD6, 0xEB, 0xF4), c(0x21, 0x41, 0x50)),
        _ => (c(0x8A, 0x88, 0x86), c(0xEA, 0xE9, 0xE8), c(0x41, 0x40, 0x40)),
    }
}

/// Segoe Fluent Icons code points (Windows 11), with plain fallbacks.
pub struct Icons {
    pub has_fluent: bool,
}

impl Icons {
    pub fn get(&self, name: &str) -> &'static str {
        if self.has_fluent {
            match name {
                "overview" => "\u{E8A9}",
                "history" => "\u{E81C}",
                "settings" => "\u{E713}",
                "search" => "\u{E721}",
                "refresh" => "\u{E72C}",
                "chevron_down" => "\u{E70D}",
                "chevron_up" => "\u{E70E}",
                "delete" => "\u{E74D}",
                "folder" => "\u{E838}",
                "check" => "\u{E73E}",
                "warning" => "\u{E7BA}",
                "info" => "\u{E946}",
                _ => "•",
            }
        } else {
            match name {
                "overview" => "▦",
                "history" => "⏱",
                "settings" => "⚙",
                "search" => "🔍",
                "refresh" => "⟳",
                "chevron_down" => "⏷",
                "chevron_up" => "⏶",
                "delete" => "🗑",
                "folder" => "🗁",
                "check" => "✔",
                "warning" => "⚠",
                "info" => "ℹ",
                _ => "•",
            }
        }
    }
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

/// Load Segoe UI / Segoe UI Semibold / Segoe Fluent Icons from the system
/// (nothing bundled). Falls back to egui's built-in fonts.
pub fn install_fonts(ctx: &egui::Context) -> Icons {
    let mut fonts = FontDefinitions::default();
    let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
    let load = |f: &str| std::fs::read(dir.join(f)).ok();

    if let Some(b) = load("segoeui.ttf") {
        fonts.font_data.insert("segoe".into(), Arc::new(FontData::from_owned(b)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    let semi = load("seguisb.ttf").map(|b| {
        fonts.font_data.insert("segoe-semibold".into(), Arc::new(FontData::from_owned(b)));
        "segoe-semibold".to_string()
    });
    let mut semi_family: Vec<String> = semi.into_iter().collect();
    semi_family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name("semibold".into()), semi_family);

    let fluent = load("SegoeIcons.ttf").or_else(|| load("segmdl2.ttf"));
    let has_fluent = fluent.is_some();
    let mut icon_family = Vec::new();
    if let Some(b) = fluent {
        fonts.font_data.insert("fluent".into(), Arc::new(FontData::from_owned(b)));
        icon_family.push("fluent".to_string());
    }
    icon_family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name("icons".into()), icon_family);
    // Icons live in the private-use area, so they can also be a fallback for
    // normal text (lets buttons mix an icon and a label).
    if has_fluent {
        for fam in [FontFamily::Proportional, FontFamily::Name("semibold".into())] {
            fonts.families.entry(fam).or_default().push("fluent".into());
        }
    }

    ctx.set_fonts(fonts);
    Icons { has_fluent }
}

pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = if p.dark { Color32::from_rgb(0x2B, 0x2B, 0x2B) } else { Color32::from_rgb(0xFB, 0xFB, 0xFB) };
    v.extreme_bg_color = p.card;
    v.faint_bg_color = p.layer;
    v.window_stroke = Stroke::new(1.0, p.stroke);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.striped = false;

    let r = CornerRadius::same(4);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.sub);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = p.card;
    w.inactive.weak_bg_fill = p.card;
    w.inactive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = p.hover;
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.stroke);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.nav_sel;
    w.active.weak_bg_fill = p.nav_sel;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.active;

    let mut style = (*ctx.global_style()).clone();
    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 5.0);
    style.spacing.interact_size.y = 32.0;
    style.spacing.window_margin = egui::Margin::same(22);
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.floating = true;
    style.text_styles = [
        (TextStyle::Heading, FontId::new(28.0, FontFamily::Name("semibold".into()))),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();
    ctx.set_global_style(style);
}
