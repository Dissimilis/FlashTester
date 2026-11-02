//! Chinese (zh-CN) localization support.
//!
//! The bundled Ubuntu fonts contain no CJK glyphs, so without a fallback the GUI
//! would render Chinese as tofu boxes. Instead of shipping a multi-megabyte CJK
//! font, we load a font that Windows already has on every install and prepend it
//! to egui's font fallback chain. This keeps the binary tiny and needs no network.
//!
//! On non-Windows builds (Linux/BSD) this is a no-op: those use system fonts and
//! the caller only translates on the Windows GUI path anyway.

use egui::{FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

/// Candidate system CJK fonts, in preference order. Windows 10/11 ship at least
/// one of these under %WINDIR%\Fonts.
const SYSTEM_CJK: &[(&str, &str)] = &[
    // Microsoft YaHei (Simplified Chinese UI font on Win10/11).
    ("msyh.ttc", "Microsoft YaHei"),
    ("msyhbd.ttc", "Microsoft YaHei Bold"),
    // SimSun (very widely present, also covers GB2312/Big5 via the .ttc).
    ("simsun.ttc", "SimSun"),
    ("simsunb.ttf", "SimSun Ext-B"),
    // Fallback for Traditional / broader coverage.
    ("msjh.ttc", "Microsoft JhengHei"),
];

/// Read a Windows system font file and return its bytes, or None if absent.
#[cfg(windows)]
fn read_system_font(file_name: &str) -> Option<Vec<u8>> {
    let windir = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot"))?;
    let path = std::path::Path::new(&windir).join("Fonts").join(file_name);
    std::fs::read(&path).ok()
}

#[cfg(not(windows))]
fn read_system_font(_file_name: &str) -> Option<Vec<u8>> {
    None
}

/// Build the font definitions with a CJK fallback prepended to every family that
/// egui searches. `base` is the project's own `fonts()` result; we only extend
/// the fallback lists (the `Families` entry at the end of each list).
pub fn fonts_with_cjk(base: FontDefinitions) -> FontDefinitions {
    let mut f = base;
    // Collect any system CJK font we can find; use the first that loads.
    let mut inserted: Vec<(String, Arc<FontData>)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for (file_name, family) in SYSTEM_CJK {
        if let Some(bytes) = read_system_font(file_name) {
            let data = FontData::from_owned(bytes).into();
            let key = format!("CJK-{}", family);
            inserted.push((key.clone(), data));
            names.push(key);
            break;
        }
    }
    if names.is_empty() {
        // No system CJK font found: leave the (English) UI as-is rather than failing.
        return f;
    }
    for (key, data) in inserted {
        f.font_data.insert(key, data);
    }
    // Prepend the CJK family to every proportional/mono family's fallback list so
    // any glyph missing from Ubuntu falls through to the CJK font.
    let families = f.families.clone();
    for (family, list) in families.iter() {
        let mut new_list = names.clone();
        new_list.extend(list.iter().cloned());
        f.families.insert(family.clone(), new_list);
    }
    f
}

/// Convenience: register the CJK-aware font set on a context.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts_with_cjk(super::gui_kit::fonts()));
}
