#![allow(unused, dead_code)]

use std::{borrow::Cow, fmt::{Write, Display}, path::MAIN_SEPARATOR};

use smallstr::SmallString;

#[inline]
pub fn format_bytes(bytes: usize) -> SmallString<[u8; 16]> {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;

    let mut s = SmallString::new();

    if b >= GB {
        _ = write!(&mut s, "{:.2} GB", b / GB);
    } else if b >= MB {
        _ = write!(&mut s, "{:.2} MB", b / MB);
    } else if b >= KB {
        _ = write!(&mut s, "{:.2} KB", b / KB);
    } else {
        _ = write!(&mut s, "{bytes} B");
    }

    s
}

// pixel coords -> NDC (Y flipped: screen top = NDC +1)
#[inline]
pub fn px(x: f32, y: f32, sw: f32, sh: f32) -> [f32; 2] {
    [(x / sw) * 2.0 - 1.0, 1.0 - (y / sh) * 2.0]
}

#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[inline]
pub fn display_path<'a>(path: &'a str, max_chars: usize) -> Cow<'a, str> {
    if path.len() <= max_chars {
        return path.into();
    }

    // find a slash after the cut point so we don't break mid-component
    let cut = path.len() - max_chars + 1; // +1 for the ellipsis char
    if let Some(slash) = path[cut..].find(MAIN_SEPARATOR) {
        format!("…{}", &path[cut + slash..]).into()
    } else {
        format!("…{}", &path[path.len() - max_chars + 1..]).into()
    }
}

#[inline]
pub fn open_in_emacs(path: &str, line: impl Display) {
    std::process::Command::new("emacsclient")
        .arg("--alternate-editor=")
        .arg(format!("+{line}"))
        .arg(path)
        .spawn()
        .ok();
}
