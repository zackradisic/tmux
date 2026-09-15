//! Mailbox: cross-agent messages, plugin-native.
//!
//! An agent sends a message to a mailbox and another agent reads it. The
//! message is data in this plugin's store until the reader pulls it; it is
//! never typed into a pane, so it does not touch a half-typed prompt and
//! does not commit on Enter. A mailbox on a linked server is reached over
//! the plugin bridge, so no ssh and no keystrokes cross the link.
//!
//! Commands (bind or run through `plugin-command mailbox ...`):
//!
//!   send <box>[@server] <text...>   leave a message
//!   inbox <box> [-a]                stash this box's messages in the
//!                                   option @mailbox_<box> (JSON) and mark
//!                                   them read; -a keeps the read ones too
//!   list                            unread counts per box, on the status
//!
//! A `<box>` is any name; the agents plugin will use the durable agent id.
//! `@server` is a name from `service::servers()`; without it the local
//! server. The reader consumes its inbox from the shell:
//!
//!   tmux2 show-options -s -v @mailbox_<box>
//!
//! A stdout-returning command is the ergonomic follow-up; it needs the
//! host to let a plugin-command write to the caller, which it cannot yet.
//!
//! Services: `deliver` stores a message, `boxes` reports the message count
//! per box, so a view (the agents picker) can badge an agent with its
//! unread count. A mailbox plugin on any server
//! accepts a message from any other. The gate is the ordinary one:
//! `plugin-remote-caps` on the receiving server, the sidecar's services
//! allowlist, and (planned) an accept-from option checked against the
//! sender's server.

use tmux_plugin_sdk::prelude::*;

const NAME: &str = "mailbox";
const TABLE: &str = "CREATE TABLE IF NOT EXISTS messages (\
    id INTEGER PRIMARY KEY AUTOINCREMENT, \
    box TEXT NOT NULL, sender TEXT NOT NULL, body TEXT NOT NULL, \
    ts INTEGER NOT NULL, read INTEGER NOT NULL DEFAULT 0)";
/// Topic published on every delivery, so a view can track unread counts.
const TOPIC: &str = "mailbox";

#[derive(serde::Serialize, serde::Deserialize)]
struct DeliverReq {
    to: String,
    from: String,
    body: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BoxCount {
    #[serde(rename = "box")]
    box_: String,
    unread: i64,
    total: i64,
}

#[derive(serde::Serialize)]
struct Message {
    id: i64,
    sender: String,
    body: String,
    ts: i64,
    read: bool,
}

struct Mailbox {
    role: Role,
}

/// Store one message for `box` from `sender`. Returns the row id.
async fn store(box_: &str, sender: &str, body: &str) -> Result<i64, HostError> {
    let ts = now_ms() as i64;
    let r = db_exec(
        "INSERT INTO messages (box, sender, body, ts) VALUES (?1, ?2, ?3, ?4)",
        &[box_.into(), sender.into(), body.into(), ts.into()],
    )
    .await?;
    let _ = service::emit_json(TOPIC, &box_);
    Ok(r.last_insert_rowid)
}

/// `box` or `box@server` -> (box, server); empty server means local.
fn split_target(addr: &str) -> (String, String) {
    match addr.split_once('@') {
        Some((b, s)) => (b.to_string(), s.to_string()),
        None => (addr.to_string(), String::new()),
    }
}

async fn cmd_send(from: &str, rest: &str) {
    let rest = rest.trim();
    let Some((addr, body)) = rest.split_once(char::is_whitespace) else {
        let _ = display_message("mailbox: send <box>[@server] <text>");
        return;
    };
    let body = body.trim();
    if body.is_empty() {
        let _ = display_message("mailbox: an empty message");
        return;
    }
    let (box_, server) = split_target(addr);
    if server.is_empty() || server == "local" {
        match store(&box_, from, body).await {
            Ok(_) => {
                let _ = display_message(&format!("mailbox: left for {box_}"));
            }
            Err(e) => {
                let _ = display_message(&format!("mailbox: {}", e.message));
            }
        }
        return;
    }
    // A mailbox on another server: hand it to that server's deliver.
    let req = DeliverReq { to: box_.clone(), from: from.to_string(), body: body.to_string() };
    let target = format!("{NAME}@{server}");
    match service::call_json::<_, serde_json::Value>(&target, "deliver", &req).await {
        Ok(_) => {
            let _ = display_message(&format!("mailbox: sent to {box_}@{server}"));
        }
        Err(e) => {
            let _ = display_message(&format!("mailbox: {server}: {}", e.message));
        }
    }
}

async fn cmd_inbox(rest: &str) {
    let mut parts = rest.split_whitespace();
    let Some(box_) = parts.next() else {
        let _ = display_message("mailbox: inbox <box> [-a]");
        return;
    };
    let all = parts.any(|p| p == "-a" || p == "--all");
    let sql = if all {
        "SELECT id, sender, body, ts, read FROM messages WHERE box = ?1 ORDER BY id"
    } else {
        "SELECT id, sender, body, ts, read FROM messages WHERE box = ?1 AND read = 0 ORDER BY id"
    };
    let rows = match db_query(sql, &[box_.into()]).await {
        Ok(r) => r,
        Err(e) => {
            let _ = display_message(&format!("mailbox: {}", e.message));
            return;
        }
    };
    let msgs: Vec<Message> = rows
        .iter()
        .map(|row| Message {
            id: row.get_named("id").and_then(DbValue::as_i64).unwrap_or(0),
            sender: row
                .get_named("sender")
                .and_then(DbValue::as_str)
                .unwrap_or("")
                .to_string(),
            body: row
                .get_named("body")
                .and_then(DbValue::as_str)
                .unwrap_or("")
                .to_string(),
            ts: row.get_named("ts").and_then(DbValue::as_i64).unwrap_or(0),
            read: row.get_named("read").and_then(DbValue::as_i64).unwrap_or(0) != 0,
        })
        .collect();
    let json = serde_json::to_string(&msgs).unwrap_or_else(|_| "[]".into());
    let _ = set_option(&format!("@mailbox_{box_}"), &json);
    // Mark the unread ones read now that they are in the option.
    let _ = db_exec("UPDATE messages SET read = 1 WHERE box = ?1 AND read = 0", &[box_.into()])
        .await;
    let _ = display_message(&format!(
        "mailbox: {} message{} in @mailbox_{box_}",
        msgs.len(),
        if msgs.len() == 1 { "" } else { "s" }
    ));
}

async fn cmd_list() {
    let rows = match db_query(
        "SELECT box, COUNT(*) AS n FROM messages WHERE read = 0 GROUP BY box ORDER BY box",
        &[],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = display_message(&format!("mailbox: {}", e.message));
            return;
        }
    };
    let mut parts = Vec::new();
    for row in rows.iter() {
        let b = row.get_named("box").and_then(DbValue::as_str).unwrap_or("");
        let n = row.get_named("n").and_then(DbValue::as_i64).unwrap_or(0);
        parts.push(format!("{b}:{n}"));
    }
    let msg = if parts.is_empty() {
        "mailbox: no unread".to_string()
    } else {
        format!("mailbox: {}", parts.join(" "))
    };
    let _ = display_message(&msg);
}

impl Plugin for Mailbox {
    const NAME: &'static str = NAME;
    type Config = serde_json::Value;

    fn init(ctx: &Ctx, _config: Self::Config) -> Result<Self, String> {
        let role = ctx.role();
        if role.provides() {
            db_exec_sync(TABLE, &[]).map_err(|e| e.message)?;
            service::register("deliver").map_err(|e| e.message.clone())?;
            service::register("boxes").map_err(|e| e.message.clone())?;
        }
        // plugin-command is addressed by name and needs a subscription.
        ctx.subscribe(&["plugin-command"]).map_err(|e| e.message.clone())?;
        Ok(Mailbox { role })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        if event.name() != "plugin-command" {
            return;
        }
        if !self.role.provides() {
            let _ = display_message("mailbox: this instance is a view");
            return;
        }
        let text = event.get_str("text").unwrap_or("").trim().to_string();
        let from = event
            .scope
            .pane
            .map(|p| format!("%{p}"))
            .unwrap_or_else(|| "unknown".to_string());
        let (verb, rest) = match text.split_once(char::is_whitespace) {
            Some((v, r)) => (v.to_string(), r.to_string()),
            None => (text.clone(), String::new()),
        };
        match verb.as_str() {
            "send" => {
                ctx.spawn(async move { cmd_send(&from, &rest).await });
            }
            "inbox" => {
                ctx.spawn(async move { cmd_inbox(&rest).await });
            }
            "list" => {
                ctx.spawn(async { cmd_list().await });
            }
            "" => {
                let _ = display_message("mailbox: send | inbox | list");
            }
            other => {
                let _ = display_message(&format!("mailbox: no verb {other:?}"));
            }
        }
    }

    fn on_service_request(&mut self, ctx: &Ctx, req: ServiceRequest) {
        if req.method == "boxes" {
            ctx.spawn(async move {
                match db_query(
                    "SELECT box, SUM(read = 0) AS unread, COUNT(*) AS total \
                     FROM messages GROUP BY box ORDER BY box",
                    &[],
                )
                .await
                {
                    Ok(rows) => {
                        let list: Vec<BoxCount> = rows
                            .iter()
                            .map(|row| BoxCount {
                                box_: row
                                    .get_named("box")
                                    .and_then(DbValue::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                unread: row
                                    .get_named("unread")
                                    .and_then(DbValue::as_i64)
                                    .unwrap_or(0),
                                total: row
                                    .get_named("total")
                                    .and_then(DbValue::as_i64)
                                    .unwrap_or(0),
                            })
                            .collect();
                        let _ = req.reply_json(&list);
                    }
                    Err(e) => {
                        let _ = req.fail(&e.message);
                    }
                }
            });
            return;
        }
        if req.method != "deliver" {
            let _ = req.fail("unknown method");
            return;
        }
        // The sender's server qualifies the sender name, so a reader can
        // see which machine a message came from.
        let server = req.server.clone();
        ctx.spawn(async move {
            let dr: DeliverReq = match req.json() {
                Ok(d) => d,
                Err(e) => {
                    let _ = req.fail(&e.message);
                    return;
                }
            };
            let sender = if server == "local" {
                dr.from
            } else {
                format!("{}@{server}", dr.from)
            };
            match store(&dr.to, &sender, &dr.body).await {
                Ok(id) => {
                    let _ = req.reply_json(&serde_json::json!({ "id": id }));
                }
                Err(e) => {
                    let _ = req.fail(&e.message);
                }
            }
        });
    }
}

tmux_plugin!(Mailbox);
