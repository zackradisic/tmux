//! formkit: the form and completion machinery a plugin panel is made of,
//! lifted out of the session creator so other plugins build on it rather
//! than copy it.
//!
//! Three layers, each usable without the others:
//!
//! - [`complete`]: a completion list for a text field. Directories,
//!   git repos, a repo's worktrees and branches, or a fixed word list;
//!   ranked, filtered by the typed fragment, with a cheap first scan and
//!   an expensive second probe that runs only for the rows on screen.
//! - [`form`]: a float with labelled text fields, one focused, each
//!   optionally completing; the key handling (`Tab` completes, `C-j`
//!   steps into the list, `Esc` hides it then closes), the touched /
//!   mirror rule between fields, and the resize handshake with the host.
//!   The meaning of the fields comes from the caller through [`form::Model`].
//! - [`git`]: the filesystem and git actions a form ends in - resolve a
//!   repo root, make sure a folder exists (asking first), add a worktree.
//!
//! This is a compile-time library: a plugin that uses it runs the scans
//! itself, so it needs the capabilities in [`complete::CAPS`] granted to
//! its own binary. A denied capability is rendered as a hint in the list,
//! never as a silent empty list.

pub mod complete;
pub mod form;
pub mod git;
pub mod text;
