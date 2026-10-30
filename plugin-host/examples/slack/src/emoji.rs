//! Slack's emoji: `:name:` shortcodes in message text and bare names in
//! reactions, drawn as the glyph.
//!
//! The standard names are iamcal/emoji-data's `short_names` (what Slack
//! ships), generated into `emoji_table.rs`; a `::skin-tone-N` suffix
//! (N in 2..=6) adds the modifier after the base scalar. Custom
//! workspace emoji come from `emoji.list`: an alias (`alias:other`)
//! resolves to the standard or custom emoji it names, an image-only one
//! has no glyph and stays `:name:`. The list is kept in the `settings`
//! table so it is there before the first fetch of a run and refetched
//! once per full sync.
//!
//! Glyphs go into the cell model through `listkit::styled::push_glyph`
//! (one cell plus a filler for a two-column glyph, a multi-scalar
//! sequence as one registered cell), so column math stays right.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;

use listkit::styled::{push_glyph, Styled};
use serde_json::Value;

use crate::emoji_table::TABLE;
use crate::store::{self, Identity};
use crate::http;

/// The settings key the `emoji.list` JSON is kept under.
pub const SETTING: &str = "emoji_list";

/// tmux's `UTF8_SIZE`: a longer sequence does not combine into one cell
/// there, so such a skin-toned glyph falls back to its base.
const MAX_GLYPH_BYTES: usize = 32;

thread_local! {
    /// Custom emoji: name -> image url or `alias:other`.
    static CUSTOM: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

/// Install the custom set from an `emoji.list` response (or its
/// `emoji` object). Returns how many there are.
pub fn set_custom_json(json: &str) -> usize {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let map = v.get("emoji").unwrap_or(&v);
    let mut out = HashMap::new();
    if let Some(obj) = map.as_object() {
        for (name, url) in obj {
            if let Some(u) = url.as_str() {
                out.insert(name.clone(), u.to_string());
            }
        }
    }
    let n = out.len();
    CUSTOM.with(|c| *c.borrow_mut() = out);
    n
}

/// Forget the custom set (tests).
#[cfg(test)]
pub fn clear_custom() {
    CUSTOM.with(|c| c.borrow_mut().clear());
}

/// The custom set from the store, at boot.
pub async fn load_custom() {
    if let Some(json) = store::setting(SETTING).await {
        set_custom_json(&json);
    }
}

/// Fetch `emoji.list`, keep it, install it. Returns the count.
pub async fn refresh_custom(id: &Identity) -> Result<usize, String> {
    let v = http::call(id, "emoji.list", &[]).await?;
    let json = Value::Object(
        [("emoji".to_string(), v.get("emoji").cloned().unwrap_or(Value::Object(Default::default())))]
            .into_iter()
            .collect(),
    )
    .to_string();
    store::set_setting(SETTING, &json).await;
    Ok(set_custom_json(&json))
}

fn standard(name: &str) -> Option<&'static str> {
    TABLE.binary_search_by(|(n, _)| (*n).cmp(name)).ok().map(|i| TABLE[i].1)
}

/// `skin-tone-2`..`skin-tone-6` as the modifier scalar.
fn skin_tone(suffix: &str) -> Option<char> {
    let n = suffix.strip_prefix("skin-tone-")?;
    match n {
        "2" => Some('\u{1F3FB}'),
        "3" => Some('\u{1F3FC}'),
        "4" => Some('\u{1F3FD}'),
        "5" => Some('\u{1F3FE}'),
        "6" => Some('\u{1F3FF}'),
        _ => None,
    }
}

/// The glyph of a bare name (no colons), aliases followed; `None` for an
/// unknown name or an image-only custom emoji.
fn resolve(name: &str, depth: u8) -> Option<String> {
    if let Some(g) = standard(name) {
        return Some(g.to_string());
    }
    if depth > 8 {
        return None;
    }
    let target = CUSTOM.with(|c| c.borrow().get(name).cloned())?;
    let alias = target.strip_prefix("alias:")?;
    resolve(alias, depth + 1)
}

/// The modifier placed after the base scalar (a FE0F right after the
/// base goes: the modified form has no text/emoji choice to make).
fn with_skin(glyph: &str, modifier: char) -> String {
    let mut chars = glyph.chars();
    let mut out = String::with_capacity(glyph.len() + 4);
    let Some(base) = chars.next() else { return glyph.to_string() };
    out.push(base);
    out.push(modifier);
    let mut rest = chars.peekable();
    if rest.peek() == Some(&'\u{FE0F}') {
        rest.next();
    }
    out.extend(rest);
    if out.len() > MAX_GLYPH_BYTES {
        return glyph.to_string();
    }
    out
}

/// The glyph for a Slack emoji name, as text (`+1`, `+1::skin-tone-3`,
/// a custom alias), or `None` when there is nothing to draw.
pub fn lookup(name: &str) -> Option<String> {
    let (base, tone) = match name.split_once("::") {
        Some((b, t)) => (b, Some(t)),
        None => (name, None),
    };
    let glyph = resolve(base, 0)?;
    match tone {
        None => Some(glyph),
        Some(t) => Some(with_skin(&glyph, skin_tone(t)?)),
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+')
}

/// Find the shortcode at `text[i..]` (which starts with `:`): the name
/// (with any `::skin-tone-N`) and the index just past its closing colon.
fn shortcode_at(text: &str, i: usize) -> Option<(&str, usize)> {
    let rest = &text[i + 1..];
    let n = rest.find(|c: char| !is_name_char(c))?;
    if n == 0 || !rest[n..].starts_with(':') {
        return None;
    }
    let name = &rest[..n];
    let end = i + 1 + n + 1;
    // `:+1::skin-tone-3:` - a tone glued to the closing colon.
    if let Some(after) = text[end..].strip_prefix(':') {
        if let Some(m) = after.find(|c: char| !is_name_char(c)) {
            if after[m..].starts_with(':') && skin_tone(&after[..m]).is_some() {
                return Some((&text[i + 1..end + 1 + m], end + 1 + m + 1));
            }
        }
    }
    Some((name, end))
}

/// Walk `text`, calling `f(piece, is_glyph)` for each run without a
/// known shortcode (`false`) and each shortcode's glyph (`true`).
fn walk(text: &str, mut f: impl FnMut(&str, bool)) {
    let mut from = 0;
    let mut i = 0;
    while let Some(off) = text[i..].find(':') {
        let at = i + off;
        match shortcode_at(text, at) {
            Some((name, end)) => match lookup(name) {
                Some(g) => {
                    f(&text[from..at], false);
                    f(&g, true);
                    from = end;
                    i = end;
                }
                // Unknown: the closing colon may open the next one.
                None => i = end - 1,
            },
            None => i = at + 1,
        }
    }
    f(&text[from..], false);
}

/// `text` with every known `:name:` replaced by its glyph; unknown names,
/// times (`12:30`), urls and `::` stay as they are. The string-level
/// counterpart of [`push_text`] for text that never becomes cells (the
/// views and `mrkdwn::plain` all go through cells, so nothing calls it
/// yet).
#[allow(dead_code)]
pub fn render_shortcodes(text: &str) -> Cow<'_, str> {
    if !text.contains(':') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    walk(text, |p, is_glyph| {
        out.push_str(p);
        changed |= is_glyph;
    });
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// `text` as cells in `style`, shortcodes drawn as glyphs and every wide
/// or multi-scalar glyph laid out as the cell model wants.
pub fn push_text(out: &mut Vec<Styled>, text: &str, style: u16) {
    walk(text, |p, _| push_glyph(out, p, style));
}

#[cfg(test)]
mod tests {
    use super::*;
    use listkit::styled::{cells_text, FILL};

    #[test]
    fn standard_names() {
        assert_eq!(lookup("+1").as_deref(), Some("\u{1F44D}"));
        assert_eq!(lookup("thumbsup").as_deref(), Some("\u{1F44D}"));
        assert_eq!(lookup("+1::skin-tone-3").as_deref(), Some("\u{1F44D}\u{1F3FC}"));
        assert_eq!(lookup("heart").as_deref(), Some("\u{2764}\u{FE0F}"));
        assert_eq!(lookup("white_check_mark").as_deref(), Some("\u{2705}"));
        assert_eq!(lookup("simple_smile").as_deref(), Some("\u{1F642}"));
        assert_eq!(lookup("man-facepalming").as_deref(), Some("\u{1F926}\u{200D}\u{2642}\u{FE0F}"));
        // The tone goes after the base scalar of a ZWJ sequence, and
        // replaces a FE0F that sat there.
        assert_eq!(lookup("man-facepalming::skin-tone-5").as_deref(), Some("\u{1F926}\u{1F3FE}\u{200D}\u{2642}\u{FE0F}"));
        assert_eq!(lookup("point_up::skin-tone-2").as_deref(), Some("\u{261D}\u{1F3FB}"));
        assert_eq!(lookup("unknown"), None);
        assert_eq!(lookup("+1::skin-tone-9"), None);
        assert_eq!(lookup(""), None);
    }

    #[test]
    fn text_rendering() {
        clear_custom();
        assert_eq!(render_shortcodes("yay :raised_hands: :fire:"), "yay \u{1F64C} \u{1F525}");
        assert_eq!(render_shortcodes("hi :+1::skin-tone-3: there"), "hi \u{1F44D}\u{1F3FC} there");
        assert_eq!(render_shortcodes(":unknown: stays"), ":unknown: stays");
        assert_eq!(render_shortcodes("at 12:30 and 1:45:00"), "at 12:30 and 1:45:00");
        assert_eq!(render_shortcodes("see https://x.y/z :: ok"), "see https://x.y/z :: ok");
        // An unknown name's closing colon may open the next shortcode.
        assert_eq!(render_shortcodes("a:zz:c:+1:"), "a:zz:c\u{1F44D}");
        assert_eq!(render_shortcodes("::+1::"), ":\u{1F44D}:");
        assert!(matches!(render_shortcodes("no emoji here"), Cow::Borrowed(_)));
        assert!(matches!(render_shortcodes(":nope:"), Cow::Borrowed(_)));
    }

    #[test]
    fn aliases_resolve() {
        let n = set_custom_json(
            r#"{"ok":true,"emoji":{"party":"alias:tada","yay":"alias:party","morph":"https://emoji.slack-edge.com/T1/morph/abc.png","loop1":"alias:loop2","loop2":"alias:loop1"}}"#,
        );
        assert_eq!(n, 5);
        assert_eq!(lookup("party").as_deref(), Some("\u{1F389}"));
        assert_eq!(lookup("yay").as_deref(), Some("\u{1F389}"));
        assert_eq!(lookup("morph"), None);
        assert_eq!(lookup("loop1"), None);
        assert_eq!(render_shortcodes(":morph: :yay:"), ":morph: \u{1F389}");
        // An alias to a custom emoji with an image.
        set_custom_json(r#"{"emoji":{"m2":"alias:morph","morph":"https://x/y.png"}}"#);
        assert_eq!(lookup("m2"), None);
        clear_custom();
        assert_eq!(lookup("party"), None);
    }

    #[test]
    fn cells_keep_columns() {
        clear_custom();
        let mut c = Vec::new();
        push_text(&mut c, "ok :+1: :man-facepalming::skin-tone-3: 12:30", 0);
        assert_eq!(cells_text(&c), "ok \u{1F44D} \u{1F926}\u{1F3FC}\u{200D}\u{2642}\u{FE0F} 12:30");
        // "ok " + 2 + " " + 2 + " 12:30" columns.
        assert_eq!(c.len(), 3 + 2 + 1 + 2 + 6);
        assert_eq!(c[4].0, FILL);
        assert_eq!(c[7].0, FILL);
    }
}
