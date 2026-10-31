//! Slack's `mrkdwn` as styled cells: `*bold*`, `_italic_`, `~strike~`,
//! `` `code` ``, ```fences```, `>` quotes, and the angle-bracket tokens
//! (`<@U…>`, `<#C…|name>`, `<!here>`, `<url|label>`), with `&amp;`
//! style entities undone, `:emoji:` shortcodes drawn as the glyph
//! (outside code). Names come from the caller (the user and channel
//! caches); an id nobody knows is drawn as itself.

use listkit::styled::{cells_text, hard_wrap, wrap_cells, Styled, ST_BOLD, ST_CODE, ST_CYAN, ST_DIM, ST_ITALIC, ST_UNDER};

use crate::emoji;

/// How ids become names.
pub struct Names<'a> {
    pub user: &'a dyn Fn(&str) -> Option<String>,
    pub channel: &'a dyn Fn(&str) -> Option<String>,
}

impl Names<'_> {
    pub fn none() -> Names<'static> {
        Names { user: &|_| None, channel: &|_| None }
    }
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
}

fn push_text(out: &mut Vec<Styled>, text: &str, style: u16) {
    let text = unescape(text);
    if style & ST_CODE != 0 {
        // Code is literal: no shortcodes, and a wide glyph in it is on
        // its own (a filler cell would show as a gap in the run).
        out.extend(text.chars().filter(|c| !c.is_control()).map(|c| (c, style)));
    } else {
        emoji::push_text(out, &text, style);
    }
}

/// An angle-bracket token's cells.
fn token(inner: &str, base: u16, names: &Names) -> Vec<Styled> {
    let mut out = Vec::new();
    let (head, label) = match inner.split_once('|') {
        Some((h, l)) => (h, Some(l)),
        None => (inner, None),
    };
    if let Some(id) = head.strip_prefix('@') {
        let name = label.map(str::to_string).or_else(|| (names.user)(id)).unwrap_or_else(|| id.to_string());
        push_text(&mut out, &format!("@{name}"), base | ST_BOLD | ST_CYAN);
    } else if let Some(id) = head.strip_prefix('#') {
        let name = label.map(str::to_string).or_else(|| (names.channel)(id)).unwrap_or_else(|| id.to_string());
        push_text(&mut out, &format!("#{name}"), base | ST_CYAN);
    } else if let Some(cmd) = head.strip_prefix('!') {
        let text = match label {
            Some(l) => l.to_string(),
            None => match cmd.split('^').next().unwrap_or(cmd) {
                "here" => "@here".into(),
                "channel" => "@channel".into(),
                "everyone" => "@everyone".into(),
                other => format!("@{other}"),
            },
        };
        push_text(&mut out, &text, base | ST_BOLD);
    } else {
        // A link. The terminal's own link detection (Ghostty, iTerm2) is
        // what makes it clickable, and it only sees what is drawn: a
        // label that is the url shortened ("very.long/url/pa…", or the
        // url without its scheme) gives way to the whole url; a label
        // that is real text keeps it and the url follows, dim, in angle
        // brackets, so the full url is on screen either way.
        let url = unescape(head);
        match label.map(unescape).filter(|l| !l.is_empty() && !label_abbreviates(l, &url)) {
            None => push_url(&mut out, &url, base | ST_UNDER),
            Some(l) => {
                push_text(&mut out, &l, base | ST_UNDER);
                out.push((' ', base));
                out.push(('<', base | ST_DIM));
                push_url(&mut out, &url, base | ST_DIM | ST_UNDER);
                out.push(('>', base | ST_DIM));
            }
        }
    }
    out
}

/// A url's cells: literal (no shortcode in `/a:b:c/` becomes a glyph),
/// one style.
fn push_url(out: &mut Vec<Styled>, url: &str, style: u16) {
    out.extend(url.chars().filter(|c| !c.is_control()).map(|c| (c, style)));
}

/// The url without its scheme (`https://x.y/z` -> `x.y/z`, `mailto:a@b`
/// -> `a@b`) and without a trailing slash.
fn bare(url: &str) -> &str {
    let rest = match url.find("://") {
        Some(i) if url[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => &url[i + 3..],
        _ => url.strip_prefix("mailto:").unwrap_or(url),
    };
    rest.trim_end_matches('/')
}

/// Whether a link's label is the url itself, shortened the way Slack
/// shortens one: the url or the url without its scheme, possibly cut
/// with an ellipsis. Real text ("the docs", "read more...") is not.
pub fn label_abbreviates(label: &str, url: &str) -> bool {
    let label = label.trim();
    let whole = label.trim_end_matches('/');
    if whole == url.trim_end_matches('/') || whole == bare(url) {
        return true;
    }
    // "x.y/some/pa…" or "x.y/some/pa...": the text before the cut is a
    // prefix of the url (with or without the scheme); an empty prefix is
    // nothing but a cut and still the url's.
    let head = match (label.find('…'), label.find("...")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let Some(cut) = head else { return false };
    let prefix = label[..cut].trim_end();
    // The tail after a middle cut ("x.y/aaa…/zzz") must end the url.
    let tail = label[cut..].trim_start_matches(['…', '.']).trim();
    let tail_ok = tail.is_empty() || url.ends_with(tail);
    tail_ok && (url.starts_with(prefix) || bare(url).starts_with(prefix))
}

fn boundary_before(chars: &[char], i: usize) -> bool {
    i == 0 || chars[i - 1].is_whitespace() || "([{\"'“‘*_~".contains(chars[i - 1])
}

fn boundary_after(chars: &[char], j: usize) -> bool {
    j + 1 >= chars.len() || chars[j + 1].is_whitespace() || ".,;:!?)]}\"'”’*_~".contains(chars[j + 1])
}

/// The closing marker for the one opened at `i`, on this line.
fn closing(chars: &[char], i: usize, marker: char) -> Option<usize> {
    if i + 1 >= chars.len() || chars[i + 1].is_whitespace() {
        return None;
    }
    (i + 2..chars.len()).find(|&j| chars[j] == marker && !chars[j - 1].is_whitespace() && boundary_after(chars, j))
}

/// One line of mrkdwn (no fences) as cells.
pub fn inline(text: &str, base: u16, names: &Names) -> Vec<Styled> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    let mut plain_from = 0;
    let flush = |out: &mut Vec<Styled>, from: usize, to: usize| {
        if from < to {
            let s: String = chars[from..to].iter().collect();
            push_text(out, &s, base);
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '<' => {
                if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '>') {
                    flush(&mut out, plain_from, i);
                    let inner: String = chars[i + 1..end].iter().collect();
                    out.extend(token(&inner, base, names));
                    i = end + 1;
                    plain_from = i;
                    continue;
                }
            }
            '`' => {
                if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '`') {
                    if end > i + 1 {
                        flush(&mut out, plain_from, i);
                        let inner: String = chars[i + 1..end].iter().collect();
                        push_text(&mut out, &inner, base | ST_CODE);
                        i = end + 1;
                        plain_from = i;
                        continue;
                    }
                }
            }
            '*' | '_' | '~' if boundary_before(&chars, i) => {
                if let Some(end) = closing(&chars, i, c) {
                    flush(&mut out, plain_from, i);
                    let inner: String = chars[i + 1..end].iter().collect();
                    let bit = match c {
                        '*' => ST_BOLD,
                        '_' => ST_ITALIC,
                        _ => ST_DIM,
                    };
                    out.extend(inline(&inner, base | bit, names));
                    i = end + 1;
                    plain_from = i;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    flush(&mut out, plain_from, chars.len());
    out
}

/// A whole message body as wrapped lines of `width`.
pub fn render(text: &str, width: usize, base: u16, names: &Names) -> Vec<Vec<Styled>> {
    let width = width.max(4);
    let mut out: Vec<Vec<Styled>> = Vec::new();
    // Fenced code alternates with prose on ``` boundaries, wherever they
    // fall (Slack puts them mid-line as often as on their own).
    for (k, seg) in text.split("```").enumerate() {
        if k % 2 == 1 {
            let code = seg.strip_prefix('\n').unwrap_or(seg);
            let code = code.strip_suffix('\n').unwrap_or(code);
            for line in code.split('\n') {
                let mut cells = vec![(' ', base | ST_CODE)];
                push_text(&mut cells, line, base | ST_CODE);
                for w in hard_wrap(&cells, width) {
                    out.push(w);
                }
            }
            continue;
        }
        let seg = if k > 0 { seg.strip_prefix('\n').unwrap_or(seg) } else { seg };
        let seg = seg.strip_suffix('\n').unwrap_or(seg);
        if seg.is_empty() {
            continue;
        }
        for line in seg.split('\n') {
            let (quoted, body) = if let Some(rest) = line.strip_prefix("&gt;") {
                (true, rest.trim_start())
            } else if let Some(rest) = line.strip_prefix('>') {
                (true, rest.trim_start())
            } else {
                (false, line)
            };
            if body.trim().is_empty() {
                out.push(Vec::new());
                continue;
            }
            let cells = inline(body, if quoted { base | ST_DIM } else { base }, names);
            if quoted {
                for mut w in wrap_cells(&cells, width.saturating_sub(2).max(2)) {
                    let mut l = vec![('▎', base | ST_DIM), (' ', base)];
                    l.append(&mut w);
                    out.push(l);
                }
            } else {
                for w in wrap_cells(&cells, width) {
                    out.push(w);
                }
            }
        }
    }
    out
}

/// The text without markup, names resolved, one String per source
/// line: a message as it reads when copied. Fenced code keeps its lines
/// (the fence markers go), a quote keeps a `> ` prefix, entities are
/// undone.
pub fn plain_lines(text: &str, names: &Names) -> Vec<String> {
    let mut out = Vec::new();
    for (k, seg) in text.split("```").enumerate() {
        let seg = if k > 0 { seg.strip_prefix('\n').unwrap_or(seg) } else { seg };
        let seg = seg.strip_suffix('\n').unwrap_or(seg);
        if seg.is_empty() {
            continue;
        }
        for line in seg.split('\n') {
            if k % 2 == 1 {
                out.push(unescape(line).trim_end().to_string());
                continue;
            }
            let (quoted, body) = match line.strip_prefix("&gt;").or_else(|| line.strip_prefix('>')) {
                Some(rest) => (true, rest.trim_start()),
                None => (false, line),
            };
            let text = cells_text(&inline(body, 0, names));
            out.push(if quoted { format!("> {}", text.trim_end()) } else { text.trim_end().to_string() });
        }
    }
    out
}

/// The text without markup, one line, names resolved: for a
/// notification body or a preview row.
pub fn plain(text: &str, names: &Names) -> String {
    let flat = text.replace("```", " ").replace('\n', " ");
    let cells = inline(&flat, 0, names);
    cells_text(&cells).split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use listkit::styled::cells_text;

    fn names() -> Names<'static> {
        Names {
            user: &|id| if id == "U1" { Some("zack".into()) } else { None },
            channel: &|id| if id == "C1" { Some("general".into()) } else { None },
        }
    }

    #[test]
    fn tokens_resolve() {
        let c = inline("hey <@U1> see <#C1|general> and <#C2> at <https://x.y/z|the link> <!here>", 0, &names());
        assert_eq!(cells_text(&c), "hey @zack see #general and #C2 at the link <https://x.y/z> @here");
        assert!(c.iter().any(|x| x.1 & ST_UNDER != 0));
        assert!(c[4].1 & ST_BOLD != 0 && c[4].1 & ST_CYAN != 0);
    }

    #[test]
    fn links_show_the_full_url() {
        // Slack's shortened label (the url without its scheme, cut with an
        // ellipsis) gives way to the whole url, underlined.
        let long = "https://very.long.example/url/path/to/a/document";
        let c = inline(&format!("see <{long}|very.long.example/url/pa…> now"), 0, &names());
        assert_eq!(cells_text(&c), format!("see {long} now"));
        let under: String = c.iter().filter(|x| x.1 & ST_UNDER != 0).map(|x| x.0).collect();
        assert_eq!(under, long);
        // The three-dot spelling, a label that is the url minus the
        // scheme, and a label that is the url itself: all the url once.
        let c = inline(&format!("<{long}|very.long.example/url/pa...>"), 0, &names());
        assert_eq!(cells_text(&c), long);
        assert_eq!(cells_text(&inline("<https://x.y/z/|x.y/z>", 0, &names())), "https://x.y/z/");
        assert_eq!(cells_text(&inline("<https://x.y/z|https://x.y/z>", 0, &names())), "https://x.y/z");
        // A real label keeps its text and the url follows in brackets,
        // dim, one word for the wrapper.
        let c = inline("<https://x.y/z|the docs>", 0, &names());
        assert_eq!(cells_text(&c), "the docs <https://x.y/z>");
        assert!(c[0].1 & ST_UNDER != 0 && c[0].1 & ST_DIM == 0);
        let dim: String = c.iter().filter(|x| x.1 & ST_DIM != 0).map(|x| x.0).collect();
        assert_eq!(dim, "<https://x.y/z>");
        // "read more..." is text, not a cut url.
        assert_eq!(cells_text(&inline("<https://x.y/z|read more...>", 0, &names())), "read more... <https://x.y/z>");
        // A bare url is itself; entities in it are undone; no shortcode
        // in a url path becomes a glyph.
        assert_eq!(cells_text(&inline("<https://x.y/?a=1&amp;b=2>", 0, &names())), "https://x.y/?a=1&b=2");
        assert_eq!(cells_text(&inline("<https://x.y/a:+1:b|the docs>", 0, &names())), "the docs <https://x.y/a:+1:b>");
        // The copy transcript carries the same.
        assert_eq!(plain("go <https://x.y/z|x.y/z> or <https://x.y/w|here>", &names()), "go https://x.y/z or here <https://x.y/w>");
        assert_eq!(plain_lines("<https://x.y/z|x.y/…>", &names()), vec!["https://x.y/z"]);
        // The wrapper keeps a url whole when it fits on the next line and
        // only cuts one wider than the view.
        let lines = render("read <https://x.y/some/long/path|x.y/some/long/pa…> now", 20, 0, &names());
        let texts: Vec<String> = lines.iter().map(|l| cells_text(l)).collect();
        assert_eq!(texts, vec!["read", "https://x.y/some/lon", "g/path now"]);
        let lines = render("read <https://x.y/path|x.y/path> now", 20, 0, &names());
        let texts: Vec<String> = lines.iter().map(|l| cells_text(l)).collect();
        assert_eq!(texts, vec!["read", "https://x.y/path now"]);
        assert!(label_abbreviates("x.y/aaa…/zzz", "https://x.y/aaa/bbb/zzz"));
        assert!(!label_abbreviates("x.y/aaa…/qqq", "https://x.y/aaa/bbb/zzz"));
        assert!(label_abbreviates("…", "https://x.y"));
    }

    #[test]
    fn emphasis_needs_boundaries() {
        let c = inline("a *bold* b_c_d _it_ ~gone~ `x*y`", 0, &names());
        assert_eq!(cells_text(&c), "a bold b_c_d it gone x*y");
        let bold: String = c.iter().filter(|x| x.1 & ST_BOLD != 0).map(|x| x.0).collect();
        assert_eq!(bold, "bold");
        let italic: String = c.iter().filter(|x| x.1 & ST_ITALIC != 0).map(|x| x.0).collect();
        assert_eq!(italic, "it");
        let code: String = c.iter().filter(|x| x.1 & ST_CODE != 0).map(|x| x.0).collect();
        assert_eq!(code, "x*y");
        assert_eq!(cells_text(&inline("2 * 3 * 4", 0, &names())), "2 * 3 * 4");
    }

    #[test]
    fn entities_and_fences() {
        let lines = render("a &amp; b &lt;c&gt;\n```\nlet x = 1;\n```\n&gt; quoted", 40, 0, &names());
        let texts: Vec<String> = lines.iter().map(|l| cells_text(l)).collect();
        assert_eq!(texts[0], "a & b <c>");
        assert_eq!(texts[1].trim(), "let x = 1;");
        assert!(texts[2].starts_with("▎ quoted"));
        assert_eq!(plain("hi <@U1>\n*there*", &names()), "hi @zack there");
        assert_eq!(
            plain_lines("hi <@U1>\n&gt; *there*\n```\nlet x = 1 &amp;&amp; 2;\n```tail", &names()),
            vec!["hi @zack", "> there", "let x = 1 && 2;", "tail"]
        );
    }

    #[test]
    fn shortcodes_become_glyphs_outside_code() {
        let c = inline("ok :+1: at 12:30 `:+1:` :nope:", 0, &names());
        assert_eq!(cells_text(&c), "ok \u{1F44D} at 12:30 :+1: :nope:");
        // The glyph is one cell plus its filler; the code span is literal.
        assert_eq!(c.len(), "ok ** at 12:30 :+1: :nope:".len());
        assert_eq!(plain("*:raised_hands:* <@U1>", &names()), "\u{1F64C} @zack");
        let lines = render("```\n:+1:\n```", 20, 0, &names());
        assert_eq!(cells_text(&lines[0]).trim(), ":+1:");
    }
}
