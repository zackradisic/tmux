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
#include <string.h>

#include "tmux.h"
#include "plugin-host.h"
#include "plugin-internal.h"

/*
 * Glue between the tmux server and the Rust plugin host (plugin-host/).
 *
 * Threading and reentrancy contract (see plugin-host.h):
 *  - Everything here runs on the server main thread.
 *  - Plugin (guest) code only ever runs inside pgh_drain(), which is only
 *    called from a command-queue callback - the established safe point
 *    (server_loop() drains the command queues after every event batch).
 *  - Vtable callbacks passed to pgh_init() are only invoked from inside
 *    pgh_* calls; they may re-enter pgh_notify() but nothing else.
 *  - When pgh_drain() stops on budget with work remaining, continuation is
 *    scheduled through a zero-timeout timer, NOT by re-appending directly:
 *    server_loop() drains the command queue until empty, so a directly
 *    re-appended item would spin without returning to the event loop and
 *    starve pty/client IO.
 */

/* Per-slice drain budget (microseconds); 0 lets the host pick its default. */
#define PLUGIN_DRAIN_BUDGET_US 2000

static int		 plugin_initialized;
static int		 plugin_drain_scheduled;
static int		 plugin_in_drain;
static struct event	 plugin_drain_timer;

static enum cmd_retval	 plugin_drain_cb(struct cmdq_item *, void *);
static void		 plugin_drain_timer_cb(int, short, void *);

void
plugin_init(void)
{
	pgh_host_vtable	 vt;

	memset(&vt, 0, sizeof vt);
	vt.log = plugin_vtable_log;
	vt.list_objects = plugin_vtable_list_objects;
	vt.resolve_object = plugin_vtable_resolve_object;
	vt.send_keys = plugin_vtable_send_keys;
	vt.capture_pane = plugin_vtable_capture_pane;
	vt.get_option = plugin_vtable_get_option;
	vt.set_option = plugin_vtable_set_option;
	vt.display_message = plugin_vtable_display_message;
	vt.run_job = plugin_vtable_run_job;
	vt.run_command = plugin_vtable_run_command;
	vt.timer_start = plugin_vtable_timer_start;
	vt.timer_cancel = plugin_vtable_timer_cancel;
	vt.plugin_state_changed = plugin_vtable_state_changed;

	if (pgh_init(&vt) != 0) {
		log_debug("%s: plugin host failed to initialize", __func__);
		return;
	}
	evtimer_set(&plugin_drain_timer, plugin_drain_timer_cb, NULL);
	plugin_initialized = 1;
	plugin_events_init();
}

void
plugin_shutdown(void)
{
	if (!plugin_initialized)
		return;
	plugin_initialized = 0;
	plugin_events_shutdown();
	evtimer_del(&plugin_drain_timer);
	plugin_async_shutdown();
	pgh_shutdown();
}

int
plugin_enabled(void)
{
	return (plugin_initialized);
}

/*
 * Schedule a drain of the plugin event queue at the next safe point.
 * Idempotent; called after anything enqueues plugin work (pgh_notify,
 * async completions, plugin loads).
 */
void
plugin_schedule_drain(void)
{
	if (!plugin_initialized || plugin_drain_scheduled)
		return;
	plugin_drain_scheduled = 1;
	cmdq_append(NULL, cmdq_get_callback(plugin_drain_cb, NULL));
}

static enum cmd_retval
plugin_drain_cb(__unused struct cmdq_item *item, __unused void *data)
{
	u_int		 remaining;
	struct timeval	 tv = { 0, 0 };

	plugin_drain_scheduled = 0;
	if (!plugin_initialized)
		return (CMD_RETURN_NORMAL);
	if (plugin_in_drain) {
		/* Cannot happen via the command queue; guard regardless. */
		log_debug("%s: nested drain refused", __func__);
		return (CMD_RETURN_NORMAL);
	}

	plugin_in_drain = 1;
	remaining = pgh_drain(PLUGIN_DRAIN_BUDGET_US);
	plugin_in_drain = 0;

	/*
	 * Budget exhausted with work left: continue after one pass through
	 * the event loop so IO keeps flowing between slices.
	 */
	if (remaining > 0)
		evtimer_add(&plugin_drain_timer, &tv);
	return (CMD_RETURN_NORMAL);
}

static void
plugin_drain_timer_cb(__unused int fd, __unused short events,
    __unused void *data)
{
	plugin_schedule_drain();
}
