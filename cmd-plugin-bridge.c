/* $OpenBSD$ */

/*
 * Copyright (c) 2026 Nicholas Marriott <nicholas.marriott@gmail.com>
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

#include "tmux.h"

/*
 * Carry one plugin bridge frame from a control client (the local end of a
 * remote link) to this server's plugin host. Only a control client may
 * send it: the frame is attributed to that client as a bridge peer.
 */

static enum cmd_retval	cmd_plugin_bridge_exec(struct cmd *,
			    struct cmdq_item *);

const struct cmd_entry cmd_plugin_bridge_entry = {
	.name = "plugin-bridge",
	.alias = NULL,

	.args = { "", 1, 1, NULL },
	.usage = "base64-frame",

	.flags = CMD_CLIENT_CANFAIL,
	.exec = cmd_plugin_bridge_exec
};

static enum cmd_retval
cmd_plugin_bridge_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args	*args = cmd_get_args(self);
	struct client	*c = cmdq_get_client(item);

	if (c == NULL || (~c->flags & CLIENT_CONTROL)) {
		cmdq_error(item, "plugin-bridge needs a control client");
		return (CMD_RETURN_ERROR);
	}
	if (!plugin_enabled()) {
		cmdq_error(item, "plugin support not available");
		return (CMD_RETURN_ERROR);
	}
	plugin_bridge_recv(c->id, args_string(args, 0));
	return (CMD_RETURN_NORMAL);
}
