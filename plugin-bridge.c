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
#include <netinet/in.h>
#include <resolv.h>

#include <stdlib.h>
#include <string.h>

#include "tmux.h"
#include "plugin-host.h"
#include "plugin-internal.h"

/*
 * The plugin bridge transport. The Rust host frames and interprets the
 * bytes; this file only moves them between the host and the two kinds of
 * peer:
 *
 *  - a remote link this server made (peer id with PGH_PEER_LINK set): a
 *    frame goes out as a "plugin-bridge <base64>" control mode command
 *    and comes back as a "%bridge <base64>" line;
 *  - a control client of this server (peer id = client id): a frame goes
 *    out as a "%bridge <base64>" line through control_write() and comes
 *    in through the plugin-bridge command.
 *
 * Everything that reaches the host here is enqueue-only, so these can run
 * from a command or a job callback.
 */

static struct client *
plugin_bridge_client(u_int id)
{
	struct client	*c;

	TAILQ_FOREACH(c, &clients, entry) {
		if (c->id == id && (c->flags & CLIENT_CONTROL) &&
		    (~c->flags & CLIENT_EXIT) && c->control_state != NULL)
			return (c);
	}
	return (NULL);
}

/* Vtable: send a frame to a peer. */
int
plugin_vtable_bridge_send(uint32_t peer, const uint8_t *data, size_t len)
{
	struct remote_link	*rl;
	struct client		*c;
	char			*b64;
	size_t			 b64len;
	int			 rc;

	if (peer & PGH_PEER_LINK) {
		rl = remote_link_find_by_id(peer & ~PGH_PEER_LINK);
		if (rl == NULL)
			return (-1);
		return (remote_link_bridge_send(rl, data, len));
	}

	c = plugin_bridge_client(peer);
	if (c == NULL)
		return (-1);
	b64len = 4 * ((len + 2) / 3) + 1;
	b64 = xmalloc(b64len);
	rc = b64_ntop(data, len, b64, b64len);
	if (rc < 0) {
		free(b64);
		return (-1);
	}
	control_write(c, "%%bridge %s", b64);
	free(b64);
	return (0);
}

/* A frame arrived from a peer as base64: decode and hand it to the host. */
void
plugin_bridge_recv(u_int peer, const char *b64)
{
	u_char	*data;
	size_t	 size;
	int	 len;

	if (!plugin_enabled())
		return;
	size = strlen(b64);
	data = xmalloc(size + 1);
	len = b64_pton(b64, data, size + 1);
	if (len < 0) {
		log_debug("%s: bad base64 from peer %u", __func__, peer);
		free(data);
		return;
	}
	pgh_bridge_recv(peer, data, len);
	free(data);
	plugin_schedule_drain();
}

/* A control client went away; the host drops its peer. */
void
plugin_bridge_client_lost(struct client *c)
{
	if (!plugin_enabled())
		return;
	pgh_bridge_state(c->id, NULL, 0);
	plugin_schedule_drain();
}

/* A remote link came up or went down. */
void
plugin_bridge_link_state(struct remote_link *rl, int up)
{
	if (!plugin_enabled())
		return;
	pgh_bridge_state(remote_link_id(rl) | PGH_PEER_LINK,
	    remote_link_host(rl), up);
	plugin_schedule_drain();
}
