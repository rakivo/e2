use std::{borrow::Cow, fmt::Display};

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
pub fn lerp_color(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [lerp(a[0],b[0],t), lerp(a[1],b[1],t), lerp(a[2],b[2],t), lerp(a[3],b[3],t)]
}

#[inline]
pub fn display_path<'a>(path: &'a str, max_chars: usize) -> Cow<'a, str> {
    if path.len() <= max_chars {
        return path.into();
    }

    // find a slash after the cut point so we don't break mid-component
    let cut = path.len() - max_chars + 1; // +1 for the ellipsis char
    if let Some(slash) = path[cut..].find('/') {
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
