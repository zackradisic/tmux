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
 * Plugin UI modes: registry and vtable entry points. A mode is a
 * window_plugin_mode entry on a freshly spawned empty floating pane
 * (window-plugin-mode.c implements the mode itself).
 *
 * Weak handles throughout: the registry maps a monotonic mode id to a pane
 * id, and every call revalidates through window_pane_find_by_id plus a
 * mode-entry match, so a pane killed or repurposed behind our back means
 * "no such mode", never a stale pointer.
 *
 * Reentrancy contract: the vtable handlers here run inside pgh_drain,
 * while the calling plugin instance is checked out of the host registry.
 * They may re-enter the enqueue-only pgh entry points (via
 * plugin_mode_event and the events fired by spawn/set-mode), but must
 * never destroy tmux objects synchronously: pane destruction re-enters
 * pgh_object_destroyed, which cannot see a checked-out instance and would
 * let an instance outlive its own scope object if that instance made the
 * call. mode_close therefore only unregisters and schedules the kill; a
 * zero-timeout evtimer (a fresh event-loop iteration, the established
 * safe point) does the actual server_kill_pane.
 *
 * The same reasoning is why open/write/preview are safe inline: spawning
 * an empty pane and parsing bytes only *create* objects and enqueue
 * events, they never tear anything down.
 */

struct plugin_mode {
	uint64_t		 id;
	u_int			 pane_id;
	RB_ENTRY(plugin_mode)	 entry;
};
RB_HEAD(plugin_modes, plugin_mode);

static uint64_t			plugin_mode_next_id = 1;
static struct plugin_modes	plugin_modes = RB_INITIALIZER(&plugin_modes);

static int
plugin_mode_cmp(struct plugin_mode *a, struct plugin_mode *b)
{
	if (a->id < b->id)
		return (-1);
	if (a->id > b->id)
		return (1);
	return (0);
}
RB_GENERATE_STATIC(plugin_modes, plugin_mode, entry, plugin_mode_cmp);

/*
 * The pending-open slot: window_pane_set_mode offers no way to pass
 * per-entry arguments through to init, so mode_open stashes the id here
 * immediately before the call and init consumes it. Single-threaded and
 * fully synchronous, so a static slot is safe; init refuses to run when
 * the slot is empty, which keeps every other path into the mode out.
 */
static struct {
	int		 active;
	uint64_t	 mode_id;
} plugin_mode_pending;

/* Deferred closes, run from a zero-timeout evtimer (see header comment). */
struct plugin_mode_close {
	uint64_t			 mode_id;
	u_int				 pane_id;
	TAILQ_ENTRY(plugin_mode_close)	 entry;
};
static TAILQ_HEAD(, plugin_mode_close) plugin_mode_closes =
    TAILQ_HEAD_INITIALIZER(plugin_mode_closes);
static struct event	plugin_mode_close_timer;
static int		plugin_mode_close_timer_set;

/* Consume the pending-open mode id (window_plugin_init only). */
int
plugin_mode_pending_take(uint64_t *mode_id)
{
	if (!plugin_mode_pending.active)
		return (0);
	plugin_mode_pending.active = 0;
	*mode_id = plugin_mode_pending.mode_id;
	return (1);
}

/* Enqueue a mode event and wake the drain machinery. */
void
plugin_mode_event(uint64_t mode_id, const char *name, const char *json)
{
	if (!plugin_enabled())
		return;
	pgh_mode_event(mode_id, name, json);
	plugin_schedule_drain();
}

/* Drop a registry entry (mode teardown; unknown ids are ignored). */
void
plugin_mode_unregister(uint64_t mode_id)
{
	struct plugin_mode	 find, *pm;

	find.id = mode_id;
	pm = RB_FIND(plugin_modes, &plugin_modes, &find);
	if (pm != NULL) {
		RB_REMOVE(plugin_modes, &plugin_modes, pm);
		free(pm);
	}
}

/*
 * Resolve a live mode id to its pane and mode entry. Only matches when our
 * mode is at the front of the pane's mode stack (a stacked copy-mode
 * temporarily hides the mode from write/preview).
 */
static struct window_pane *
plugin_mode_find(uint64_t mode_id, struct window_mode_entry **wme_out)
{
	struct plugin_mode		 find, *pm;
	struct window_pane		*wp;
	struct window_mode_entry	*wme;

	find.id = mode_id;
	pm = RB_FIND(plugin_modes, &plugin_modes, &find);
	if (pm == NULL)
		return (NULL);
	wp = window_pane_find_by_id(pm->pane_id);
	if (wp == NULL || (wp->flags & PANE_DESTROYED))
		return (NULL);
	wme = TAILQ_FIRST(&wp->modes);
	if (wme == NULL || wme->mode != &window_plugin_mode)
		return (NULL);
	if (window_plugin_mode_id(wme) != mode_id)
		return (NULL);
	if (wme_out != NULL)
		*wme_out = wme;
	return (wp);
}

/*
 * Open a mode: spawn an empty floating pane in the window, enter
 * window_plugin_mode on it and make it the active pane. Returns the new
 * mode id, or -1 (no such window), -2 (spawn failed), -3 (init failed).
 */
int64_t
plugin_vtable_mode_open(u_int window, u_int width, u_int height, int x,
    int y, const char *title)
{
	struct window			*w;
	struct winlink			*wl;
	struct session			*s;
	struct window_pane		*new_wp;
	struct layout_cell		*lc;
	struct layout_geometry		 lg;
	struct spawn_context		 sc;
	struct plugin_mode		*pm;
	char				*cause = NULL;
	uint64_t			 mode_id;
	u_int				 sx, sy;
	int				 border, xoff, yoff, rc;

	w = window_find_by_id(window);
	if (w == NULL)
		return (-1);
	wl = TAILQ_FIRST(&w->winlinks);
	if (wl == NULL)
		return (-1);
	s = wl->session;

	if (w->sx <= PANE_MINIMUM + 2 || w->sy <= PANE_MINIMUM + 2)
		return (-2);
	border = window_get_pane_lines(w) != PANE_LINES_NONE;

	/* Clamp the size so the float (and its border) fits the window. */
	sx = width;
	if (sx < PANE_MINIMUM)
		sx = PANE_MINIMUM;
	if (sx > w->sx - 2 * border)
		sx = w->sx - 2 * border;
	sy = height;
	if (sy < PANE_MINIMUM)
		sy = PANE_MINIMUM;
	if (sy > w->sy - 2 * border)
		sy = w->sy - 2 * border;

	/* Explicit offsets or centered, clamped in-bounds. */
	xoff = x >= 0 ? x + border : (int)(w->sx - sx) / 2;
	yoff = y >= 0 ? y + border : (int)(w->sy - sy) / 2;
	if (xoff < border)
		xoff = border;
	if (xoff + (int)sx > (int)w->sx - border)
		xoff = (int)w->sx - border - (int)sx;
	if (yoff < border)
		yoff = border;
	if (yoff + (int)sy > (int)w->sy - border)
		yoff = (int)w->sy - border - (int)sy;

	lg.sx = sx;
	lg.sy = sy;
	lg.xoff = xoff;
	lg.yoff = yoff;
	lc = layout_floating_pane(w, w->active, &lg);

	memset(&sc, 0, sizeof sc);
	sc.item = NULL; /* safe for empty panes: no format expansion, no
			 * fork (spawn.c) */
	sc.s = s;
	sc.wl = wl;
	sc.lc = lc;
	sc.idx = -1;
	sc.flags = SPAWN_EMPTY|SPAWN_FLOATING|SPAWN_DETACHED;

	new_wp = spawn_pane(&sc, &cause);
	if (new_wp == NULL) {
		log_debug("%s: spawn failed: %s", __func__, cause);
		free(cause);
		return (-2);
	}

	if (title != NULL)
		screen_set_title(&new_wp->base, title, 0);

	mode_id = plugin_mode_next_id++;
	plugin_mode_pending.active = 1;
	plugin_mode_pending.mode_id = mode_id;
	rc = window_pane_set_mode(new_wp, NULL, &window_plugin_mode, NULL,
	    NULL, NULL);
	plugin_mode_pending.active = 0;
	if (rc != 0) {
		/* Mirror the cmd-split-window failure path (floating panes
		 * skip layout_close_pane). */
		server_client_remove_pane(new_wp);
		window_remove_pane(w, new_wp);
		return (-3);
	}

	/*
	 * Deliberately NOT wme->kill: teardown always goes through the
	 * deferred server_kill_pane in plugin_vtable_mode_close (the kill
	 * flag would also fire from window_pane_reset_mode_all inside
	 * respawn-pane, killing the pane mid-spawn).
	 */

	pm = xcalloc(1, sizeof *pm);
	pm->id = mode_id;
	pm->pane_id = new_wp->id;
	RB_INSERT(plugin_modes, &plugin_modes, pm);

	/* Focus the float so keys flow to the mode immediately. */
	window_set_active_pane(w, new_wp, 1);
	server_redraw_window(w);

	return ((int64_t)mode_id);
}

/* Parse ANSI bytes into a mode's screen. 0 ok, -1 no such mode. */
int
plugin_vtable_mode_write(uint64_t mode_id, const u_char *buf, size_t len)
{
	struct window_mode_entry	*wme;

	if (plugin_mode_find(mode_id, &wme) == NULL)
		return (-1);
	window_plugin_mode_write(wme, buf, len);
	return (0);
}

/*
 * Set (pane >= 0) or clear (pane < 0) a mode's retained preview rect.
 * 0 ok, -1 no such mode, -2 rect does not fit.
 */
int
plugin_vtable_mode_preview(uint64_t mode_id, int64_t pane, u_int x, u_int y,
    u_int w, u_int h)
{
	struct window_mode_entry	*wme;

	if (plugin_mode_find(mode_id, &wme) == NULL)
		return (-1);
	return (window_plugin_mode_preview(wme, pane, x, y, w, h));
}

/* Run the deferred closes: the safe point for killing the mode panes. */
static void
plugin_mode_close_timer_callback(__unused int fd, __unused short events,
    __unused void *arg)
{
	struct plugin_mode_close	*pmc, *pmc1;
	struct window_pane		*wp;
	struct window_mode_entry	*wme;

	TAILQ_FOREACH_SAFE(pmc, &plugin_mode_closes, entry, pmc1) {
		TAILQ_REMOVE(&plugin_mode_closes, pmc, entry);
		wp = window_pane_find_by_id(pmc->pane_id);
		if (wp != NULL && (~wp->flags & PANE_DESTROYED)) {
			/*
			 * Only kill while our mode is still on the pane
			 * (anywhere in the stack - a copy-mode on top must
			 * not save it); if the pane was respawned into
			 * something else meanwhile, leave it alone.
			 */
			TAILQ_FOREACH(wme, &wp->modes, entry) {
				if (wme->mode == &window_plugin_mode &&
				    window_plugin_mode_id(wme) ==
				    pmc->mode_id) {
					server_kill_pane(wp);
					break;
				}
			}
		}
		free(pmc);
	}
}

/*
 * Close a mode. The registry entry goes away immediately (subsequent calls
 * see "no such mode") but the pane teardown is deferred to the event loop:
 * killing a pane inside a vtable call would re-enter pgh_object_destroyed
 * mid-drain. 0 ok, -1 no such mode.
 */
int
plugin_vtable_mode_close(uint64_t mode_id)
{
	struct plugin_mode		 find, *pm;
	struct plugin_mode_close	*pmc;
	struct window_pane		*wp;
	struct window_mode_entry	*wme;
	struct timeval			 tv = { 0, 0 };

	find.id = mode_id;
	pm = RB_FIND(plugin_modes, &plugin_modes, &find);
	if (pm == NULL)
		return (-1);
	RB_REMOVE(plugin_modes, &plugin_modes, pm);

	wp = window_pane_find_by_id(pm->pane_id);
	if (wp == NULL || (wp->flags & PANE_DESTROYED)) {
		free(pm);
		return (-1);
	}

	/* Anywhere in the stack: close works under a stacked copy-mode. */
	TAILQ_FOREACH(wme, &wp->modes, entry) {
		if (wme->mode == &window_plugin_mode &&
		    window_plugin_mode_id(wme) == mode_id) {
			window_plugin_mode_set_close_reason(wme, "closed");
			break;
		}
	}

	pmc = xcalloc(1, sizeof *pmc);
	pmc->mode_id = mode_id;
	pmc->pane_id = pm->pane_id;
	free(pm);
	TAILQ_INSERT_TAIL(&plugin_mode_closes, pmc, entry);

	if (!plugin_mode_close_timer_set) {
		plugin_mode_close_timer_set = 1;
		evtimer_set(&plugin_mode_close_timer,
		    plugin_mode_close_timer_callback, NULL);
	}
	if (!evtimer_pending(&plugin_mode_close_timer, NULL))
		evtimer_add(&plugin_mode_close_timer, &tv);
	return (0);
}

/* Server shutdown: drop bookkeeping (panes die with the server). */
void
plugin_mode_shutdown(void)
{
	struct plugin_mode		*pm, *pm1;
	struct plugin_mode_close	*pmc, *pmc1;

	RB_FOREACH_SAFE(pm, plugin_modes, &plugin_modes, pm1) {
		RB_REMOVE(plugin_modes, &plugin_modes, pm);
		free(pm);
	}
	TAILQ_FOREACH_SAFE(pmc, &plugin_mode_closes, entry, pmc1) {
		TAILQ_REMOVE(&plugin_mode_closes, pmc, entry);
		free(pmc);
	}
	if (plugin_mode_close_timer_set)
		evtimer_del(&plugin_mode_close_timer);
}
