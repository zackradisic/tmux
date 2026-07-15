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

#include "tmux.h"
#include "plugin-host.h"
#include "plugin-internal.h"

/*
 * Bridge from tmux notifications and object teardown to the plugin host.
 *
 * plugin_notify() is called from notify_add() for every tmux notification
 * and snapshots ids/names into JSON immediately (objects are guaranteed live
 * at that point). pgh_notify() is enqueue-only, so this is safe anywhere on
 * the main thread, however deep in tmux internals.
 *
 * plugin_object_destroyed() is the authoritative death signal for scoped
 * plugin instances, called from the teardown paths in session.c, window.c
 * and server-client.c. It fires a synthesized "<kind>-destroyed" event for
 * other plugins, then invalidates the dead object's instances. The Rust side
 * only marks and queues here - guest on_unload runs at the next drain, since
 * these calls can come from deep teardown (e.g. window_destroy runs inside
 * command-queue callbacks when the last reference drops).
 */

static const char *plugin_obj_created_event[] = {
	"session-created-internal",	/* unused: native session-created */
	"window-created",
	"pane-created",
	"client-created",
};
static const char *plugin_obj_event[] = {
	"session-destroyed",
	"window-destroyed",
	"pane-destroyed",
	"client-destroyed",
};
static const char *plugin_obj_key[] = {
	"session",
	"window",
	"pane",
	"client",
};
static const int plugin_obj_pgh[] = {
	PGH_OBJ_SESSION,
	PGH_OBJ_WINDOW,
	PGH_OBJ_PANE,
	PGH_OBJ_CLIENT,
};

void
plugin_notify(const char *name, struct client *c, struct session *s,
    struct window *w, struct window_pane *wp, const char *pbname)
{
	struct plugin_json	*pj;

	if (!plugin_enabled())
		return;

	pj = plugin_json_create();
	plugin_json_obj_start(pj, NULL);
	plugin_json_str(pj, "event", name);
	if (c != NULL) {
		plugin_json_obj_start(pj, "client");
		plugin_json_num(pj, "id", c->id);
		if (c->name != NULL)
			plugin_json_str(pj, "name", c->name);
		plugin_json_obj_end(pj);
	}
	if (s != NULL) {
		plugin_json_obj_start(pj, "session");
		plugin_json_num(pj, "id", s->id);
		plugin_json_str(pj, "name", s->name);
		plugin_json_obj_end(pj);
	}
	if (w != NULL) {
		plugin_json_obj_start(pj, "window");
		plugin_json_num(pj, "id", w->id);
		plugin_json_str(pj, "name", w->name);
		plugin_json_obj_end(pj);
	}
	if (wp != NULL) {
		plugin_json_obj_start(pj, "pane");
		plugin_json_num(pj, "id", wp->id);
		if (wp->window != NULL)
			plugin_json_num(pj, "window", wp->window->id);
		plugin_json_obj_end(pj);
	}
	if (pbname != NULL)
		plugin_json_str(pj, "pbname", pbname);
	plugin_json_obj_end(pj);

	pgh_notify(plugin_json_string(pj));
	plugin_json_free(pj);
	plugin_schedule_drain();
}

/*
 * Synthesized creation event (window-created, pane-created): tmux has no
 * native notification for these and scoped plugin instances are created
 * eagerly when their object appears. Sessions use the native
 * session-created notification instead.
 */
void
plugin_object_created(enum plugin_obj_kind kind, u_int id)
{
	struct plugin_json	*pj;

	if (!plugin_enabled())
		return;

	pj = plugin_json_create();
	plugin_json_obj_start(pj, NULL);
	plugin_json_str(pj, "event", plugin_obj_created_event[kind]);
	plugin_json_obj_start(pj, plugin_obj_key[kind]);
	plugin_json_num(pj, "id", id);
	plugin_json_obj_end(pj);
	plugin_json_obj_end(pj);

	pgh_notify(plugin_json_string(pj));
	plugin_json_free(pj);
	plugin_schedule_drain();
}

void
plugin_object_destroyed(enum plugin_obj_kind kind, u_int id)
{
	struct plugin_json	*pj;

	if (!plugin_enabled())
		return;

	/*
	 * Synthesized event for other plugins first; the dying object's own
	 * instances are invalidated by pgh_object_destroyed below, which
	 * also purges their queued deliveries.
	 */
	pj = plugin_json_create();
	plugin_json_obj_start(pj, NULL);
	plugin_json_str(pj, "event", plugin_obj_event[kind]);
	plugin_json_obj_start(pj, plugin_obj_key[kind]);
	plugin_json_num(pj, "id", id);
	plugin_json_obj_end(pj);
	plugin_json_obj_end(pj);

	pgh_notify(plugin_json_string(pj));
	plugin_json_free(pj);

	pgh_object_destroyed(plugin_obj_pgh[kind], id);
	plugin_schedule_drain();
}
