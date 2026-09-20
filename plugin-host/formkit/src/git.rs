//! The filesystem and git actions a form ends in. Each is one or two
//! `run_job` calls and reports failure as the job's last output line, so
//! a form can show it where the user is looking.

use tmux_plugin_sdk::prelude::*;

use crate::text::{last_line, quote};

/// Resolve the git repo for a directory: the main working tree when the
/// directory sits inside a linked worktree (common dir minus "/.git"),
/// so worktrees don't nest inside worktrees.
pub async fn root(dir: &str) -> Option<String> {
    match run_job(
        "git rev-parse --show-toplevel --path-format=absolute --git-common-dir",
        Some(dir),
    )
    .await
    {
        Ok(out) if out.status == 0 => {
            let mut lines = out.output.lines();
            let toplevel = lines.next().unwrap_or("").trim().to_string();
            let common = lines.next().unwrap_or("").trim();
            let root = match common.strip_suffix("/.git") {
                Some(main) if !main.is_empty() => main.to_string(),
                _ => toplevel,
            };
            (!root.is_empty()).then_some(root)
        }
        _ => None,
    }
}

/// What [`ensure_dir`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Ensured {
    /// The directory exists (it did, or it was just created).
    Ready,
    /// The directory is missing and `confirmed` was false: ask, then call
    /// again with `confirmed` true.
    Missing,
}

/// Make sure a folder exists. A missing folder is created only when the
/// caller says the user confirmed it - creating directories on a typo is
/// the one thing a path form must not do.
pub async fn ensure_dir(dir: &str, confirmed: bool) -> Result<Ensured, String> {
    let exists = run_job(&format!("test -d {}", quote(dir)), None)
        .await
        .map(|o| o.status == 0)
        .unwrap_or(false);
    if exists {
        return Ok(Ensured::Ready);
    }
    if !confirmed {
        return Ok(Ensured::Missing);
    }
    match run_job(&format!("mkdir -p {}", quote(dir)), None).await {
        Ok(out) if out.status == 0 => Ok(Ensured::Ready),
        Ok(out) => Err(last_line(&out.output, "mkdir failed")),
        Err(e) => Err(format!("job failed: {}", e.message)),
    }
}

/// Whether `branch` exists in `repo`.
pub async fn branch_exists(repo: &str, branch: &str) -> bool {
    run_job(
        &format!(
            "git -C {} show-ref --verify --quiet {}",
            quote(repo),
            quote(&format!("refs/heads/{branch}"))
        ),
        None,
    )
    .await
    .map(|o| o.status == 0)
    .unwrap_or(false)
}

/// `git worktree add`: check the branch out at `dest` if it exists, else
/// create it there with `-b`.
pub async fn add_worktree(repo: &str, dest: &str, branch: &str) -> Result<(), String> {
    let add = if branch_exists(repo, branch).await {
        format!("git -C {} worktree add {} {}", quote(repo), quote(dest), quote(branch))
    } else {
        format!("git -C {} worktree add -b {} {}", quote(repo), quote(branch), quote(dest))
    };
    match run_job(&add, None).await {
        Ok(out) if out.status == 0 => Ok(()),
        Ok(out) => Err(last_line(&out.output, "git worktree add failed")),
        Err(e) => Err(format!("job failed: {}", e.message)),
    }
}
