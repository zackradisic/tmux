/* $OpenBSD$ */

/*
 * Copyright (c) 2026 Zack Radisic
 *
 * Permission to use, copy, modify, and distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WHATSOEVER INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS.
 * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT,
 * OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF
 * MIND, USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE
 * OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
 * PERFORMANCE OF THIS SOFTWARE.
 */

#include <sys/types.h>

#include <stdlib.h>
#include <string.h>

#include "tmux.h"
#include "plugin-host.h"
#include "plugin-internal.h"

/*
 * Plugin management commands. These only query or mutate the plugin
 * registry; they never run guest code inline (that happens at drain time).
 */

static enum cmd_retval	cmd_show_plugins_exec(struct cmd *,
			    struct cmdq_item *);
static enum cmd_retval	cmd_load_plugin_exec(struct cmd *,
			    struct cmdq_item *);
static enum cmd_retval	cmd_unload_plugin_exec(struct cmd *,
			    struct cmdq_item *);

const struct cmd_entry cmd_show_plugins_entry = {
	.name = "show-plugins",
	.alias = NULL,

	.args = { "v", 0, 0, NULL },
	.usage = "[-v]",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_show_plugins_exec
};

const struct cmd_entry cmd_load_plugin_entry = {
	.name = "load-plugin",
	.alias = NULL,

	.args = { "n:s:c:o:", 1, 1, NULL },
	.usage = "[-n name] [-s scope] [-c capability] [-o key=value] path",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_load_plugin_exec
};

const struct cmd_entry cmd_unload_plugin_entry = {
	.name = "unload-plugin",
	.alias = NULL,

	.args = { "", 1, 1, NULL },
	.usage = "name",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_unload_plugin_exec
};

static enum cmd_retval	cmd_sync_plugins_exec(struct cmd *,
			    struct cmdq_item *);
static enum cmd_retval	cmd_plugin_command_exec(struct cmd *,
			    struct cmdq_item *);

const struct cmd_entry cmd_plugin_command_entry = {
	.name = "plugin-command",
	.alias = NULL,

	.args = { "t:", 2, 2, NULL },
	.usage = CMD_TARGET_PANE_USAGE " plugin command",

	.target = { 't', CMD_FIND_PANE, 0 },

	.flags = CMD_AFTERHOOK,
	.exec = cmd_plugin_command_exec
};

const struct cmd_entry cmd_sync_plugins_entry = {
	.name = "sync-plugins",
	.alias = NULL,

	.args = { "", 1, 1, NULL },
	.usage = "manifest-path",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_sync_plugins_exec
};

static enum cmd_retval	cmd_reload_plugin_exec(struct cmd *,
			    struct cmdq_item *);
static enum cmd_retval	cmd_enable_plugin_exec(struct cmd *,
			    struct cmdq_item *);
static enum cmd_retval	cmd_plugin_log_exec(struct cmd *,
			    struct cmdq_item *);

const struct cmd_entry cmd_reload_plugin_entry = {
	.name = "reload-plugin",
	.alias = NULL,

	.args = { "a", 0, 1, NULL },
	.usage = "[-a] [name]",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_reload_plugin_exec
};

const struct cmd_entry cmd_enable_plugin_entry = {
	.name = "enable-plugin",
	.alias = NULL,

	.args = { "", 1, 1, NULL },
	.usage = "name",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_enable_plugin_exec
};

const struct cmd_entry cmd_disable_plugin_entry = {
	.name = "disable-plugin",
	.alias = NULL,

	.args = { "", 1, 1, NULL },
	.usage = "name",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_enable_plugin_exec
};

const struct cmd_entry cmd_plugin_log_entry = {
	.name = "plugin-log",
	.alias = NULL,

	.args = { "n:", 0, 1, NULL },
	.usage = "[-n lines] [name]",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_plugin_log_exec
};

/*
 * Sink receiving preformatted text from the plugin host; buffers it so it
 * can be printed line by line through cmdq_print.
 */
static void
cmd_plugin_sink(void *ctx, const char *ptr, size_t len)
{
	struct evbuffer	*evb = ctx;

	evbuffer_add(evb, ptr, len);
}

/* Print buffered sink output to the command queue, one line at a time. */
static void
cmd_plugin_print(struct cmdq_item *item, struct evbuffer *evb)
{
	char	*line;

	while ((line = evbuffer_readline(evb)) != NULL) {
		cmdq_print(item, "%s", line);
		free(line);
	}
	if (EVBUFFER_LENGTH(evb) > 0) {
		/* Trailing text without a newline. */
		evbuffer_add(evb, "", 1);
		cmdq_print(item, "%s", (const char *)EVBUFFER_DATA(evb));
		evbuffer_drain(evb, EVBUFFER_LENGTH(evb));
	}
}

static enum cmd_retval
cmd_show_plugins_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args	*args = cmd_get_args(self);
	struct evbuffer	*evb;

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}

	evb = evbuffer_new();
	if (evb == NULL)
		fatalx("out of memory");
	pgh_query_plugins(args_has(args, 'v'), cmd_plugin_sink, evb);
	cmd_plugin_print(item, evb);
	evbuffer_free(evb);
	return (CMD_RETURN_NORMAL);
}

static enum cmd_retval
cmd_load_plugin_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args		*args = cmd_get_args(self);
	struct args_value	*av;
	struct evbuffer		*evb;
	const char		*path = args_string(args, 0);
	const char		*name = args_get(args, 'n');
	const char		*scope = args_get(args, 's');
	const char		**caps = NULL, **opts = NULL;
	u_int			 ncaps = 0, nopts = 0;
	char			*copy = NULL, *base, *dot, *cmd;
	int			 rc;

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}

	if (scope != NULL && strcmp(scope, "server") != 0 &&
	    strcmp(scope, "session") != 0 && strcmp(scope, "window") != 0 &&
	    strcmp(scope, "pane") != 0) {
		cmdq_error(item, "bad scope: %s", scope);
		return (CMD_RETURN_ERROR);
	}

	if (name == NULL) {
		/* Default name: basename without a .wasm suffix. */
		copy = xstrdup(path);
		if ((base = strrchr(copy, '/')) != NULL)
			base++;
		else
			base = copy;
		if ((dot = strrchr(base, '.')) != NULL &&
		    strcmp(dot, ".wasm") == 0)
			*dot = '\0';
		if (*base == '\0') {
			cmdq_error(item, "cannot derive plugin name from %s",
			    path);
			free(copy);
			return (CMD_RETURN_ERROR);
		}
		name = base;
	}

	for (av = args_first_value(args, 'c'); av != NULL;
	    av = args_next_value(av)) {
		caps = xreallocarray(caps, ncaps + 1, sizeof *caps);
		caps[ncaps++] = av->string;
	}
	for (av = args_first_value(args, 'o'); av != NULL;
	    av = args_next_value(av)) {
		opts = xreallocarray(opts, nopts + 1, sizeof *opts);
		opts[nopts++] = av->string;
	}

	evb = evbuffer_new();
	if (evb == NULL)
		fatalx("out of memory");
	rc = pgh_plugin_load(name, path, scope,
	    (const char *const *)caps, ncaps, (const char *const *)opts,
	    nopts, cmd_plugin_sink, evb);
	free(caps);
	free(opts);
	if (rc != 0) {
		evbuffer_add(evb, "", 1);
		cmdq_error(item, "load-plugin %s: %s", name,
		    (const char *)EVBUFFER_DATA(evb));
		evbuffer_free(evb);
		free(copy);
		return (CMD_RETURN_ERROR);
	}
	evbuffer_free(evb);

	/*
	 * Remember the command so that a server restart can load the plugin
	 * again: guest memory does not survive the exec.
	 */
	cmd = cmd_print(self);
	server_handoff_record_plugin(name, cmd);
	free(cmd);
	free(copy);

	/* Instantiation is queued; run it at the next safe point. */
	plugin_schedule_drain();
	return (CMD_RETURN_NORMAL);
}

static enum cmd_retval
cmd_unload_plugin_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args	*args = cmd_get_args(self);
	const char	*name = args_string(args, 0);

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}
	if (pgh_plugin_unload(name) != 0) {
		cmdq_error(item, "unknown plugin: %s", name);
		return (CMD_RETURN_ERROR);
	}
	server_handoff_forget_plugin(name);
	plugin_schedule_drain();
	return (CMD_RETURN_NORMAL);
}

/*
 * Send a command string to a named plugin as a targeted "plugin-command"
 * event (delivered only to that plugin's subscribed instances). The
 * target pane/window/session travel in the event scope, so key bindings
 * can tell a plugin where to act.
 */
static enum cmd_retval
cmd_plugin_command_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args		*args = cmd_get_args(self);
	struct cmd_find_state	*target = cmdq_get_target(item);
	struct client		*c = cmdq_get_client(item);
	struct plugin_buf	*pb;

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}

	pb = plugin_event_create("plugin-command");
	plugin_event_str(pb, "plugin", args_string(args, 0));
	plugin_event_str(pb, "text", args_string(args, 1));
	if (c != NULL)
		plugin_event_scope(pb, PGH_OBJ_CLIENT, c->id);
	if (target->s != NULL)
		plugin_event_scope(pb, PGH_OBJ_SESSION, target->s->id);
	if (target->w != NULL)
		plugin_event_scope(pb, PGH_OBJ_WINDOW, target->w->id);
	if (target->wp != NULL)
		plugin_event_scope(pb, PGH_OBJ_PANE, target->wp->id);
	plugin_event_send(pb);
	return (CMD_RETURN_NORMAL);
}

/*
 * Reconcile the managed plugin pool against a TOML manifest: load new
 * entries, upsert changed ones, unload managed plugins the manifest no
 * longer names. Validation is atomic (a bad manifest changes nothing).
 */
static enum cmd_retval
cmd_sync_plugins_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args	*args = cmd_get_args(self);
	const char	*path = args_string(args, 0);
	struct evbuffer	*evb;
	char		*cmd;

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}

	evb = evbuffer_new();
	if (evb == NULL)
		fatalx("out of memory");
	if (pgh_plugin_sync(path, cmd_plugin_sink, evb) != 0) {
		evbuffer_add(evb, "", 1);
		cmdq_error(item, "sync-plugins: %s",
		    (const char *)EVBUFFER_DATA(evb));
		evbuffer_free(evb);
		return (CMD_RETURN_ERROR);
	}
	cmd_plugin_print(item, evb);
	evbuffer_free(evb);

	/*
	 * Remember the manifest so that a server restart reconciles the same
	 * pool again. The name is NULL because one manifest covers many
	 * plugins, so the record is never replaced.
	 */
	cmd = cmd_print(self);
	server_handoff_record_plugin(NULL, cmd);
	free(cmd);

	/* Instantiations and unloads are queued; run at the safe point. */
	plugin_schedule_drain();
	return (CMD_RETURN_NORMAL);
}

static enum cmd_retval
cmd_reload_plugin_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args	*args = cmd_get_args(self);
	const char	*name = NULL;
	struct evbuffer	*evb;
	int		 rc;

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}
	if (args_count(args) > 0)
		name = args_string(args, 0);
	else if (!args_has(args, 'a')) {
		cmdq_error(item, "plugin name or -a required");
		return (CMD_RETURN_ERROR);
	}

	evb = evbuffer_new();
	if (evb == NULL)
		fatalx("out of memory");
	rc = pgh_plugin_reload(name, cmd_plugin_sink, evb);
	if (rc != 0) {
		evbuffer_add(evb, "", 1);
		cmdq_error(item, "reload failed: %s",
		    (const char *)EVBUFFER_DATA(evb));
		evbuffer_free(evb);
		return (CMD_RETURN_ERROR);
	}
	evbuffer_free(evb);
	plugin_schedule_drain();
	return (CMD_RETURN_NORMAL);
}

static enum cmd_retval
cmd_enable_plugin_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args	*args = cmd_get_args(self);
	const char	*name = args_string(args, 0);
	int		 enable;

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}
	enable = (cmd_get_entry(self) == &cmd_enable_plugin_entry);
	if (pgh_plugin_set_enabled(name, enable) != 0) {
		cmdq_error(item, "unknown plugin: %s", name);
		return (CMD_RETURN_ERROR);
	}
	plugin_schedule_drain();
	return (CMD_RETURN_NORMAL);
}

static enum cmd_retval
cmd_plugin_log_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args	*args = cmd_get_args(self);
	const char	*name = NULL, *nstr;
	struct evbuffer	*evb;
	u_int		 limit = 0;

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}
	if (args_count(args) > 0)
		name = args_string(args, 0);
	if ((nstr = args_get(args, 'n')) != NULL)
		limit = strtonum(nstr, 1, 10000, NULL);

	evb = evbuffer_new();
	if (evb == NULL)
		fatalx("out of memory");
	pgh_query_log(name, limit, cmd_plugin_sink, evb);
	cmd_plugin_print(item, evb);
	evbuffer_free(evb);
	return (CMD_RETURN_NORMAL);
}
