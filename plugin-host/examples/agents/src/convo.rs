//! The conversation pane: a scratch pane that holds the rendered
//! conversation of the highlighted row, in copy mode, so the preview
//! gets tmux's own search, scrolling and selection instead of a
//! re-implementation of them.
//!
//! The picker's preview is a blit of a pane's visible screen (the host
//! blits `wp->screen`, so a pane in copy mode shows its copy-mode
//! screen: the scrollback position, the search highlights, the cursor).
//! A live agent's preview is its own pane. For a conversation there is
//! no pane, so this module makes one: a detached session with one
//! window, sized to the preview, whose pane holds a sleeping shell and is
//! fed the rendered conversation straight into its screen (`pane_feed`,
//! no pty: a pty would deliver it one read per event-loop turn). Then
//! `copy-mode` on it, `history-top`, and `search-forward` with the
//! query's terms as a regex - copy mode marks every match and `n`/`N`
//! step through them, as they do anywhere in tmux. Keys typed into the
//! preview go to that pane like keys into an agent's pane do, so every
//! copy-mode binding of the user's works there: scroll, select, yank, a
//! bound regex that picks out file paths.
//!
//! The session is named [`SESSION`], created on first use and killed
//! with the picker. Its pane is reused: showing another conversation is
//! a `respawn-pane -k`. The window keeps a large scrollback so a long
//! conversation is all there.

use tmux_plugin_sdk::prelude::*;

/// The scratch session. Shows in `list-sessions` while the picker is
/// open; gone when it closes.
pub const SESSION: &str = "_agents-preview";
/// Scrollback for the conversation window. A long session runs to
/// thousands of rendered lines; this is generous and cheap.
const HISTORY: u32 = 200_000;
/// Bytes fed into the pane per wake (see `show`). The parser runs at
/// tens of MB/s, so this is well under a millisecond.
const FEED_SLICE: usize = 64 * 1024;

/// The scratch pane, if the session exists: found by session name.
pub fn pane() -> Option<u32> {
    let sid = list_sessions().ok()?.into_iter().find(|s| s.name == SESSION)?.id;
    let windows = list_windows().ok()?;
    let win = windows.iter().find(|w| w.sessions.contains(&sid))?.id;
    list_panes().ok()?.into_iter().find(|p| p.window == win).map(|p| p.id)
}

/// Is this pane in the scratch session? Every pane there is the
/// picker's, whatever its environment says - never an agent.
pub fn owns(pane: u32) -> bool {
    let Some(sid) = list_sessions().ok().and_then(|v| v.into_iter().find(|s| s.name == SESSION).map(|s| s.id))
    else {
        return false;
    };
    let Ok(p) = resolve_pane(PaneId(pane)) else { return false };
    list_windows()
        .ok()
        .into_iter()
        .flatten()
        .any(|w| w.id == p.window && w.sessions.contains(&sid))
}

/// The scratch pane, created if need be, sized `w` x `h`.
pub async fn ensure(w: u32, h: u32) -> Option<u32> {
    let (w, h) = (w.max(10), h.max(3));
    if pane().is_none() {
        // The first window takes the global history limit; the session
        // option is set after it exists, and a fresh window takes it.
        let _ = run_command(format!(
            "new-session -d -s {SESSION} -x {w} -y {h} \"sh -c 'exec sleep 2147483647'\""
        ))
        .await;
        let _ = run_command(format!("set-option -t {SESSION} history-limit {HISTORY}")).await;
        let _ = run_command(format!(
            "new-window -d -t {SESSION} \"sh -c 'exec sleep 2147483647'\""
        ))
        .await;
        let _ = run_command(format!("kill-window -t {SESSION}:0")).await;
        // Nothing of the scratch session should reach the user's status
        // line.
        let _ = run_command(format!("set-option -t {SESSION} status off")).await;
    }
    let pane = pane()?;
    let _ = run_command(format!("resize-window -t {SESSION} -x {w} -y {h}")).await;
    Some(pane)
}

/// Show `lines` (rendered, ANSI) in the pane and enter copy mode on it,
/// searching for `regex` when there is one. Returns the pane.
///
/// The text goes into the pane's screen with `pane_feed`, not through
/// its pty: a pty delivers one read per event-loop turn, which on a
/// long conversation is seconds. The feed parses on the main thread, so
/// it goes in slices with an await between, each a bounded wake.
pub async fn show(lines: &[String], regex: Option<&str>, w: u32, h: u32) -> Option<u32> {
    let pane = ensure(w, h).await?;
    // A fresh screen: the respawn leaves any mode and clears the visible
    // grid; the scrollback is cleared separately (a respawn keeps it).
    let _ = run_command(format!("respawn-pane -k -t %{pane} \"sh -c 'exec sleep 2147483647'\"")).await;
    let _ = run_command(format!("clear-history -t %{pane}")).await;
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    let bytes = text.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let end = (pos + FEED_SLICE).min(bytes.len());
        // Cut at a line so an escape sequence is never split.
        let end = if end < bytes.len() {
            bytes[pos..end].iter().rposition(|&b| b == b'\n').map(|i| pos + i + 1).unwrap_or(end)
        } else {
            end
        };
        pane_feed(PaneId(pane), &bytes[pos..end]).ok()?;
        pos = end;
        if pos < bytes.len() && sleep_ms(1).await.is_err() {
            return None;
        }
    }
    let _ = run_command(format!("copy-mode -t %{pane}")).await;
    let _ = run_command(format!("send-keys -t %{pane} -X history-top")).await;
    if let Some(re) = regex.filter(|r| !r.is_empty()) {
        let _ = run_command(format!(
            "send-keys -t %{pane} -X search-forward {}",
            tmux_quote(re)
        ))
        .await;
    }
    Some(pane)
}

/// The query's terms as one copy-mode search regex, lowercase (copy
/// mode's search is case-insensitive for a lowercase pattern), with the
/// regex metacharacters in the terms escaped. Several terms are an
/// alternation in a group: copy mode takes a pattern for a regex only
/// when it holds one of `^$*+()?[].\` - `|` alone does not count, and
/// `a|b` would be searched literally.
pub fn search_regex(terms: &[String]) -> Option<String> {
    let mut parts: Vec<String> = terms
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| {
            let mut out = String::with_capacity(t.len() + 4);
            for c in t.to_lowercase().chars() {
                if r"\.[]{}()*+?|^$".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        })
        .collect();
    match parts.len() {
        0 => None,
        1 => Some(parts.remove(0)),
        _ => Some(format!("({})", parts.join("|"))),
    }
}

/// A tmux command argument: single-quoted, with any single quote in it
/// spelled out.
fn tmux_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Kill the scratch session, if it exists.
pub async fn teardown() {
    if pane().is_some() {
        let _ = run_command(format!("kill-session -t {SESSION}")).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_from_terms() {
        assert_eq!(search_regex(&[]), None);
        assert_eq!(search_regex(&["DFlash2".into()]).unwrap(), "dflash2");
        assert_eq!(search_regex(&["DFlash2".into(), "a.b".into()]).unwrap(), r"(dflash2|a\.b)");
        assert_eq!(tmux_quote("it's"), r"'it'\''s'");
    }
}
