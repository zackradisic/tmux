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

#include <string.h>

#include "tmux.h"
#include "plugin-host.h"
#include "plugin-internal.h"

/*
 * Rust -> C vtable callbacks. Every function here:
 *  - runs synchronously on the main thread from inside a pgh_* call;
 *  - validates any object handle via the *_find_by_id lookups (weak
 *    handles: lookup failure means the object is dead);
 *  - may re-enter pgh_notify but no other pgh_* entry point.
 */

static void	plugin_vtable_emit_session(struct plugin_buf *,
		    struct session *);
static void	plugin_vtable_emit_window(struct plugin_buf *,
		    struct window *);
static void	plugin_vtable_emit_pane(struct plugin_buf *,
		    struct window_pane *);
static void	plugin_vtable_emit_client(struct plugin_buf *,
		    struct client *);
static void	plugin_vtable_flush(struct plugin_buf *, pgh_sink,
		    void *);

/* Sentinel matching abi-types NONE_ID. */
#define PLUGIN_VTABLE_NONE_ID 0xffffffffU

/* Pane/client record flag bits (mirror abi-types). */
#define PLUGIN_PANE_ACTIVE	0x1
#define PLUGIN_PANE_FLOATING	0x2
#define PLUGIN_PANE_DEAD	0x4
#define PLUGIN_CLIENT_ATTACHED	0x1
#define PLUGIN_CLIENT_CONTROL	0x2

void
plugin_vtable_log(int level, const char *plugin, const char *msg)
{
	log_debug("plugin[%s]: %s%s", plugin,
	    level >= PGH_LOG_ERROR ? "error: " : "", msg);
}

static void
plugin_vtable_flush(struct plugin_buf *pb, pgh_sink sink, void *ctx)
{
	const u_char	*data;
	size_t		 len;

	data = plugin_buf_data(pb, &len);
	sink(ctx, (const char *)data, len);
	plugin_buf_free(pb);
}

static void
plugin_vtable_emit_session(struct plugin_buf *pb, struct session *s)
{
	struct winlink	*wl;
	u_int		 n = 0;

	plugin_buf_u32(pb, s->id);
	plugin_buf_u8(pb, s->attached != 0);
	plugin_buf_u32(pb, s->curw != NULL ?
	    s->curw->window->id : PLUGIN_VTABLE_NONE_ID);
	plugin_buf_str(pb, s->name);
	RB_FOREACH(wl, winlinks, &s->windows)
		n++;
	plugin_buf_u32(pb, n);
	RB_FOREACH(wl, winlinks, &s->windows) {
		plugin_buf_u32(pb, wl->idx);
		plugin_buf_u32(pb, wl->window->id);
	}
}

static void
plugin_vtable_emit_window(struct plugin_buf *pb, struct window *w)
{
	struct winlink		*wl;
	struct window_pane	*wp;
	u_int			 n;

	plugin_buf_u32(pb, w->id);
	plugin_buf_u32(pb, w->sx);
	plugin_buf_u32(pb, w->sy);
	plugin_buf_u32(pb, w->active != NULL ?
	    w->active->id : PLUGIN_VTABLE_NONE_ID);
	plugin_buf_str(pb, w->name);
	n = 0;
	TAILQ_FOREACH(wl, &w->winlinks, wentry)
		n++;
	plugin_buf_u32(pb, n);
	TAILQ_FOREACH(wl, &w->winlinks, wentry)
		plugin_buf_u32(pb, wl->session->id);
	n = 0;
	TAILQ_FOREACH(wp, &w->panes, entry)
		n++;
	plugin_buf_u32(pb, n);
	TAILQ_FOREACH(wp, &w->panes, entry)
		plugin_buf_u32(pb, wp->id);
}

static void
plugin_vtable_emit_pane(struct plugin_buf *pb, struct window_pane *wp)
{
	char	*cwd = NULL;
	uint8_t	 flags = 0;

	if (wp == wp->window->active)
		flags |= PLUGIN_PANE_ACTIVE;
	if (window_pane_is_floating(wp))
		flags |= PLUGIN_PANE_FLOATING;
	if (wp->flags & PANE_EXITED)
		flags |= PLUGIN_PANE_DEAD;

	plugin_buf_u32(pb, wp->id);
	plugin_buf_u32(pb, wp->window->id);
	plugin_buf_u32(pb, wp->sx);
	plugin_buf_u32(pb, wp->sy);
	plugin_buf_u8(pb, flags);
	plugin_buf_str(pb, wp->base.title);
	plugin_buf_str(pb, wp->shell);
	/* Same source as #{pane_current_path}; buffer is static, not freed. */
	if (wp->fd != -1)
		cwd = osdep_get_cwd(wp->fd);
	plugin_buf_str(pb, cwd);
}

static void
plugin_vtable_emit_client(struct plugin_buf *pb, struct client *c)
{
	uint8_t	flags = 0;

	if (c->session != NULL)
		flags |= PLUGIN_CLIENT_ATTACHED;
	if (c->flags & CLIENT_CONTROL)
		flags |= PLUGIN_CLIENT_CONTROL;

	plugin_buf_u32(pb, c->id);
	plugin_buf_u32(pb, c->session != NULL ?
	    c->session->id : PLUGIN_VTABLE_NONE_ID);
	plugin_buf_u8(pb, flags);
	plugin_buf_str(pb, c->name);
}

void
plugin_vtable_list_objects(int kind, pgh_sink sink, void *ctx)
{
	struct plugin_buf	*pb;
	struct session		*s;
	struct window		*w;
	struct window_pane	*wp;
	struct client		*c;
	u_int			 n = 0;

	pb = plugin_buf_create();
	switch (kind) {
	case PGH_OBJ_SESSION:
		RB_FOREACH(s, sessions, &sessions)
			n++;
		plugin_buf_u32(pb, n);
		RB_FOREACH(s, sessions, &sessions)
			plugin_vtable_emit_session(pb, s);
		break;
	case PGH_OBJ_WINDOW:
		RB_FOREACH(w, windows, &windows)
			n++;
		plugin_buf_u32(pb, n);
		RB_FOREACH(w, windows, &windows)
			plugin_vtable_emit_window(pb, w);
		break;
	case PGH_OBJ_PANE:
		RB_FOREACH(wp, window_pane_tree, &all_window_panes)
			n++;
		plugin_buf_u32(pb, n);
		RB_FOREACH(wp, window_pane_tree, &all_window_panes)
			plugin_vtable_emit_pane(pb, wp);
		break;
	case PGH_OBJ_CLIENT:
		TAILQ_FOREACH(c, &clients, entry) {
			if (~c->flags & CLIENT_DEAD)
				n++;
		}
		plugin_buf_u32(pb, n);
		TAILQ_FOREACH(c, &clients, entry) {
			if (c->flags & CLIENT_DEAD)
				continue;
			plugin_vtable_emit_client(pb, c);
		}
		break;
	default:
		plugin_buf_u32(pb, 0);
		break;
	}
	plugin_vtable_flush(pb, sink, ctx);
}

/*
 * Expand a format string against a scope (kind -1 = server/global, else
 * PGH_OBJ_SESSION/WINDOW/PANE). FORMAT_NOJOBS: #() never spawns. The fmt
 * string is borrowed for the call and consumed before returning.
 * Returns 0, -1 dead/bad target.
 */
int
plugin_vtable_format_expand(int kind, u_int id, const char *fmt,
    pgh_sink sink, void *ctx)
{
	struct format_tree	*ft;
	struct session		*s = NULL;
	struct window		*w = NULL;
	struct winlink		*wl = NULL;
	struct window_pane	*wp = NULL;
	char			*out;

	switch (kind) {
	case -1:
		break;
	case PGH_OBJ_SESSION:
		if ((s = session_find_by_id(id)) == NULL)
			return (-1);
		break;
	case PGH_OBJ_WINDOW:
		if ((w = window_find_by_id(id)) == NULL)
			return (-1);
		wl = TAILQ_FIRST(&w->winlinks);
		s = (wl != NULL) ? wl->session : NULL;
		break;
	case PGH_OBJ_PANE:
		if ((wp = window_pane_find_by_id(id)) == NULL ||
		    (wp->flags & PANE_DESTROYED))
			return (-1);
		w = wp->window;
		wl = (w != NULL) ? TAILQ_FIRST(&w->winlinks) : NULL;
		s = (wl != NULL) ? wl->session : NULL;
		break;
	default:
		return (-1);
	}

	ft = format_create(NULL, NULL, FORMAT_NONE, FORMAT_NOJOBS);
	format_defaults(ft, NULL, s, wl, wp);
	out = format_expand(ft, fmt);
	sink(ctx, out, strlen(out));
	free(out);
	format_free(ft);
	return (0);
}

/*
 * Relation queries for scope checks (PGH_REL_*). Returns the related id,
 * a 0/1 membership answer, or -1 when an object is dead.
 */
int64_t
plugin_vtable_obj_relation(int rel, u_int a, u_int b)
{
	struct window_pane	*wp;
	struct window		*w;
	struct session		*s;
	struct winlink		*wl;

	switch (rel) {
	case PGH_REL_PANE_WINDOW:
		wp = window_pane_find_by_id(a);
		if (wp == NULL || wp->window == NULL)
			return (-1);
		return (wp->window->id);
	case PGH_REL_SESSION_CURWIN:
		s = session_find_by_id(a);
		if (s == NULL || s->curw == NULL)
			return (-1);
		return (s->curw->window->id);
	case PGH_REL_WINDOW_IN_SESSION:
		w = window_find_by_id(a);
		if (w == NULL)
			return (-1);
		TAILQ_FOREACH(wl, &w->winlinks, wentry) {
			if (wl->session->id == b)
				return (1);
		}
		return (0);
	case PGH_REL_PANE_IN_WINDOW:
		wp = window_pane_find_by_id(a);
		if (wp == NULL || wp->window == NULL)
			return (-1);
		return (wp->window->id == b);
	case PGH_REL_PANE_IN_SESSION:
		wp = window_pane_find_by_id(a);
		if (wp == NULL || wp->window == NULL)
			return (-1);
		TAILQ_FOREACH(wl, &wp->window->winlinks, wentry) {
			if (wl->session->id == b)
				return (1);
		}
		return (0);
	}
	return (-1);
}

/*
 * Send keys to a pane. literal != 0 treats `keys` as a UTF-8 string, one
 * key per character; otherwise `keys` is a single tmux key name ("Enter",
 * "C-c", "M-x", ...). Returns 0, -1 if the pane is dead, -2 for a bad key
 * name.
 */
int
plugin_vtable_send_keys(u_int pane_id, const char *keys, int literal)
{
	struct window_pane	*wp;
	struct winlink		*wl;
	struct session		*s;
	struct utf8_data	*ud, *loop;
	utf8_char		 uc;
	key_code		 key;

	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL || (wp->flags & PANE_DESTROYED))
		return (-1);
	wl = TAILQ_FIRST(&wp->window->winlinks);
	s = (wl != NULL) ? wl->session : NULL;

	if (!literal) {
		key = key_string_lookup_string(keys);
		if (key == KEYC_NONE || key == KEYC_UNKNOWN)
			return (-2);
		window_pane_key(wp, NULL, s, wl, key, NULL);
		return (0);
	}

	ud = utf8_fromcstr(keys);
	for (loop = ud; loop->size != 0; loop++) {
		if (loop->size == 1 && loop->data[0] <= 0x7f)
			key = loop->data[0];
		else {
			if (utf8_from_data(loop, &uc) != UTF8_DONE)
				continue;
			key = uc;
		}
		window_pane_key(wp, NULL, s, wl, key, NULL);
	}
	free(ud);
	return (0);
}

/*
 * Capture pane contents as text into the sink, one line per row.
 * start/end are grid rows relative to the top of the visible screen
 * (negative reaches into history), end inclusive; escapes != 0 includes
 * escape sequences. The Rust side validates ranges/caps before calling.
 * Returns 0 or -1 if the pane is dead.
 */
int
plugin_vtable_capture_pane(u_int pane_id, int start, int end, int escapes,
    pgh_sink sink, void *ctx)
{
	struct window_pane	*wp;
	struct grid		*gd;
	struct screen		*s;
	struct grid_cell	*gc = NULL;
	int			 flags = 0;
	u_int			 i, sx, top, bottom;
	char			*line;

	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL || (wp->flags & PANE_DESTROYED))
		return (-1);
	s = &wp->base;
	gd = wp->base.grid;
	sx = screen_size_x(s);

	if (escapes)
		flags |= GRID_STRING_WITH_SEQUENCES|
		    GRID_STRING_ESCAPE_SEQUENCES;

	/* Clamp to the grid before mixing with unsigned arithmetic. */
	if (start >= (int)gd->sy)
		start = gd->sy - 1;
	if (end >= (int)gd->sy)
		end = gd->sy - 1;
	if (start < 0 && (u_int)-start > gd->hsize)
		top = 0;
	else
		top = gd->hsize + start;
	if (end < 0 && (u_int)-end > gd->hsize)
		bottom = 0;
	else
		bottom = gd->hsize + end;
	if (top > bottom)
		return (0);
	if (bottom - top >= PLUGIN_CAPTURE_MAX_LINES)
		bottom = top + PLUGIN_CAPTURE_MAX_LINES - 1;

	for (i = top; i <= bottom; i++) {
		/*
		 * gc may point at grid_string_cells' internal static cell;
		 * it is not owned by the caller and must not be freed.
		 */
		line = grid_string_cells(gd, 0, i, sx, &gc, flags, s);
		sink(ctx, line, strlen(line));
		sink(ctx, "\n", 1);
		free(line);
	}
	return (0);
}

/*
 * Read one environment variable from a pane's foreground process into
 * the sink. Uses the pty master fd, like pane_current_command. Returns
 * 0, -1 if the pane is dead, -2 if the variable is not set.
 */
int
plugin_vtable_pane_env(u_int pane_id, const char *name, pgh_sink sink,
    void *ctx)
{
	struct window_pane	*wp;
	char			*value;

	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL || (wp->flags & PANE_DESTROYED) || wp->fd == -1)
		return (-1);

	value = osdep_get_env(wp->fd, name);
	if (value == NULL)
		return (-2);
	sink(ctx, value, strlen(value));
	free(value);
	return (0);
}

/* Return the pid of a pane's foreground process group, or -1. */
int
plugin_vtable_pane_pid(u_int pane_id)
{
	struct window_pane	*wp;
	pid_t			 pgrp;

	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL || (wp->flags & PANE_DESTROYED) || wp->fd == -1)
		return (-1);

	pgrp = tcgetpgrp(wp->fd);
	if (pgrp == -1)
		return (-1);
	return ((int)pgrp);
}

/* Return the open-file paths of a pane's foreground process, one per line. */
int
plugin_vtable_pane_fds(u_int pane_id, pgh_sink sink, void *ctx)
{
	struct window_pane	*wp;
	char			*value;

	wp = window_pane_find_by_id(pane_id);
	if (wp == NULL || (wp->flags & PANE_DESTROYED) || wp->fd == -1)
		return (-1);

	value = osdep_get_fds(wp->fd);
	if (value == NULL)
		return (-2);
	sink(ctx, value, strlen(value));
	free(value);
	return (0);
}

/* Resolve the options tree for a scope kind + id; NULL if dead/bad. */
static struct options *
plugin_vtable_options(int kind, u_int id)
{
	struct session		*s;
	struct window		*w;
	struct window_pane	*wp;

	switch (kind) {
	case -1:	/* server/global */
		return (global_options);
	case PGH_OBJ_SESSION:
		if ((s = session_find_by_id(id)) == NULL)
			return (NULL);
		return (s->options);
	case PGH_OBJ_WINDOW:
		if ((w = window_find_by_id(id)) == NULL)
			return (NULL);
		return (w->options);
	case PGH_OBJ_PANE:
		if ((wp = window_pane_find_by_id(id)) == NULL)
			return (NULL);
		return (wp->options);
	}
	return (NULL);
}

/*
 * Get an option value (walking the parent chain) as a string into the
 * sink. kind -1 = global. Returns 0, -1 dead target, -2 no such option.
 */
int
plugin_vtable_get_option(int kind, u_int id, const char *name, pgh_sink sink,
    void *ctx)
{
	struct options		*oo;
	struct options_entry	*o;
	char			*value;

	oo = plugin_vtable_options(kind, id);
	if (oo == NULL)
		return (-1);
	o = options_get(oo, name);
	if (o == NULL)
		return (-2);
	value = options_to_string(o, NULL, 0);
	sink(ctx, value, strlen(value));
	free(value);
	return (0);
}

/*
 * Set a user option (@-prefixed only in v1; real options go through
 * run_command). Returns 0, -1 dead target, -2 not a user option.
 *
 * When the value actually changes, attached clients get a status-line
 * redraw: publishing state for #{@...} in the status line is the most
 * common reason a plugin sets options, and unlike the set-option command
 * this path does not redraw on its own. The changed-check keeps periodic
 * writers from causing redraw churn.
 */
int
plugin_vtable_set_option(int kind, u_int id, const char *name,
    const char *value)
{
	struct options		*oo;
	struct options_entry	*o;
	struct client		*c;
	char			*cur;
	int			 changed = 1;

	oo = plugin_vtable_options(kind, id);
	if (oo == NULL)
		return (-1);
	if (*name != '@')
		return (-2);

	o = options_get_only(oo, name);
	if (o != NULL) {
		cur = options_to_string(o, NULL, 0);
		changed = (strcmp(cur, value) != 0);
		free(cur);
	}
	if (!changed)
		return (0);

	options_set_string(oo, name, 0, "%s", value);
	TAILQ_FOREACH(c, &clients, entry) {
		if ((~c->flags & CLIENT_DEAD) && c->session != NULL)
			c->flags |= CLIENT_REDRAWSTATUS;
	}
	return (0);
}

/*
 * Show a status-line message. client_id targets one client; -1 targets
 * every attached client. Always lands in the server message log too.
 * Returns 0 (missing client degrades to log-only).
 */
int
plugin_vtable_display_message(int client_id, const char *plugin,
    const char *msg)
{
	struct client	*c;
	int		 found = 0;

	server_add_message("plugin %s: %s", plugin, msg);
	TAILQ_FOREACH(c, &clients, entry) {
		if (c->flags & CLIENT_DEAD)
			continue;
		if (c->session == NULL)
			continue;
		if (client_id >= 0 && c->id != (u_int)client_id)
			continue;
		found = 1;
		status_message_set(c, -1, 1, 0, 0, "%s", msg);
	}
	return (found || client_id < 0 ? 0 : -1);
}

/*
 * A plugin changed state in a user-visible way (disabled after repeated
 * failures, load failure, ...). Show it prominently: status line on every
 * attached client plus the server message log (and the config-error list
 * during config load).
 */
void
plugin_vtable_state_changed(const char *plugin, const char *state,
    const char *reason)
{
	struct client	*c;

	if (!cfg_finished) {
		cfg_add_cause("plugin %s: %s (%s)", plugin, state, reason);
		return;
	}
	server_add_message("plugin %s: %s (%s)", plugin, state, reason);
	TAILQ_FOREACH(c, &clients, entry) {
		if ((c->flags & CLIENT_DEAD) || c->session == NULL)
			continue;
		status_message_set(c, -1, 1, 0, 0, "Plugin %s %s: %s",
		    plugin, state, reason);
	}
}

int
plugin_vtable_resolve_object(int kind, u_int id, pgh_sink sink, void *ctx)
{
	struct plugin_buf	*pb;
	struct session		*s;
	struct window		*w;
	struct window_pane	*wp;
	struct client		*c;

	pb = plugin_buf_create();
	switch (kind) {
	case PGH_OBJ_SESSION:
		if ((s = session_find_by_id(id)) == NULL)
			goto missing;
		plugin_vtable_emit_session(pb, s);
		break;
	case PGH_OBJ_WINDOW:
		if ((w = window_find_by_id(id)) == NULL)
			goto missing;
		plugin_vtable_emit_window(pb, w);
		break;
	case PGH_OBJ_PANE:
		if ((wp = window_pane_find_by_id(id)) == NULL)
			goto missing;
		plugin_vtable_emit_pane(pb, wp);
		break;
	case PGH_OBJ_CLIENT:
		TAILQ_FOREACH(c, &clients, entry) {
			if (c->id == id && (~c->flags & CLIENT_DEAD))
				break;
		}
		if (c == NULL)
			goto missing;
		plugin_vtable_emit_client(pb, c);
		break;
	default:
		goto missing;
	}
	plugin_vtable_flush(pb, sink, ctx);
	return (0);

missing:
	plugin_buf_free(pb);
	return (-1);
}
