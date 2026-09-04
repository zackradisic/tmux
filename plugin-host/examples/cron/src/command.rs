//! The `plugin-command cron '<verb ...>'` grammar and the replies.

use std::rc::Rc;

use tmux_plugin_sdk::prelude::*;

use crate::scheduler::{self, Ctl};
use crate::schedule;
use crate::store::{self, Catchup, Kind, NewJob};
use crate::{picker, runner};

pub const VERBS: &str = "add|rm|ls|run|enable|disable|last|status|pick";

#[derive(Debug)]
pub struct Add {
    pub name: Option<String>,
    pub catchup: Catchup,
    pub cwd: Option<String>,
    pub schedule: String,
    pub kind: Kind,
    pub command: String,
}

#[derive(Debug)]
pub enum Cmd {
    Add(Add),
    Rm(String),
    Ls,
    Run(String),
    Enable(String),
    Disable(String),
    Last(String),
    Status,
    Pick,
}

/// Split the line at a whitespace-delimited `--`: (head tokens, the
/// verbatim action text after it).
fn split_action(line: &str) -> (Vec<&str>, Option<&str>) {
    let mut head = Vec::new();
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return (head, None);
        }
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let tok = &trimmed[..end];
        let after = &trimmed[end..];
        if tok == "--" {
            return (head, Some(after.trim_start()));
        }
        head.push(tok);
        rest = after;
    }
}

pub fn parse(line: &str) -> Result<Cmd, String> {
    let (head, action) = split_action(line);
    let Some(&verb) = head.first() else {
        return Err(format!("no verb ({VERBS})"));
    };
    let one_arg = |what: &str| -> Result<String, String> {
        match head.get(1) {
            Some(a) if head.len() == 2 => Ok((*a).to_string()),
            _ => Err(format!("{verb} takes one argument: {what}")),
        }
    };
    match verb {
        "add" => parse_add(&head[1..], action),
        "rm" => one_arg("<id|name>").map(Cmd::Rm),
        "run" => one_arg("<id|name>").map(Cmd::Run),
        "enable" => one_arg("<id|name>").map(Cmd::Enable),
        "disable" => one_arg("<id|name>").map(Cmd::Disable),
        "last" => one_arg("<id|name>").map(Cmd::Last),
        "ls" => Ok(Cmd::Ls),
        "status" => Ok(Cmd::Status),
        "pick" => Ok(Cmd::Pick),
        other => Err(format!("unknown verb {other:?} ({VERBS})")),
    }
}

fn parse_add(args: &[&str], action: Option<&str>) -> Result<Cmd, String> {
    let mut name = None;
    let mut catchup = Catchup::Once;
    let mut cwd = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-n" | "--name" => {
                name = Some(args.get(i + 1).ok_or("-n needs a name")?.to_string());
                i += 2;
            }
            "--catchup" => {
                let v = args.get(i + 1).ok_or("--catchup needs skip|once|each")?;
                catchup = Catchup::parse(v)
                    .ok_or_else(|| format!("bad --catchup {v:?} (skip|once|each)"))?;
                i += 2;
            }
            "--cwd" => {
                cwd = Some(args.get(i + 1).ok_or("--cwd needs a directory")?.to_string());
                i += 2;
            }
            _ => break,
        }
    }
    let sched_tokens = &args[i..];
    let schedule = match sched_tokens.first() {
        Some(&"every") => {
            if sched_tokens.len() != 2 {
                return Err("every needs one interval, e.g. `every 15m`".into());
            }
            schedule::parse_every(sched_tokens[1])?;
            format!("every {}", sched_tokens[1])
        }
        Some(_) if sched_tokens.len() == 5 => {
            schedule::parse(&sched_tokens.join(" "))?;
            sched_tokens.join(" ")
        }
        Some(_) => {
            return Err(format!(
                "schedule must be `every <interval>` or 5 cron fields, got {} token(s)",
                sched_tokens.len()
            ))
        }
        None => return Err("add needs a schedule before --".into()),
    };
    let Some(action) = action else {
        return Err("add needs `-- shell <command>` or `-- tmux <command>`".into());
    };
    let (kind, command) = match action.split_once(char::is_whitespace) {
        Some((k, rest)) => (k, rest.trim_start()),
        None => (action, ""),
    };
    let kind = Kind::parse(kind)
        .ok_or_else(|| format!("action must start with shell or tmux, got {kind:?}"))?;
    if command.is_empty() {
        return Err("the action has no command".into());
    }
    if let Some(n) = &name {
        if n.parse::<i64>().is_ok() || n.starts_with('#') {
            return Err("a job name cannot look like an id".into());
        }
    }
    Ok(Cmd::Add(Add {
        name,
        catchup,
        cwd,
        schedule,
        kind,
        command: command.to_string(),
    }))
}

fn reply(client: Option<ClientId>, msg: &str) {
    let msg = format!("cron: {msg}");
    let sent = match client {
        Some(c) => display_message_to(c, msg.as_str()).is_ok(),
        None => false,
    };
    if !sent {
        let _ = display_message(msg.as_str());
    }
}

/// Run one command line, replying to `client`. `pick` is handled by the
/// caller (it needs the plugin struct); everything else lands here.
pub async fn run(sh: Ctl, cmd: Cmd, client: Option<ClientId>) {
    let cmd = match cmd {
        Cmd::Enable(ident) => return set_enabled(sh, ident, true, client).await,
        Cmd::Disable(ident) => return set_enabled(sh, ident, false, client).await,
        other => other,
    };
    match run_inner(&sh, cmd).await {
        Ok(lines) => {
            for l in lines {
                reply(client, &l);
            }
        }
        Err(e) => reply(client, &e),
    }
}

fn err_str(e: HostError) -> String {
    format!("db error: {}", e.message)
}

async fn run_inner(sh: &Ctl, cmd: Cmd) -> Result<Vec<String>, String> {
    let now = now_ms() as i64;
    let off = sh.tz_offset_min.get();
    match cmd {
        Cmd::Add(a) => {
            let sched = schedule::parse(&a.schedule)?;
            let next = schedule::next_after(&sched, now, now, off)
                .ok_or_else(|| format!("schedule {:?} never fires", a.schedule))?;
            if let Some(n) = &a.name {
                if store::find_job(n).await.map_err(err_str)?.is_some() {
                    return Err(format!("a job named {n:?} already exists"));
                }
            }
            let id = store::insert_job(&NewJob {
                name: a.name.clone(),
                schedule: a.schedule.clone(),
                kind: a.kind,
                command: a.command.clone(),
                cwd: a.cwd,
                catchup: a.catchup,
                next_run_ms: next,
                tz_offset_min: off,
                now_ms: now,
            })
            .await
            .map_err(err_str)?;
            scheduler::kick(sh);
            picker::refresh(sh);
            let label = match &a.name {
                Some(n) => format!("#{id} \"{n}\""),
                None => format!("#{id}"),
            };
            Ok(vec![format!(
                "added {label} ({}) {} {}; next {}",
                sched,
                a.kind.as_str(),
                a.command,
                schedule::describe_next(next, now)
            )])
        }
        Cmd::Rm(ident) => {
            let job = find(&ident).await?;
            let dropped = store::delete_job(job.id).await.map_err(err_str)?;
            sh.running.borrow_mut().remove(&job.id);
            scheduler::kick(sh);
            picker::refresh(sh);
            Ok(vec![format!("removed {} ({dropped} runs dropped)", job.label())])
        }
        Cmd::Ls => {
            let rows = store::list_jobs().await.map_err(err_str)?;
            let mut out = Vec::new();
            let enabled = rows.iter().filter(|r| r.job.enabled).count();
            for r in &rows {
                out.push(format!(
                    "{} {} ({}) {} {}; {}; last {}",
                    if r.job.enabled { "●" } else { "○" },
                    r.job.label(),
                    r.job.schedule,
                    r.job.kind.as_str(),
                    r.job.command,
                    next_text(r, now),
                    last_text(r, now),
                ));
            }
            out.push(format!("{} jobs ({enabled} enabled)", rows.len()));
            Ok(out)
        }
        Cmd::Run(ident) => {
            let job = find(&ident).await?;
            if sh.running.borrow().contains(&job.id) {
                return Ok(vec![format!("{} already running", job.label())]);
            }
            let run_id = store::insert_run(job.id, "manual", now, now).await.map_err(err_str)?;
            sh.running.borrow_mut().insert(job.id);
            let label = job.label();
            tmux_plugin_sdk::executor::spawn(runner::execute(Rc::clone(sh), job, run_id, 1));
            picker::refresh(sh);
            Ok(vec![format!("running {label} now")])
        }
        Cmd::Enable(_) | Cmd::Disable(_) | Cmd::Pick => Ok(Vec::new()),
        Cmd::Last(ident) => {
            let job = find(&ident).await?;
            match store::last_run(job.id).await.map_err(err_str)? {
                None => Ok(vec![format!("{} has not run yet", job.label())]),
                Some(r) => {
                    let when = r.finished_ms.map(|t| schedule::describe_ago(t, now));
                    let first = r
                        .output
                        .as_deref()
                        .and_then(|o| o.lines().find(|l| !l.trim().is_empty()))
                        .unwrap_or("");
                    let detail = match (&r.error, r.exit_code) {
                        (Some(e), _) => e.clone(),
                        (None, Some(c)) if r.signalled => format!("signal {c}"),
                        (None, Some(c)) => format!("exit {c}"),
                        (None, None) => String::new(),
                    };
                    Ok(vec![format!(
                        "{} last run {} {} in {}, {}: {}",
                        job.label(),
                        r.state,
                        detail,
                        schedule::fmt_duration(r.duration_ms.unwrap_or(0).max(0) as u64),
                        when.unwrap_or_default(),
                        first
                    )])
                }
            }
        }
        Cmd::Status => {
            let s = store::status_counts().await.map_err(err_str)?;
            let sign = if off < 0 { '-' } else { '+' };
            Ok(vec![format!(
                "{} jobs ({} enabled), {} running, {} retries pending, {} runs kept, tz {sign}{:02}:{:02}",
                s.jobs,
                s.enabled,
                s.running,
                s.retries_pending,
                s.runs_kept,
                off.abs() / 60,
                off.abs() % 60
            )])
        }
    }
}

/// `enable` / `disable`, separated because both need the schedule.
pub async fn set_enabled(sh: Ctl, ident: String, enabled: bool, client: Option<ClientId>) {
    let now = now_ms() as i64;
    let off = sh.tz_offset_min.get();
    let r: Result<String, String> = async {
        let job = find(&ident).await?;
        let next = if enabled {
            let sched = schedule::parse(&job.schedule)?;
            schedule::next_after(&sched, now, now, off)
        } else {
            None
        };
        store::set_enabled(job.id, enabled, next, now).await.map_err(err_str)?;
        scheduler::kick(&sh);
        picker::refresh(&sh);
        Ok(match next {
            Some(n) => format!("{} enabled, next {}", job.label(), schedule::describe_next(n, now)),
            None => format!("{} disabled", job.label()),
        })
    }
    .await;
    match r {
        Ok(m) => reply(client, &m),
        Err(e) => reply(client, &e),
    }
}

async fn find(ident: &str) -> Result<store::Job, String> {
    store::find_job(ident)
        .await
        .map_err(err_str)?
        .ok_or_else(|| format!("no job {ident:?}"))
}

pub fn next_text(r: &store::JobListing, now: i64) -> String {
    if r.running {
        "running".into()
    } else if !r.job.enabled {
        "disabled".into()
    } else {
        match r.job.next_run_ms {
            Some(t) => format!("next {}", schedule::describe_next(t, now)),
            None => "next never".into(),
        }
    }
}

pub fn last_text(r: &store::JobListing, now: i64) -> String {
    match (&r.last_state, r.last_finished_ms) {
        (Some(s), Some(t)) => {
            let exit = match r.last_exit {
                Some(c) if s == "failed" => format!(" exit {c}"),
                _ => String::new(),
            };
            format!("{s}{exit} {}", schedule::describe_ago(t, now))
        }
        _ => "—".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_grammar() {
        let Cmd::Add(a) = parse("add -n nightly --catchup skip 0 2 * * * -- shell ~/bin/backup --full")
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(a.name.as_deref(), Some("nightly"));
        assert_eq!(a.catchup, Catchup::Skip);
        assert_eq!(a.schedule, "0 2 * * *");
        assert_eq!(a.kind, Kind::Shell);
        assert_eq!(a.command, "~/bin/backup --full");

        let Cmd::Add(a) = parse("add every 2s -- tmux plugin-command resurrect save").unwrap()
        else {
            panic!()
        };
        assert_eq!(a.schedule, "every 2s");
        assert_eq!(a.kind, Kind::Tmux);
        assert_eq!(a.command, "plugin-command resurrect save");
        assert_eq!(a.catchup, Catchup::Once);

        assert!(parse("add every 2s").unwrap_err().contains("--"));
        assert!(parse("add every 2s -- python x").unwrap_err().contains("shell or tmux"));
        assert!(parse("add every 500ms -- shell x").unwrap_err().contains("floor"));
        assert!(parse("add 60 * * * * -- shell x").unwrap_err().contains("0-59"));
        assert!(parse("add * * * -- shell x").unwrap_err().contains("5 cron fields"));
        assert!(parse("add -n 12 every 2s -- shell x").unwrap_err().contains("id"));
        assert!(parse("add every 2s -- shell").unwrap_err().contains("no command"));
    }

    #[test]
    fn other_verbs() {
        assert!(matches!(parse("ls").unwrap(), Cmd::Ls));
        assert!(matches!(parse("status").unwrap(), Cmd::Status));
        assert!(matches!(parse("pick").unwrap(), Cmd::Pick));
        assert!(matches!(parse("rm 3").unwrap(), Cmd::Rm(s) if s == "3"));
        assert!(matches!(parse("run nightly").unwrap(), Cmd::Run(s) if s == "nightly"));
        assert!(parse("rm").unwrap_err().contains("one argument"));
        assert!(parse("frob").unwrap_err().contains("unknown verb"));
        assert!(parse("").unwrap_err().contains("no verb"));
    }

    #[test]
    fn action_text_is_verbatim() {
        let (head, action) = split_action("add every 2s --  echo  'a  b' -- c");
        assert_eq!(head, vec!["add", "every", "2s"]);
        assert_eq!(action, Some("echo  'a  b' -- c"));
    }
}
