//! A rebindable key table with help text. Each action has a name, a
//! default key, the section of the `?` card it belongs in, and a line of
//! help; the consumer's config rebinds by action name. The quick
//! reference and the action menu are generated from it, so a key is
//! written down once.

use crate::text::keyname;

#[derive(Clone, Debug)]
pub struct Binding {
    /// The action's name, as the consumer matches on it (`"jump"`) and
    /// as the config rebinds it (`pick_jump`).
    pub action: &'static str,
    /// The key as bound (tmux key name: `Enter`, `C-f`, `Escape`).
    pub key: String,
    pub default: &'static str,
    /// The `?` card section this belongs to.
    pub section: &'static str,
    pub help: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct KeyTable {
    bindings: Vec<Binding>,
    /// Lines of the `?` card that are not bindings: a fixed key (one
    /// the engine owns, like `j/k`) and what it does, by section.
    notes: Vec<(&'static str, &'static str, &'static str)>,
}

impl KeyTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare an action with its default key.
    pub fn with(mut self, action: &'static str, default: &'static str, section: &'static str, help: &'static str) -> Self {
        self.bindings.push(Binding { action, key: default.to_string(), default, section, help });
        self
    }

    /// A `?` line for a key the consumer does not rebind.
    pub fn note(mut self, section: &'static str, key: &'static str, help: &'static str) -> Self {
        self.notes.push((section, key, help));
        self
    }

    /// Rebind an action; `None` or an empty key puts the default back.
    pub fn bind(&mut self, action: &str, key: Option<&str>) {
        if let Some(b) = self.bindings.iter_mut().find(|b| b.action == action) {
            let k = key.map(str::trim).filter(|k| !k.is_empty());
            b.key = k.unwrap_or(b.default).to_string();
        }
    }

    pub fn key_of(&self, action: &str) -> &str {
        self.bindings.iter().find(|b| b.action == action).map(|b| b.key.as_str()).unwrap_or("")
    }

    /// The action a pressed key is bound to.
    pub fn action_of(&self, key: &str) -> Option<&Binding> {
        self.bindings.iter().find(|b| b.key == key)
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// The `?` card: `(key, what)` rows grouped under section titles,
    /// which are marked with a leading `\0` so the renderer can draw
    /// them differently. Sections come in the order first declared.
    pub fn help(&self) -> Vec<(String, String)> {
        let mut sections: Vec<&'static str> = Vec::new();
        for b in &self.bindings {
            if !sections.contains(&b.section) {
                sections.push(b.section);
            }
        }
        for (s, _, _) in &self.notes {
            if !sections.contains(s) {
                sections.push(s);
            }
        }
        let mut out: Vec<(String, String)> = Vec::new();
        for s in sections {
            if !out.is_empty() {
                out.push((String::new(), String::new()));
            }
            out.push((format!("\0{s}"), String::new()));
            for b in self.bindings.iter().filter(|b| b.section == s) {
                out.push((keyname(&b.key).to_string(), b.help.to_string()));
            }
            for (_, k, h) in self.notes.iter().filter(|(sec, _, _)| *sec == s) {
                out.push((k.to_string(), h.to_string()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> KeyTable {
        KeyTable::new()
            .with("jump", "Enter", "moving", "jump to the pane")
            .with("close", "Escape", "picker", "close")
            .note("moving", "j/k", "move the cursor")
    }

    #[test]
    fn bind_and_lookup() {
        let mut t = table();
        assert_eq!(t.key_of("jump"), "Enter");
        t.bind("jump", Some(" o "));
        assert_eq!(t.key_of("jump"), "o");
        assert_eq!(t.action_of("o").map(|b| b.action), Some("jump"));
        t.bind("jump", Some(""));
        assert_eq!(t.key_of("jump"), "Enter");
        assert_eq!(t.key_of("nope"), "");
    }

    #[test]
    fn help_is_grouped() {
        let h = table().help();
        assert_eq!(h[0], ("\0moving".to_string(), String::new()));
        assert_eq!(h[1], ("Enter".to_string(), "jump to the pane".to_string()));
        assert_eq!(h[2], ("j/k".to_string(), "move the cursor".to_string()));
        assert_eq!(h[3], (String::new(), String::new()));
        assert_eq!(h[4], ("\0picker".to_string(), String::new()));
        assert_eq!(h[5].0, "Esc");
    }
}
