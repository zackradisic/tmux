//! Text helpers a picker draws with.

/// Clip to `max` cells, ending in an ellipsis when something was cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Flatten text that came from outside onto one line. A harness writes
/// its own words into a note (the Claude `Notification` message is a
/// sentence, sometimes two), and a newline or a stray control character
/// inside a row would tear the list apart.
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if c.is_control() {
            gap = !out.is_empty();
        } else {
            if gap {
                out.push(' ');
                gap = false;
            }
            out.push(c);
        }
    }
    out
}

pub fn keyname(k: &str) -> &str {
    match k {
        "Escape" => "Esc",
        "Enter" => "Enter",
        other => other,
    }
}

/// A short, readable key label: `C-f` shows as `^F`.
pub fn pretty_key(k: &str) -> String {
    if let Some(rest) = k.strip_prefix("C-") {
        format!("^{}", rest.to_uppercase())
    } else {
        keyname(k).to_string()
    }
}

pub fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Drop SGR escape sequences so the reverse-video selection line does not
/// carry a colour that would reset the inversion mid-row.
pub fn strip_sgr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `~/...` for a path under the home directory.
pub fn tilde_of(home: &Option<String>, path: &str) -> String {
    match home {
        Some(h) if path.starts_with(h.as_str()) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    }
}

/// Text that is safe to drop into a tmux command string as a
/// single-quoted argument. tmux's single quotes take no escapes, so a
/// quote inside one cannot be escaped - it can only be removed. `#` goes
/// too: a menu name is format-expanded, and `#{...}` from an agent's own
/// title is not something to hand to the format parser.
pub fn menu_safe(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !matches!(c, '\'' | '"' | '#' | '\\' | ';' | '$'))
        .collect();
    clip(cleaned.trim(), max)
}

/// One row of a `display-menu`: a label, the picker key it stands for,
/// and whether it applies right now. Choosing it runs
/// `plugin-command <plugin> 'menu-key <key>'`, so the plugin handles the
/// menu like a key press. A name starting with `-` is what tmux draws
/// dimmed and refuses to select, so an action that does not apply is
/// still SHOWN - the menu is the place you go to find out what you can
/// do, and a silently missing line answers nothing.
pub fn menu_item(plugin: &str, label: &str, key: &str, enabled: bool) -> String {
    let name = if enabled {
        label.to_string()
    } else {
        format!("-{label}")
    };
    format!(" '{}' '{}' \"plugin-command {} 'menu-key {}'\"", name, key, plugin, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_and_flatten() {
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
        assert_eq!(one_line("a\nb\t\tc"), "a b c");
        assert_eq!(one_line("\n\nlead"), "lead");
        assert_eq!(strip_sgr("\x1b[1;31mred\x1b[0m"), "red");
        assert_eq!(tilde_of(&Some("/home/u".into()), "/home/u/x"), "~/x");
        assert_eq!(tilde_of(&None, "/home/u/x"), "/home/u/x");
    }

    #[test]
    fn labels() {
        assert_eq!(pretty_key("C-f"), "^F");
        assert_eq!(pretty_key("Escape"), "Esc");
        assert_eq!(fmt_age(59), "59s");
        assert_eq!(fmt_age(3700), "1h01m");
        assert_eq!(menu_safe("it's #1; \"x\"", 20), "its 1 x");
        assert!(menu_item("agents", "jump", "Enter", false).starts_with(" '-jump'"));
    }
}
