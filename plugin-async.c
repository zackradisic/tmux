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
#include <sys/wait.h>

#include <event.h>
#include <stdlib.h>
#include <string.h>

#include "tmux.h"
#include "plugin-host.h"
#include "plugin-internal.h"

/*
 * Async host operations: jobs, tmux commands and timers. Every starter
 * returns immediately; the eventual result is delivered to the plugin host
 * with pgh_async_complete(token, ...) - which is enqueue-only, like
 * pgh_notify - followed by plugin_schedule_drain(). Generation checks on
 * the Rust side drop completions whose instance died or was reloaded, so
 * these callbacks never need to know about plugin lifecycles.
 */

struct plugin_job {
	uint64_t	 token;
	int		 completed;
};

struct plugin_run_command {
	uint64_t	 token;
};

struct plugin_timer {
	uint64_t		 id;
	uint64_t		 token;
	struct event		 ev;
	RB_ENTRY(plugin_timer)	 entry;
};
RB_HEAD(plugin_timers, plugin_timer);

static uint64_t			 plugin_timer_next_id = 1;
static struct plugin_timers	 plugin_timers = RB_INITIALIZER(&plugin_timers);

static int
plugin_timer_cmp(struct plugin_timer *a, struct plugin_timer *b)
{
	if (a->id < b->id)
		return (-1);
	if (a->id > b->id)
		return (1);
	return (0);
}
RB_GENERATE_STATIC(plugin_timers, plugin_timer, entry, plugin_timer_cmp);

/* Deliver a completion and wake the drain machinery. */
static void
plugin_async_done(uint64_t token, const char *json, int is_error)
{
	pgh_async_complete(token, json, is_error);
	plugin_schedule_drain();
}

/* Job complete: collect exit status and output, deliver as JSON. */
static void
plugin_job_complete(struct job *job)
{
	struct plugin_job	*pjob = job_get_data(job);
	struct bufferevent	*event = job_get_event(job);
	struct plugin_json	*pj;
	size_t			 size;
	char			*out;
	int			 status, retcode = -1, signalled = 0;

	status = job_get_status(job);
	if (WIFEXITED(status))
		retcode = WEXITSTATUS(status);
	else if (WIFSIGNALED(status)) {
		retcode = WTERMSIG(status);
		signalled = 1;
	}

	size = EVBUFFER_LENGTH(event->input);
	out = xmalloc(size + 1);
	memcpy(out, EVBUFFER_DATA(event->input), size);
	out[size] = '\0';

	pj = plugin_json_create();
	plugin_json_obj_start(pj, NULL);
	plugin_json_num(pj, "status", retcode);
	plugin_json_bool(pj, "signalled", signalled);
	plugin_json_str(pj, "output", out);
	plugin_json_obj_end(pj);
	free(out);

	pjob->completed = 1;
	plugin_async_done(pjob->token, plugin_json_string(pj), 0);
	plugin_json_free(pj);
}

/* Job freed: if it never completed (e.g. server shutdown), send an error. */
static void
plugin_job_free(void *data)
{
	struct plugin_job	*pjob = data;

	if (!pjob->completed) {
		plugin_async_done(pjob->token,
		    "{\"code\":\"E_CANCELLED\",\"message\":\"job cancelled\"}",
		    1);
	}
	free(pjob);
}

/*
 * Start a shell command as a job. Returns 0 on start, -1 on failure.
 * Completion JSON: {"status": n, "signalled": bool, "output": "..."}.
 */
int
plugin_vtable_run_job(const char *cmd, const char *cwd, uint64_t token)
{
	struct plugin_job	*pjob;

	pjob = xcalloc(1, sizeof *pjob);
	pjob->token = token;

	if (job_run(cmd, 0, NULL, NULL, NULL, cwd, NULL, plugin_job_complete,
	    plugin_job_free, pjob, JOB_NOWAIT, -1, -1) == NULL) {
		free(pjob);
		return (-1);
	}
	return (0);
}

/* Command-queue callback appended after a plugin-initiated command. */
static enum cmd_retval
plugin_run_command_done(__unused struct cmdq_item *item, void *data)
{
	struct plugin_run_command	*prc = data;

	plugin_async_done(prc->token, "{}", 0);
	free(prc);
	return (CMD_RETURN_NORMAL);
}

/*
 * Run a tmux command string through the command queue (never inline).
 * Runs with NOHOOKS to avoid hook recursion. Parse errors are delivered
 * asynchronously as an error completion; returns -1 only on internal
 * failure.
 */
int
plugin_vtable_run_command(const char *cmdstr, uint64_t token)
{
	struct cmd_parse_result		*pr;
	struct plugin_run_command	*prc;
	struct cmdq_state		*state;
	struct cmdq_item		*item;
	struct plugin_json		*pj;

	pr = cmd_parse_from_string(cmdstr, NULL);
	if (pr->status == CMD_PARSE_ERROR) {
		pj = plugin_json_create();
		plugin_json_obj_start(pj, NULL);
		plugin_json_str(pj, "code", "E_BAD_REQUEST");
		plugin_json_str(pj, "message", pr->error);
		plugin_json_obj_end(pj);
		plugin_async_done(token, plugin_json_string(pj), 1);
		plugin_json_free(pj);
		free(pr->error);
		return (0);
	}

	prc = xcalloc(1, sizeof *prc);
	prc->token = token;

	state = cmdq_new_state(NULL, NULL, CMDQ_STATE_NOHOOKS);
	item = cmdq_get_command(pr->cmdlist, state);
	cmdq_free_state(state);
	cmd_list_free(pr->cmdlist);

	cmdq_append(NULL, item);
	cmdq_append(NULL, cmdq_get_callback(plugin_run_command_done, prc));
	return (0);
}

static void
plugin_timer_fire(__unused int fd, __unused short events, void *data)
{
	struct plugin_timer	*pt = data;

	RB_REMOVE(plugin_timers, &plugin_timers, pt);
	plugin_async_done(pt->token, "{}", 0);
	free(pt);
}

/* One-shot timer; repeating intervals are re-armed by the Rust side. */
uint64_t
plugin_vtable_timer_start(uint64_t ms, uint64_t token)
{
	struct plugin_timer	*pt;
	struct timeval		 tv;

	pt = xcalloc(1, sizeof *pt);
	pt->id = plugin_timer_next_id++;
	pt->token = token;
	evtimer_set(&pt->ev, plugin_timer_fire, pt);
	tv.tv_sec = ms / 1000;
	tv.tv_usec = (ms % 1000) * 1000;
	evtimer_add(&pt->ev, &tv);
	RB_INSERT(plugin_timers, &plugin_timers, pt);
	return (pt->id);
}

/* Cancel a pending timer; no completion is delivered. 0 ok, -1 unknown. */
int
plugin_vtable_timer_cancel(uint64_t timer_id)
{
	struct plugin_timer	 find, *pt;

	find.id = timer_id;
	pt = RB_FIND(plugin_timers, &plugin_timers, &find);
	if (pt == NULL)
		return (-1);
	RB_REMOVE(plugin_timers, &plugin_timers, pt);
	evtimer_del(&pt->ev);
	free(pt);
	return (0);
}

/* Cancel all pending plugin timers (server shutdown). */
void
plugin_async_shutdown(void)
{
	struct plugin_timer	*pt, *pt1;

	RB_FOREACH_SAFE(pt, plugin_timers, &plugin_timers, pt1) {
		RB_REMOVE(plugin_timers, &plugin_timers, pt);
		evtimer_del(&pt->ev);
		free(pt);
	}
}
