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

#include <event.h>
#include <stdlib.h>
#include <string.h>

#include "tmux.h"
#include "plugin-host.h"
#include "plugin-internal.h"

/*
 * The generic plugin UI window mode. One instance per open plugin mode,
 * always on a freshly spawned empty floating pane (plugin-mode.c owns the
 * spawn, the id registry and the vtable entry points; this file owns the
 * mode itself).
 *
 * The plugin renders by sending ANSI bytes which are parsed server-side
 * into the mode screen (input_parse_screen - full escape support, so TUI
 * libraries work unmodified), and may declare one retained preview rect
 * that mirrors another pane's live grid (screen_write_preview blits grid
 * cells, no escape reparsing), refreshed on a timer while set.
 *
 * Reentrancy: every callback here can run inside pgh_drain (mode_write
 * arrives mid-dispatch) or from deep teardown paths (free via
 * window_pane_free_modes inside window_pane_destroy). Everything that
 * talks back to the plugin host is therefore enqueue-only
 * (pgh_mode_event); nothing here ever destroys tmux objects.
 */

static struct screen *window_plugin_init(struct window_mode_entry *,
		    struct cmdq_item *, struct cmd_find_state *,
		    struct args *);
static void	window_plugin_free(struct window_mode_entry *);
static void	window_plugin_resize(struct window_mode_entry *, u_int,
		    u_int);
static void	window_plugin_key(struct window_mode_entry *,
		    struct client *, struct session *, struct winlink *,
		    key_code, struct mouse_event *);
static void	window_plugin_refresh_callback(int, short, void *);
static void	window_plugin_draw_preview(struct window_mode_entry *);

const struct window_mode window_plugin_mode = {
	.name = "plugin-mode",

	.init = window_plugin_init,
	.free = window_plugin_free,
	.resize = window_plugin_resize,
	.key = window_plugin_key,
};

/* Preview refresh interval while a preview rect is set. */
#define WINDOW_PLUGIN_REFRESH_MSEC 500

struct window_plugin_mode_data {
	uint64_t		 mode_id;
	struct screen		 screen;
	struct input_ctx	*ictx;
	struct colour_palette	 palette;

	struct {
		int	 set;
		u_int	 pane_id;
		u_int	 px, py, sx, sy;
	} preview;
	struct event		 refresh;

	/* Static string; set by the close path before teardown. */
	const char		*close_reason;
};

static struct screen *
window_plugin_init(struct window_mode_entry *wme,
    __unused struct cmdq_item *item, __unused struct cmd_find_state *fs,
    __unused struct args *args)
{
	struct window_pane		*wp = wme->wp;
	struct window_plugin_mode_data	*data;
	uint64_t			 mode_id;
	struct screen			*s;

	/*
	 * This mode is only entered through plugin_vtable_mode_open, which
	 * stashes the pending mode id just before window_pane_set_mode.
	 * Refuse to initialize without it (nothing else may enter it).
	 */
	if (!plugin_mode_pending_take(&mode_id))
		return (NULL);

	wme->data = data = xcalloc(1, sizeof *data);
	data->mode_id = mode_id;

	s = &data->screen;
	screen_init(s, screen_size_x(&wp->base), screen_size_y(&wp->base), 0);
	s->mode &= ~MODE_CURSOR; /* plugins re-enable with \033[?25h */

	colour_palette_init(&data->palette);
	colour_palette_from_option(&data->palette, wp->options);

	/*
	 * NULL bufferevent as for popups (popup.c); replies that would go
	 * to a terminal (e.g. OSC 52) are dropped by the NULL guard in
	 * input_reply_clipboard.
	 */
	data->ictx = input_init(NULL, NULL, &data->palette, NULL);

	evtimer_set(&data->refresh, window_plugin_refresh_callback, wme);

	return (s);
}

/*
 * Local teardown plus an enqueue-only mode-closed notification. Reached
 * from every mode-exit path: the deferred plugin close (server_kill_pane),
 * a user kill-pane, respawn-pane and window destruction.
 */
static void
window_plugin_free(struct window_mode_entry *wme)
{
	struct window_plugin_mode_data	*data = wme->data;
	struct plugin_buf		*pb;

	pb = plugin_event_create("mode-closed");
	plugin_event_i64(pb, "mode", data->mode_id);
	plugin_event_str(pb, "reason",
	    data->close_reason != NULL ? data->close_reason : "killed");
	plugin_event_send_mode(pb, data->mode_id);

	plugin_mode_unregister(data->mode_id);

	evtimer_del(&data->refresh);
	input_free(data->ictx);
	colour_palette_free(&data->palette);
	screen_free(&data->screen);
	free(data);
}

static void
window_plugin_resize(struct window_mode_entry *wme, u_int sx, u_int sy)
{
	struct window_plugin_mode_data	*data = wme->data;
	struct plugin_buf		*pb;

	screen_resize(&data->screen, sx, sy, 0);

	/* Drop a preview that no longer fits at all; else it is re-clamped
	 * on every draw. */
	if (data->preview.set &&
	    (data->preview.px >= sx || data->preview.py >= sy)) {
		data->preview.set = 0;
		evtimer_del(&data->refresh);
	}

	pb = plugin_event_create("mode-resize");
	plugin_event_i64(pb, "mode", data->mode_id);
	plugin_event_i64(pb, "width", sx);
	plugin_event_i64(pb, "height", sy);
	plugin_event_send_mode(pb, data->mode_id);
}

static void
window_plugin_key(struct window_mode_entry *wme, struct client *c,
    __unused struct session *s, __unused struct winlink *wl, key_code key,
    struct mouse_event *m)
{
	struct window_plugin_mode_data	*data = wme->data;
	struct plugin_buf		*pb;
	u_int				 mx, my;

	pb = plugin_event_create("mode-key");
	plugin_event_i64(pb, "mode", data->mode_id);
	/* key_string_lookup_key returns a static buffer: serialized (copied)
	 * immediately by plugin_event_str. */
	plugin_event_str(pb, "key", key_string_lookup_key(key, 0));
	/* The pressing client, so plugins can act on the right client
	 * (e.g. switch-client for a cross-session jump). */
	if (c != NULL)
		plugin_event_i64(pb, "client", c->id);
	if (KEYC_IS_MOUSE(key) && m != NULL &&
	    cmd_mouse_at(wme->wp, m, &mx, &my, 0) == 0) {
		plugin_event_i64(pb, "mouse_x", mx);
		plugin_event_i64(pb, "mouse_y", my);
		plugin_event_i64(pb, "mouse_b", m->b);
	}
	plugin_event_send_mode(pb, data->mode_id);
}

/* Blit the preview source's live grid into the retained rect. */
static void
window_plugin_draw_preview(struct window_mode_entry *wme)
{
	struct window_plugin_mode_data	*data = wme->data;
	struct screen			*s = &data->screen;
	struct screen_write_ctx		 ctx;
	struct window_pane		*src;
	u_int				 nx, ny, i;

	if (!data->preview.set)
		return;
	if (data->preview.px >= screen_size_x(s) ||
	    data->preview.py >= screen_size_y(s))
		return;
	nx = data->preview.sx;
	if (nx > screen_size_x(s) - data->preview.px)
		nx = screen_size_x(s) - data->preview.px;
	ny = data->preview.sy;
	if (ny > screen_size_y(s) - data->preview.py)
		ny = screen_size_y(s) - data->preview.py;

	src = window_pane_find_by_id(data->preview.pane_id);
	if (src == NULL || (src->flags & PANE_DESTROYED)) {
		/* Source is gone: blank the rect and stop refreshing. */
		screen_write_start(&ctx, s);
		for (i = 0; i < ny; i++) {
			screen_write_cursormove(&ctx, data->preview.px,
			    data->preview.py + i, 0);
			screen_write_clearcharacter(&ctx, nx, 8);
		}
		screen_write_stop(&ctx);
		data->preview.set = 0;
		evtimer_del(&data->refresh);
		wme->wp->flags |= PANE_REDRAW;
		return;
	}

	screen_write_start(&ctx, s);
	screen_write_cursormove(&ctx, data->preview.px, data->preview.py, 0);
	screen_write_preview(&ctx, &src->base, nx, ny);
	screen_write_stop(&ctx);
	wme->wp->flags |= PANE_REDRAW;
}

static void
window_plugin_refresh_callback(__unused int fd, __unused short events,
    void *arg)
{
	struct window_mode_entry	*wme = arg;
	struct window_plugin_mode_data	*data = wme->data;
	struct timeval			 tv = {
		.tv_sec = WINDOW_PLUGIN_REFRESH_MSEC / 1000,
		.tv_usec = (WINDOW_PLUGIN_REFRESH_MSEC % 1000) * 1000
	};

	window_plugin_draw_preview(wme);
	if (data->preview.set)
		evtimer_add(&data->refresh, &tv);
}

/* Accessors for plugin-mode.c (registry validation and vtable calls). */

uint64_t
window_plugin_mode_id(struct window_mode_entry *wme)
{
	struct window_plugin_mode_data	*data = wme->data;

	return (data->mode_id);
}

void
window_plugin_mode_set_close_reason(struct window_mode_entry *wme,
    const char *reason)
{
	struct window_plugin_mode_data	*data = wme->data;

	data->close_reason = reason;
}

/* Parse ANSI bytes into the mode screen. */
void
window_plugin_mode_write(struct window_mode_entry *wme, const u_char *buf,
    size_t len)
{
	struct window_plugin_mode_data	*data = wme->data;

	input_parse_screen(data->ictx, &data->screen, NULL, NULL, buf, len);
	wme->wp->flags |= PANE_REDRAW;
}

/*
 * Set (pane >= 0) or clear (pane < 0) the retained preview rect. Returns 0,
 * or -2 if the rect does not fit the current mode screen.
 */
int
window_plugin_mode_preview(struct window_mode_entry *wme, int64_t pane,
    u_int px, u_int py, u_int sx, u_int sy)
{
	struct window_plugin_mode_data	*data = wme->data;
	struct screen			*s = &data->screen;
	struct timeval			 tv = {
		.tv_sec = WINDOW_PLUGIN_REFRESH_MSEC / 1000,
		.tv_usec = (WINDOW_PLUGIN_REFRESH_MSEC % 1000) * 1000
	};

	if (pane < 0) {
		data->preview.set = 0;
		evtimer_del(&data->refresh);
		return (0);
	}

	if (px + sx > screen_size_x(s) || py + sy > screen_size_y(s))
		return (-2);

	data->preview.set = 1;
	data->preview.pane_id = (u_int)pane;
	data->preview.px = px;
	data->preview.py = py;
	data->preview.sx = sx;
	data->preview.sy = sy;

	window_plugin_draw_preview(wme);
	if (data->preview.set) {
		evtimer_del(&data->refresh);
		evtimer_add(&data->refresh, &tv);
	}
	return (0);
}
