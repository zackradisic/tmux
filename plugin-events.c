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
 * Bridge from the tmux event bus and object teardown to the plugin host.
 *
 * plugin_events_init() registers an event sink for every hookable event
 * (plus the paste-buffer events, which have no hook entries), so plugins
 * observe the same vocabulary as hooks and control mode - including events
 * added upstream in the future, with no bridge changes. Sinks run
 * synchronously inside events_fire() while every payload object is still
 * alive: the payload is snapshotted into a binary event buffer on the spot
 * and pgh_notify() is enqueue-only, so this is safe anywhere on the main
 * thread, however deep in tmux internals. Guest code runs later, at the
 * drain safe point.
 *
 * The canonical payload objects (client/session/window/pane) become the
 * event header's scope ids; their names travel as flat fields
 * (session_name, window_name, client_name). Everything else the payload
 * carries is forwarded flat (window_index, exit_status, command_duration,
 * old_pane, ...). Event names and field keys are interned through
 * pgh_intern - the same table guests use to subscribe and compare keys.
 *
 * plugin_notify() delivers events that have no bus equivalent: the OSC 9 /
 * OSC 777 pane-notification (from input.c).
 *
 * plugin_object_destroyed() is the authoritative death signal for scoped
 * plugin instances, called from the teardown paths in session.c, window.c
 * and server-client.c. It fires a synthesized "<kind>-destroyed" event for
 * other plugins, then invalidates the dead object's instances. The Rust
 * side only marks and queues here - guest on_unload runs at the next
 * drain, since these calls can come from deep teardown (e.g.
 * window_destroy runs inside command-queue callbacks when the last
 * reference drops). plugin_object_created() stays as well: the bus
 * *-created events fire from command paths (e.g. spawn) and miss panes
 * created directly via window_add_pane, so eager scoped-instance creation
 * keeps its own object-level signal.
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
static const int plugin_obj_pgh[] = {
	PGH_OBJ_SESSION,
	PGH_OBJ_WINDOW,
	PGH_OBJ_PANE,
	PGH_OBJ_CLIENT,
};

/* Bus events with no hook entry in the options table. */
static const char *plugin_events_extra[] = {
	"paste-buffer-changed",
	"paste-buffer-deleted",
};

/*
 * Bus events NOT bridged: creation is delivered by the synthesized
 * object-level events from plugin_object_created() instead (the bus
 * versions fire from command paths and miss direct window_add_pane
 * creation); bridging both would deliver duplicates.
 */
static const char *plugin_events_skip[] = {
	"client-created",
	"pane-created",
	"window-created",
};

static int
plugin_events_skipped(const char *name)
{
	u_int	i;

	for (i = 0; i < nitems(plugin_events_skip); i++) {
		if (strcmp(name, plugin_events_skip[i]) == 0)
			return (1);
	}
	return (0);
}

static struct events_sink	**plugin_sinks;
static u_int			  plugin_nsinks;

/* Item-walk state for one bridged event. */
struct plugin_events_state {
	struct plugin_buf	*pb;
	int			 pass;
	struct window		*window;
	int			 have_pane;
};

/*
 * Payload item -> event buffer. Pass 0 maps the canonical object items
 * (client/session/window/pane) to the header scope ids plus name fields;
 * pass 1 forwards everything else flat (window_index, exit_status,
 * command_duration, old_pane, ...) for the guest's event fields.
 */
static void
plugin_events_item(const char *name, const struct event_payload_value *epv,
    void *arg)
{
	struct plugin_events_state	*st = arg;
	struct plugin_buf		*pb = st->pb;

	if (st->pass == 0) {
		switch (epv->type) {
		case EVENT_PAYLOAD_CLIENT:
			if (strcmp(name, "client") == 0) {
				plugin_event_scope(pb, PGH_OBJ_CLIENT,
				    epv->client->id);
				if (epv->client->name != NULL)
					plugin_event_str(pb, "client_name",
					    epv->client->name);
			}
			break;
		case EVENT_PAYLOAD_SESSION:
			if (strcmp(name, "session") == 0) {
				plugin_event_scope(pb, PGH_OBJ_SESSION,
				    epv->session->id);
				plugin_event_str(pb, "session_name",
				    epv->session->name);
			}
			break;
		case EVENT_PAYLOAD_WINDOW:
			if (strcmp(name, "window") == 0) {
				st->window = epv->window;
				plugin_event_scope(pb, PGH_OBJ_WINDOW,
				    epv->window->id);
				plugin_event_str(pb, "window_name",
				    epv->window->name);
			}
			break;
		case EVENT_PAYLOAD_PANE:
			if (strcmp(name, "pane") == 0) {
				st->have_pane = 1;
				plugin_event_scope(pb, PGH_OBJ_PANE,
				    epv->pane->id);
				if (epv->pane->window != NULL &&
				    !plugin_event_scope_set(pb,
				    PGH_OBJ_WINDOW)) {
					plugin_event_scope(pb, PGH_OBJ_WINDOW,
					    epv->pane->window->id);
				}
			}
			break;
		default:
			break;
		}
		return;
	}

	switch (epv->type) {
	case EVENT_PAYLOAD_STRING:
		if (strcmp(name, "event") != 0)
			plugin_event_str(pb, name, epv->string);
		break;
	case EVENT_PAYLOAD_INT:
		plugin_event_i64(pb, name, epv->number);
		break;
	case EVENT_PAYLOAD_UINT:
		plugin_event_i64(pb, name, epv->unsigned_number);
		break;
	case EVENT_PAYLOAD_TIME:
		plugin_event_i64(pb, name, (long long)epv->time);
		break;
	case EVENT_PAYLOAD_CLIENT:
		if (strcmp(name, "client") != 0)
			plugin_event_i64(pb, name, epv->client->id);
		break;
	case EVENT_PAYLOAD_SESSION:
		if (strcmp(name, "session") != 0)
			plugin_event_i64(pb, name, epv->session->id);
		break;
	case EVENT_PAYLOAD_WINDOW:
		if (strcmp(name, "window") != 0)
			plugin_event_i64(pb, name, epv->window->id);
		break;
	case EVENT_PAYLOAD_PANE:
		if (strcmp(name, "pane") != 0)
			plugin_event_i64(pb, name, epv->pane->id);
		break;
	default:
		break;
	}
}

/* Event sink: snapshot the payload into an event buffer and enqueue. */
static void
plugin_events_sink(const char *name, struct event_payload *ep,
    __unused void *data)
{
	struct plugin_events_state	 st;

	if (!plugin_enabled())
		return;

	memset(&st, 0, sizeof st);
	st.pb = plugin_event_create(name);

	st.pass = 0;
	event_payload_foreach(ep, plugin_events_item, &st);

	/*
	 * session-window-changed carries only the window: attach its active
	 * pane so pane-scoped instances get their "you are now on display"
	 * signal. (window-pane-changed already carries the pane.)
	 */
	if (!st.have_pane && st.window != NULL && st.window->active != NULL &&
	    strcmp(name, "session-window-changed") == 0) {
		plugin_event_scope(st.pb, PGH_OBJ_PANE,
		    st.window->active->id);
	}

	st.pass = 1;
	event_payload_foreach(ep, plugin_events_item, &st);

	plugin_event_send(st.pb);
}

/* Register a sink for every hookable event plus the extras above. */
void
plugin_events_init(void)
{
	const struct options_table_entry	*oe;
	u_int					 i, n;

	n = nitems(plugin_events_extra);
	for (oe = options_table; oe->name != NULL; oe++)
		n++;
	plugin_sinks = xcalloc(n, sizeof *plugin_sinks);

	for (oe = options_table; oe->name != NULL; oe++) {
		if (~oe->flags & OPTIONS_TABLE_IS_HOOK)
			continue;
		if (strncmp(oe->name, "after-", 6) == 0)
			continue;	/* per-command noise, not state */
		if (plugin_events_skipped(oe->name))
			continue;
		plugin_sinks[plugin_nsinks++] = events_add_sink(oe->name,
		    plugin_events_sink, NULL);
	}
	for (i = 0; i < nitems(plugin_events_extra); i++) {
		plugin_sinks[plugin_nsinks++] = events_add_sink(
		    plugin_events_extra[i], plugin_events_sink, NULL);
	}
}

void
plugin_events_shutdown(void)
{
	u_int	i;

	for (i = 0; i < plugin_nsinks; i++)
		events_remove_sink(plugin_sinks[i]);
	free(plugin_sinks);
	plugin_sinks = NULL;
	plugin_nsinks = 0;
}

/*
 * Direct delivery for events with no bus equivalent (pane-notification
 * from OSC 9/777). Objects are the caller's responsibility to have live.
 */
void
plugin_notify(const char *name, struct client *c, struct session *s,
    struct window *w, struct window_pane *wp, const char *text)
{
	struct plugin_buf	*pb;

	if (!plugin_enabled())
		return;

	pb = plugin_event_create(name);
	if (c != NULL) {
		plugin_event_scope(pb, PGH_OBJ_CLIENT, c->id);
		if (c->name != NULL)
			plugin_event_str(pb, "client_name", c->name);
	}
	if (s != NULL) {
		plugin_event_scope(pb, PGH_OBJ_SESSION, s->id);
		plugin_event_str(pb, "session_name", s->name);
	}
	if (w != NULL) {
		plugin_event_scope(pb, PGH_OBJ_WINDOW, w->id);
		plugin_event_str(pb, "window_name", w->name);
	}
	if (wp != NULL) {
		plugin_event_scope(pb, PGH_OBJ_PANE, wp->id);
		/*
		 * Backfill the window from the pane, as the bus path does:
		 * a pane-scoped event must carry the window it sits in
		 * (notify-toast jumps to the source window by this id).
		 */
		if (w == NULL && wp->window != NULL) {
			plugin_event_scope(pb, PGH_OBJ_WINDOW,
			    wp->window->id);
			plugin_event_str(pb, "window_name",
			    wp->window->name);
		}
	}
	if (text != NULL)
		plugin_event_str(pb, "text", text);
	plugin_event_send(pb);
}

/*
 * Synthesized creation event (window-created, pane-created equivalents at
 * the object level): the bus events fire from command paths and can miss
 * objects created directly (e.g. window_add_pane from a popup); scoped
 * plugin instances are created eagerly when their object appears.
 * Sessions use the native session-created event instead.
 */
void
plugin_object_created(enum plugin_obj_kind kind, u_int id)
{
	struct plugin_buf	*pb;

	if (!plugin_enabled())
		return;

	pb = plugin_event_create(plugin_obj_created_event[kind]);
	plugin_event_scope(pb, plugin_obj_pgh[kind], id);
	plugin_event_send(pb);
}

void
plugin_object_destroyed(enum plugin_obj_kind kind, u_int id)
{
	struct plugin_buf	*pb;

	if (!plugin_enabled())
		return;

	/*
	 * Synthesized event for other plugins first; the dying object's own
	 * instances are invalidated by pgh_object_destroyed below, which
	 * also purges their queued deliveries.
	 */
	pb = plugin_event_create(plugin_obj_event[kind]);
	plugin_event_scope(pb, plugin_obj_pgh[kind], id);
	plugin_event_send(pb);

	pgh_object_destroyed(plugin_obj_pgh[kind], id);
	plugin_schedule_drain();
}
