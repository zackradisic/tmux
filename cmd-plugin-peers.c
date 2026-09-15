/* $OpenBSD$ */

/*
 * Copyright (c) 2026 Zack Radisic
 *
 * Permission to use, copy, modify, and distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF MIND, USE, DATA OR PROFITS, WHETHER
 * IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING
 * OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#include <sys/types.h>

#include <string.h>

#include "tmux.h"
#include "plugin-host.h"

/*
 * Peer grants: which linked server may call which of this server's plugins
 * back over the bridge. The gate lives in the plugin host; this command
 * reads and writes its grant table.
 *
 *   plugin-peers list
 *   plugin-peers allow  <server> [plugin]
 *   plugin-peers deny   <server> [plugin]
 *   plugin-peers revoke <server> [plugin]
 *   plugin-peers menu   [server]
 */

static enum cmd_retval	cmd_plugin_peers_exec(struct cmd *, struct cmdq_item *);

const struct cmd_entry cmd_plugin_peers_entry = {
	.name = "plugin-peers",
	.alias = NULL,

	.args = { "", 1, 3, NULL },
	.usage = "list|allow|deny|revoke|menu [server] [plugin]",

	.flags = CMD_AFTERHOOK,
	.exec = cmd_plugin_peers_exec
};

static void
cmd_plugin_peers_sink(void *ctx, const char *ptr, size_t len)
{
	struct evbuffer	*evb = ctx;

	evbuffer_add(evb, ptr, len);
}

static enum cmd_retval
cmd_plugin_peers_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args	*args = cmd_get_args(self);
	struct client	*c = cmdq_get_client(item);
	const char	*verb = args_string(args, 0);
	const char	*server = args_count(args) > 1 ? args_string(args, 1) : NULL;
	const char	*plugin = args_count(args) > 2 ? args_string(args, 2) : NULL;
	struct evbuffer	*evb;
	char		*line;

	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}

	if (strcmp(verb, "list") == 0) {
		evb = evbuffer_new();
		if (evb == NULL)
			fatalx("out of memory");
		pgh_peers_list(cmd_plugin_peers_sink, evb);
		while ((line = evbuffer_readline(evb)) != NULL) {
			cmdq_print(item, "%s", line);
			free(line);
		}
		evbuffer_free(evb);
		return (CMD_RETURN_NORMAL);
	}

	if (strcmp(verb, "allow") == 0 || strcmp(verb, "deny") == 0) {
		if (server == NULL) {
			cmdq_error(item, "usage: plugin-peers %s <server> "
			    "[plugin]", verb);
			return (CMD_RETURN_ERROR);
		}
		if (pgh_peers_set(server, plugin, verb) != 0) {
			cmdq_error(item, "plugin-peers %s failed", verb);
			return (CMD_RETURN_ERROR);
		}
		return (CMD_RETURN_NORMAL);
	}

	if (strcmp(verb, "revoke") == 0) {
		if (server == NULL) {
			cmdq_error(item, "usage: plugin-peers revoke <server> "
			    "[plugin]");
			return (CMD_RETURN_ERROR);
		}
		pgh_peers_revoke(server, plugin);
		return (CMD_RETURN_NORMAL);
	}

	if (strcmp(verb, "menu") == 0) {
		if (server == NULL) {
			cmdq_error(item, "usage: plugin-peers menu <server>");
			return (CMD_RETURN_ERROR);
		}
		if (c == NULL || c->name == NULL) {
			cmdq_error(item, "plugin-peers menu needs a client");
			return (CMD_RETURN_ERROR);
		}
		pgh_peers_menu(server, c->name);
		return (CMD_RETURN_NORMAL);
	}

	cmdq_error(item, "unknown verb: %s", verb);
	return (CMD_RETURN_ERROR);
}
