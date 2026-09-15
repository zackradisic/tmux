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

#include <stdlib.h>
#include <string.h>

#include "tmux.h"

/*
 * Mirror a session on a remote tmux server as a local shadow session, or
 * with -k drop such a link again.
 */

static enum cmd_retval	cmd_remote_attach_exec(struct cmd *,
			    struct cmdq_item *);

const struct cmd_entry cmd_remote_attach_entry = {
	.name = "remote-attach",
	.alias = "remote",

	.args = { "c:kLt:", 1, 1, NULL },
	.usage = "[-kL] [-c working-directory] [-t remote-session] host",

	.flags = CMD_STARTSERVER,
	.exec = cmd_remote_attach_exec
};

static enum cmd_retval
cmd_remote_attach_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args		*args = cmd_get_args(self);
	const char		*host = args_string(args, 0);
	const char		*session = args_get(args, 't');
	const char		*cwd = args_get(args, 'c');
	struct remote_link	*rl, *next;
	struct client		*c = cmdq_get_client(item);
	char			*cause = NULL;
	u_int			 killed = 0;

	if (*host == '\0') {
		cmdq_error(item, "empty host");
		return (CMD_RETURN_ERROR);
	}

	if (args_has(args, 'L'))
		return (remote_link_list(item, host));

	if (args_has(args, 'k')) {
		rl = remote_link_first();
		while (rl != NULL) {
			next = remote_link_next(rl);
			if (strcmp(remote_link_host(rl), host) == 0 &&
			    (session == NULL ||
			    (remote_link_remote_session(rl) != NULL &&
			    strcmp(remote_link_remote_session(rl),
			    session) == 0))) {
				remote_link_destroy(rl);
				killed++;
			}
			rl = next;
		}
		if (killed == 0) {
			cmdq_error(item, "no remote link to %s", host);
			return (CMD_RETURN_ERROR);
		}
		return (CMD_RETURN_NORMAL);
	}

	if (remote_link_find(host, session) != NULL) {
		cmdq_error(item, "already linked to %s", host);
		return (CMD_RETURN_ERROR);
	}
	rl = remote_link_create(host, session, cwd, -1, &cause);
	if (rl == NULL) {
		cmdq_error(item, "%s", cause);
		free(cause);
		return (CMD_RETURN_ERROR);
	}
	if (c != NULL && c->name != NULL)
		remote_link_set_menu_client(rl, c->name);
	return (CMD_RETURN_NORMAL);
}
