// Desktop appearance (light/dark + accent), read from the OS rather than
// guessed.
//
// The source is the XDG desktop portal's `org.freedesktop.appearance`
// namespace — the ONE cross-desktop answer. KDE, GNOME, and anything else that
// implements the Settings portal all publish `color-scheme` and `accent-color`
// there, so this file never reads `kdeglobals`, never shells out to
// `gsettings`, and never learns which desktop it is running on. That matters
// twice over inside the flatpak: the sandbox cannot see the host's config files
// at all, but the portal is available with no extra permission (verified on
// KDE Plasma 6 / Wayland, which reports `color-scheme: 1` and a live accent).
//
// What the OS gets to decide, and what it does not:
//
//   * LIGHT vs DARK — the OS decides, fully. This is the setting a user
//     actually changes, often on a schedule, and an app that ignores it is the
//     one bright window at midnight.
//   * ACCENT — the OS decides, and it tints CHROME: selection, focus rings,
//     the active nav row. The places a desktop expects to see "your" colour.
//   * The HIVE PALETTE — the app decides. Honey gold on warm dark is the
//     product's identity, not a default waiting to be overridden, so primary
//     actions stay gold even when the desktop accent is blue. Following the
//     accent everywhere would make hive look like every other window, which is
//     the opposite of honouring a theme; it is dissolving into one.
//
// A portal that is missing, slow, or answering nonsense is not an error. There
// is always a complete answer without it (dark, gold accent), because a
// journal that refuses to open because D-Bus is busy would be a worse app than
// one that opened in the wrong shade.

use std::time::Duration;

/// How long the portal gets to answer before the app stops waiting and opens
/// in the default appearance. Startup is not the place to block on a bus.
const PORTAL_TIMEOUT: Duration = Duration::from_millis(600);

/// What the desktop asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appearance {
    pub dark: bool,
    /// `#rrggbb` from the portal's accent, or `None` when the desktop has no
    /// opinion — in which case chrome falls back to the brand gold.
    pub accent: Option<String>,
}

impl Default for Appearance {
    /// Dark, brand accent. The shipped look, and what every failure path
    /// resolves to.
    fn default() -> Self {
        Appearance {
            dark: true,
            accent: None,
        }
    }
}

impl Appearance {
    /// Ask the portal. Never fails and never hangs: anything unexpected — a
    /// missing portal, a wedged bus, a nonsense payload — is
    /// [`Appearance::default`].
    ///
    /// The read runs on its own thread with a hard deadline rather than
    /// leaning on zbus's default method timeout, which is tens of seconds.
    /// This is on the startup path, and no colour is worth a frozen window.
    pub fn detect() -> Appearance {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // A send failure means the deadline already passed and the
            // receiver is gone. Nothing to report to.
            let _ = tx.send(read_portal());
        });
        match rx.recv_timeout(PORTAL_TIMEOUT) {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                tracing::debug!("appearance portal unavailable, using defaults: {e:#}");
                Appearance::default()
            }
            Err(_) => {
                tracing::debug!("appearance portal did not answer in time, using defaults");
                Appearance::default()
            }
        }
    }

    /// The `:root` custom properties every style in the app resolves against.
    ///
    /// One block, swapped whole. The alternative — threading a theme struct
    /// through several hundred inline `style:` attributes — would put the
    /// theme in the render path instead of in the stylesheet, and re-render
    /// the world to change a colour.
    pub fn css_root(&self) -> String {
        let p = if self.dark { DARK } else { LIGHT };
        // The accent tints chrome only. `--gold` is the brand and does not
        // move; `--accent` falls back to it so a desktop with no accent still
        // looks deliberate rather than grey.
        let accent = self.accent.as_deref().unwrap_or(p.gold_text);
        format!(
            ":root {{ \
             --bg: {}; --panel: {}; --edge: {}; --ink: {}; --dim: {}; --faint: {}; \
             --gold: {}; --gold-text: {}; --danger: {}; --accent: {}; \
             color-scheme: {}; }} \
             ::selection {{ background: {}; color: {}; }} \
             :focus-visible {{ outline: 2px solid {}; outline-offset: 2px; }}",
            p.bg,
            p.panel,
            p.edge,
            p.ink,
            p.dim,
            p.faint,
            p.gold,
            p.gold_text,
            p.danger,
            accent,
            if self.dark { "dark" } else { "light" },
            accent,
            p.bg,
            accent,
        )
    }
}

/// One theme's colours. Two instances, below.
struct Palette {
    bg: &'static str,
    panel: &'static str,
    edge: &'static str,
    ink: &'static str,
    dim: &'static str,
    faint: &'static str,
    /// Gold as a FILL — a button background, with dark text on it. Identical
    /// in both themes: it is the brand.
    gold: &'static str,
    /// Gold as TEXT, on that theme's background. Darker in the light theme,
    /// because #e2a921 on white is about 2:1 and unreadable.
    gold_text: &'static str,
    danger: &'static str,
}

/// The shipped look: warm dark, honey gold.
const DARK: Palette = Palette {
    bg: "#14120e",
    panel: "#1a1712",
    edge: "#2c2818",
    ink: "#e8e2d4",
    dim: "#9a927e",
    faint: "#6f684f",
    gold: "#e2a921",
    gold_text: "#e2a921",
    danger: "#e07a5f",
};

/// The same identity in daylight: warm paper rather than clinical white, so
/// the gold still belongs to it.
const LIGHT: Palette = Palette {
    bg: "#faf7ef",
    panel: "#ffffff",
    edge: "#e0d9c6",
    ink: "#241f16",
    dim: "#5f5847",
    faint: "#8c8470",
    gold: "#e2a921",
    // Contrast, not taste: the fill gold fails on paper as text.
    gold_text: "#8a6410",
    danger: "#b8412a",
};

/// Read `org.freedesktop.appearance` from the Settings portal.
fn read_portal() -> anyhow::Result<Appearance> {
    use zbus::blocking::Connection;

    let conn = Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )?;

    let dark = read_one(&proxy, "color-scheme")
        .and_then(|v| scheme_u32(&v))
        // 0 = no preference, 1 = prefer dark, 2 = prefer light. "No
        // preference" keeps the shipped look rather than inventing one.
        .map(|scheme| scheme != 2)
        .unwrap_or(true);

    let accent = read_one(&proxy, "accent-color").and_then(|v| accent_hex(&v));

    Ok(Appearance { dark, accent })
}

/// One key, or `None` — a portal that does not publish a key is not an error,
/// it is a desktop with no opinion about it.
fn read_one(proxy: &zbus::blocking::Proxy<'_>, key: &str) -> Option<zbus::zvariant::OwnedValue> {
    proxy
        .call::<_, _, zbus::zvariant::OwnedValue>("Read", &("org.freedesktop.appearance", key))
        .ok()
}

/// Strip the variant layers `Settings.Read` wraps its answer in.
///
/// `Read` is specified to return `v`, so the value is ALWAYS at least one
/// variant deep — and some implementations nest it twice. Downcasting the
/// outer value directly therefore fails on a perfectly good answer, which is a
/// silent fall back to defaults rather than an error anyone would notice.
fn unwrap_variants(value: &zbus::zvariant::Value<'_>) -> Option<zbus::zvariant::Value<'static>> {
    let mut v = value.try_clone().ok()?.try_to_owned().ok()?.into();
    for _ in 0..3 {
        match v {
            zbus::zvariant::Value::Value(inner) => {
                v = inner.try_clone().ok()?.try_to_owned().ok()?.into()
            }
            other => return Some(other),
        }
    }
    Some(v)
}

/// The portal's `color-scheme`: 0 no preference, 1 dark, 2 light.
fn scheme_u32(value: &zbus::zvariant::Value<'_>) -> Option<u32> {
    match unwrap_variants(value)? {
        zbus::zvariant::Value::U32(n) => Some(n),
        _ => None,
    }
}

/// The portal's accent is `(ddd)` — three 0.0..=1.0 doubles, NOT bytes.
fn accent_hex(value: &zbus::zvariant::Value<'_>) -> Option<String> {
    let zbus::zvariant::Value::Structure(s) = unwrap_variants(value)? else {
        return None;
    };
    let fields = s.fields();
    if fields.len() != 3 {
        return None;
    }
    let mut rgb = [0u8; 3];
    for (i, f) in fields.iter().enumerate() {
        let c: f64 = f.downcast_ref::<f64>().ok()?;
        if !c.is_finite() || !(0.0..=1.0).contains(&c) {
            return None;
        }
        rgb[i] = (c * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    Some(format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_shipped_look() {
        let a = Appearance::default();
        assert!(
            a.dark,
            "a failed portal read must not flip the app to light"
        );
        assert_eq!(a.accent, None, "no accent means the brand gold");
    }

    #[test]
    fn each_theme_defines_every_token_a_style_can_reference() {
        // The failure this catches is a blank screen: one missing custom
        // property and every rule using it silently drops.
        for dark in [true, false] {
            let css = Appearance { dark, accent: None }.css_root();
            for token in [
                "--bg:",
                "--panel:",
                "--edge:",
                "--ink:",
                "--dim:",
                "--faint:",
                "--gold:",
                "--gold-text:",
                "--danger:",
                "--accent:",
            ] {
                assert!(css.contains(token), "dark={dark} is missing {token}");
            }
        }
    }

    #[test]
    fn the_scheme_hint_follows_the_theme_so_native_widgets_agree() {
        // `color-scheme` is what makes date pickers, scrollbars and form
        // controls match; hard-coding it dark was why they stayed dark.
        assert!(Appearance {
            dark: true,
            accent: None
        }
        .css_root()
        .contains("color-scheme: dark"));
        assert!(Appearance {
            dark: false,
            accent: None
        }
        .css_root()
        .contains("color-scheme: light"));
    }

    #[test]
    fn an_accent_tints_chrome_and_never_replaces_the_brand() {
        let css = Appearance {
            dark: true,
            accent: Some("#315bef".to_string()),
        }
        .css_root();
        assert!(css.contains("--accent: #315bef"));
        assert!(
            css.contains("--gold: #e2a921"),
            "the desktop accent must not repaint the brand"
        );
        assert!(
            css.contains("::selection { background: #315bef"),
            "chrome is where the accent belongs"
        );
    }

    #[test]
    fn a_missing_accent_falls_back_to_gold_rather_than_to_nothing() {
        let css = Appearance {
            dark: true,
            accent: None,
        }
        .css_root();
        assert!(
            css.contains("--accent: #e2a921"),
            "an unset accent must still resolve, or focus rings vanish"
        );
    }

    #[test]
    fn the_light_theme_darkens_gold_for_text_but_not_for_fills() {
        let light = Appearance {
            dark: false,
            accent: None,
        }
        .css_root();
        // The fill is the brand and does not move; the text form has to, or it
        // sits at roughly 2:1 on paper.
        assert!(light.contains("--gold: #e2a921"));
        assert!(light.contains("--gold-text: #8a6410"));
    }

    #[test]
    fn a_hostile_accent_payload_is_declined_rather_than_rendered() {
        use zbus::zvariant::{Structure, Value};

        // Out of range, wrong arity, and wrong type all mean "no accent", not
        // a garbage colour spliced into the stylesheet.
        let too_big = Value::from(Structure::from((2.0f64, 0.5f64, 0.5f64)));
        assert_eq!(accent_hex(&too_big), None);
        let two_fields = Value::from(Structure::from((0.5f64, 0.5f64)));
        assert_eq!(accent_hex(&two_fields), None);
        let not_a_struct = Value::from(42u32);
        assert_eq!(accent_hex(&not_a_struct), None);
        let nan = Value::from(Structure::from((f64::NAN, 0.5f64, 0.5f64)));
        assert_eq!(accent_hex(&nan), None);
    }

    #[test]
    fn a_variant_wrapped_answer_is_read_rather_than_silently_ignored() {
        use zbus::zvariant::{Structure, Value};

        // `Settings.Read` returns `v`, so the payload is ALWAYS at least one
        // variant deep. Downcasting the outer value failed on a good answer
        // and fell back to defaults with nothing logged — the app just wore
        // the wrong theme.
        // `Value::from(Value)` is identity, so the nesting has to be built
        // with the variant constructor itself — which is exactly the shape
        // that arrives off the bus.
        let nest = |v: Value<'static>| Value::Value(Box::new(v));

        let one_deep = nest(Value::from(1u32));
        assert_eq!(scheme_u32(&one_deep), Some(1));
        let two_deep = nest(nest(Value::from(2u32)));
        assert_eq!(scheme_u32(&two_deep), Some(2));
        assert_eq!(scheme_u32(&Value::from(0u32)), Some(0));
        // Still refuses what is genuinely not a scheme.
        assert_eq!(scheme_u32(&Value::from("dark")), None);

        let wrapped_accent = nest(Value::from(Structure::from((0.0f64, 0.0f64, 1.0f64))));
        assert_eq!(accent_hex(&wrapped_accent), Some("#0000ff".to_string()));
    }

    #[test]
    fn a_well_formed_accent_becomes_the_hex_the_stylesheet_wants() {
        use zbus::zvariant::{Structure, Value};

        // The live value from KDE Plasma 6 on this machine — the portal sends
        // 49/91/239 as f32-precision doubles, which is why these are not
        // rounder numbers.
        let kde = Value::from(Structure::from((
            0.192_156_86_f64,
            0.356_862_75_f64,
            0.937_254_9_f64,
        )));
        assert_eq!(accent_hex(&kde), Some("#315bef".to_string()));

        let black = Value::from(Structure::from((0.0f64, 0.0f64, 0.0f64)));
        assert_eq!(accent_hex(&black), Some("#000000".to_string()));
        let white = Value::from(Structure::from((1.0f64, 1.0f64, 1.0f64)));
        assert_eq!(accent_hex(&white), Some("#ffffff".to_string()));
    }
}
