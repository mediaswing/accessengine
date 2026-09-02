//! Fonts, colour, and the choice between light and dark.
//!
//! Text size matters more here than in most apps: the document pane is the
//! thing people are reading along with while it is spoken, often the reason
//! they opened a text-to-speech reader in the first place.
//!
//! The palette is the one from the `watchspend` app, and for the same reason:
//! egui's two default themes are not written down anywhere as a pair, so the
//! contrast of a colour against the surface it lands on cannot be reasoned
//! about. Both themes are defined here instead, with every text colour at 4.5:1
//! or better against its background.

use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals};

const UBUNTU_BOLD: &[u8] = include_bytes!("../assets/fonts/Ubuntu-Bold.ttf");

/// Ubuntu Bold in front of everything egui ships.
///
/// `RichText::strong()` only recolours; a heavier weight has to arrive as a
/// real font. Putting it first in the `Proportional` chain means every widget
/// picks it up without each call site asking. Everything egui already had
/// stays behind it, so a glyph Ubuntu Bold does not cover still renders
/// instead of becoming a tofu box — which is what keeps the transport symbols
/// (`▶`, `⏸`, `⏹`, `⏮`, `⏭`) on the toolbar working.
pub fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "ubuntu-bold".to_owned(),
        std::sync::Arc::new(FontData::from_static(UBUNTU_BOLD)),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "ubuntu-bold".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .push("ubuntu-bold".to_owned());
    fonts
}

/// The colours that change meaning between light and dark. Held as a struct so
/// a call site asks for "the error colour" and gets one that is legible on the
/// surface it is actually drawing on.
#[derive(Clone, Copy)]
pub struct Palette {
    /// Something worked — a document opened, an image described.
    pub ok: Color32,
    /// Something failed.
    pub bad: Color32,
    /// Focus ring, selection, and the chunk being spoken.
    pub accent: Color32,
    /// Something worth knowing before it bites: a model that may not read
    /// images, sentences the wordlists will drop entirely.
    ///
    /// egui's own `warn_fg_color` is `#FF6400` on a light theme, which is about
    /// 2.7:1 on the panel below — the one colour in the default palette that
    /// this app's contrast floor would have failed.
    pub warn: Color32,
}

/// 4.5:1 or better against the light surfaces below.
const LIGHT: Palette = Palette {
    ok: Color32::from_rgb(0x1b, 0x5e, 0x20),
    bad: Color32::from_rgb(0xb7, 0x1c, 0x1c),
    accent: Color32::from_rgb(11, 87, 164),
    warn: Color32::from_rgb(0x9a, 0x5b, 0x00),
};

/// 4.5:1 or better against the dark ones.
const DARK: Palette = Palette {
    ok: Color32::from_rgb(0x81, 0xc9, 0x84),
    bad: Color32::from_rgb(0xff, 0x8a, 0x80),
    accent: Color32::from_rgb(124, 187, 255),
    warn: Color32::from_rgb(0xff, 0xa7, 0x26),
};

/// The palette matching whichever theme is currently in force.
pub fn palette(visuals: &Visuals) -> Palette {
    if visuals.dark_mode {
        DARK
    } else {
        LIGHT
    }
}

fn light_visuals() -> Visuals {
    let mut visuals = Visuals::light();
    let text = Color32::from_rgb(18, 22, 28);

    visuals.panel_fill = Color32::from_rgb(244, 246, 249);
    visuals.window_fill = Color32::WHITE;
    visuals.extreme_bg_color = Color32::WHITE;
    visuals.faint_bg_color = Color32::from_rgb(236, 239, 243);
    visuals.hyperlink_color = LIGHT.accent;
    visuals.error_fg_color = LIGHT.bad;
    visuals.warn_fg_color = LIGHT.warn;
    visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(150, 158, 168));

    // Control surfaces: white fills with a stroke dark enough to be a real
    // boundary rather than a suggestion.
    visuals.widgets.noninteractive.bg_fill = visuals.panel_fill;
    visuals.widgets.noninteractive.weak_bg_fill = visuals.panel_fill;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(170, 178, 188));
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.inactive.bg_fill = Color32::WHITE;
    visuals.widgets.inactive.weak_bg_fill = Color32::WHITE;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.5, Color32::from_rgb(96, 105, 116));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(228, 238, 250);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(228, 238, 250);
    visuals.widgets.hovered.bg_stroke = Stroke::new(2.0, LIGHT.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, text);

    // `active` is also what egui uses for the keyboard-focused widget, so this
    // is the focus ring. It is deliberately the loudest thing on screen.
    visuals.widgets.active.bg_fill = Color32::from_rgb(214, 231, 249);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(214, 231, 249);
    visuals.widgets.active.bg_stroke = Stroke::new(3.0, LIGHT.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::from_rgb(8, 12, 18));

    visuals.widgets.open.bg_fill = Color32::WHITE;
    visuals.widgets.open.weak_bg_fill = Color32::WHITE;
    visuals.widgets.open.bg_stroke = Stroke::new(2.0, LIGHT.accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, text);

    visuals.selection.bg_fill = LIGHT.accent;
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    visuals
}

fn dark_visuals() -> Visuals {
    let mut visuals = Visuals::dark();
    let text = Color32::from_rgb(240, 244, 249);

    visuals.panel_fill = Color32::from_rgb(20, 24, 31);
    visuals.window_fill = Color32::from_rgb(28, 33, 41);
    visuals.extreme_bg_color = Color32::from_rgb(13, 16, 21);
    visuals.faint_bg_color = Color32::from_rgb(32, 38, 47);
    visuals.hyperlink_color = DARK.accent;
    visuals.error_fg_color = DARK.bad;
    visuals.warn_fg_color = DARK.warn;
    visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(96, 106, 118));

    visuals.widgets.noninteractive.bg_fill = visuals.panel_fill;
    visuals.widgets.noninteractive.weak_bg_fill = visuals.panel_fill;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(88, 98, 110));
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.inactive.bg_fill = Color32::from_rgb(38, 45, 55);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(38, 45, 55);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.5, Color32::from_rgb(140, 152, 166));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 60, 76);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(48, 60, 76);
    visuals.widgets.hovered.bg_stroke = Stroke::new(2.0, DARK.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.active.bg_fill = Color32::from_rgb(58, 74, 94);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(58, 74, 94);
    visuals.widgets.active.bg_stroke = Stroke::new(3.0, DARK.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);

    visuals.widgets.open.bg_fill = Color32::from_rgb(38, 45, 55);
    visuals.widgets.open.weak_bg_fill = Color32::from_rgb(38, 45, 55);
    visuals.widgets.open.bg_stroke = Stroke::new(2.0, DARK.accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, text);

    visuals.selection.bg_fill = Color32::from_rgb(31, 92, 156);
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    visuals
}

/// Puts the user's light/dark choice into effect.
///
/// Both palettes are registered either way by [`apply`]; all this decides is
/// which of them egui reaches for. `System` hands the question back to the
/// operating system, which is what the app did unconditionally before there
/// was anything to ask.
///
/// Cheap enough to call every frame — it sets one enum in egui's options and
/// rebuilds no fonts.
pub fn apply_appearance(ctx: &egui::Context, appearance: crate::config::Appearance) {
    use crate::config::Appearance;

    ctx.set_theme(match appearance {
        Appearance::System => egui::ThemePreference::System,
        Appearance::Light => egui::ThemePreference::Light,
        Appearance::Dark => egui::ThemePreference::Dark,
    });
}

/// Install the fonts, both palettes, and the spacing. Rebuilding the glyph
/// atlas is expensive, so this runs once, from the constructor.
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
    ctx.set_visuals_of(egui::Theme::Light, light_visuals());
    ctx.set_visuals_of(egui::Theme::Dark, dark_visuals());

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();

        // Room to breathe, which is most of what makes a dense settings panel
        // read as a calm one.
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.interact_size.y = 28.0;

        // egui's default is 60% alpha, which drops the weak text this app uses
        // for every caption and hint below 4.5:1 on both themes. Weak text here
        // is a shade, not a whisper.
        style.visuals.weak_text_alpha = 0.85;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Appearance;

    #[test]
    fn the_bundled_face_leads_the_proportional_chain() {
        let fonts = font_definitions();
        assert!(fonts.font_data.contains_key("ubuntu-bold"));
        assert_eq!(
            fonts.families[&FontFamily::Proportional].first().map(String::as_str),
            Some("ubuntu-bold")
        );
    }

    /// egui's built-in faces must stay behind ours, or any glyph Ubuntu Bold
    /// lacks becomes a tofu box.
    #[test]
    fn egui_fallbacks_are_kept() {
        let fonts = font_definitions();
        assert!(
            fonts.families[&FontFamily::Proportional].len() > 1,
            "the fallback chain was replaced rather than prepended to"
        );
    }

    /// Relative luminance, per WCAG 2.1.
    fn luminance(c: Color32) -> f64 {
        let channel = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        let (lighter, darker) = if x > y { (x, y) } else { (y, x) };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// Every colour that carries meaning has to be readable on the surface it
    /// is drawn on. This is the promise the module header makes; without a test
    /// it is a comment.
    #[test]
    fn meaningful_colours_clear_the_contrast_floor() {
        for (name, palette, visuals) in [
            ("light", LIGHT, light_visuals()),
            ("dark", DARK, dark_visuals()),
        ] {
            for (role, colour) in [
                ("ok", palette.ok),
                ("bad", palette.bad),
                ("warn", palette.warn),
                ("accent", palette.accent),
            ] {
                let ratio = contrast(colour, visuals.panel_fill);
                assert!(
                    ratio >= 4.5,
                    "{name} {role} is {ratio:.2}:1 against the panel, below 4.5:1"
                );
            }
            // The visuals must carry the measured warning colour, not egui's.
            assert_eq!(visuals.warn_fg_color, palette.warn, "{name}");
        }
    }

    /// Each choice reaches the theme it names. Worth pinning down because the
    /// two enums have the same three variants, so getting the mapping backwards
    /// would compile perfectly and hand somebody who asked for dark a white
    /// screen.
    #[test]
    fn each_appearance_selects_the_theme_it_names() {
        for (appearance, expected) in [
            (Appearance::System, egui::ThemePreference::System),
            (Appearance::Light, egui::ThemePreference::Light),
            (Appearance::Dark, egui::ThemePreference::Dark),
        ] {
            let ctx = egui::Context::default();
            apply_appearance(&ctx, appearance);
            assert_eq!(
                ctx.options(|options| options.theme_preference),
                expected,
                "{appearance:?} chose the wrong theme"
            );
        }
    }

    /// Both palettes stay registered whichever way the preference points: a
    /// `Light` choice that had left the dark visuals unset would fall back to
    /// egui's defaults, contrast measurements and all, the moment the user
    /// switched over.
    #[test]
    fn both_palettes_survive_a_choice_of_either() {
        let ctx = egui::Context::default();
        apply(&ctx);
        apply_appearance(&ctx, Appearance::Dark);

        assert!(ctx.style_of(egui::Theme::Dark).visuals.dark_mode);
        assert!(!ctx.style_of(egui::Theme::Light).visuals.dark_mode);
        assert_eq!(
            ctx.style_of(egui::Theme::Light).visuals.panel_fill,
            light_visuals().panel_fill
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.panel_fill,
            dark_visuals().panel_fill
        );
    }
}
