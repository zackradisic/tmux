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
#include <sys/socket.h>
#include <sys/wait.h>
#include <netinet/in.h>
#include <resolv.h>

#include <ctype.h>
#include <errno.h>
#include <limits.h>
#include <event.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"

/*
 * A link mirrors one session of a remote tmux server as a local shadow
 * session. The link runs "ssh host tmux -C attach" as a job and speaks the
 * control mode protocol on its stdin and stdout. Each remote window becomes
 * a local window and each remote pane a local pane whose descriptor is one
 * end of a socketpair; the link writes %output bytes into the other end and
 * reads the keys the pane writes, which it forwards with send-keys.
 *
 * The remote owns the tiled layout. Structural commands that target a shadow
 * object are sent to the remote (see remote_link_forward()) and the remote's
 * %layout-change brings the local tree in line.
 */

/* Field separator for -F formats: never appears in an id or a layout. */
#define REMOTE_SEP "\037"
#define REMOTE_SEP_CHAR '\037'

/* Format for one window, in list-windows and display-message replies. */
#define REMOTE_WINDOW_FORMAT \
	"#{window_id}" REMOTE_SEP "#{window_index}" REMOTE_SEP \
	"#{window_active}" REMOTE_SEP "#{window_layout}" REMOTE_SEP \
	"#{window_name}"

/* Format for one pane, in list-panes replies. */
#define REMOTE_PANE_FORMAT "#{pane_id}" REMOTE_SEP "#{pane_active}"

/* Bytes of keys per send-keys command. */
#define REMOTE_KEY_CHUNK 256

/* Reconnect backoff limit in seconds. */
#define REMOTE_BACKOFF_MAX 60

/* Number of seconds a paused pane may fall behind before the remote pauses. */
#define REMOTE_PAUSE_AFTER 5

/* Window index of the placeholder, out of the way of remote indices. */
#define REMOTE_PLACEHOLDER_INDEX 999999

/*
 * Bit set in the plugin bridge peer id of a link, so the host can tell a
 * link this server made from a control client that reached it. Must match
 * PGH_PEER_LINK in plugin-host.h.
 */
#define REMOTE_LINK_PEER_BIT 0x80000000U

struct remote_link;
struct remote_request;

typedef void (*remote_request_cb)(struct remote_link *,
	    struct remote_request *, int, const char *);

/* A command sent to the remote whose reply block is still to come. */
struct remote_request {
	char				*cmd;
	remote_request_cb		 cb;
	u_int				 arg;
	struct cmdq_item		*item;

	TAILQ_ENTRY(remote_request)	 entry;
};
TAILQ_HEAD(remote_requests, remote_request);

/* A remote pane and its local shadow. */
struct remote_pane {
	u_int			 remote_id;
	struct window_pane	*wp;

	int			 fd;		/* our end of the socketpair */
	struct bufferevent	*event;		/* reads keys, writes output */

	int			 paused;
	int			 awaiting_capture;

	char			*cache[4];	/* REMOTE_CACHE_* */

	RB_ENTRY(remote_pane)	 entry;
};
RB_HEAD(remote_panes, remote_pane);

/* A remote window and its local shadow. */
struct remote_window {
	u_int			 remote_id;
	struct window		*w;

	u_int			 sent_sx;	/* last size sent with -C */
	u_int			 sent_sy;

	int			 seen;		/* used while reconciling */
	int			 want_active;	/* pane id not yet seen, or -1 */

	RB_ENTRY(remote_window)	 entry;
};
RB_HEAD(remote_windows, remote_window);

enum remote_state {
	REMOTE_CONNECTING,
	REMOTE_SYNCING,
	REMOTE_UP,
	REMOTE_DOWN
};

struct remote_link {
	u_int			 id;

	char			*host;
	char			*remote_session;	/* as given, or NULL */
	char			*remote_cwd;		/* -c for a new session */
	char			*menu_client;		/* client that ran remote-attach */
	int			 may_create;		/* new-session -A until synced */
	u_int			 remote_session_id;
	int			 have_session_id;

	struct job		*job;
	struct remote_parser	*parser;
	enum remote_state	 state;

	struct session		*s;
	int			 placeholder_id;	/* window id, or -1 */

	struct remote_windows	 windows;
	struct remote_panes	 panes;
	struct remote_requests	 requests;
	struct remote_request	*cur;		/* request for the open block */
	int			 skip_block;	/* open block is not ours */
	int			 unsolicited;

	u_int			 backoff;
	struct event		 retry_timer;
	struct event		 defer_timer;
	int			 dying;
	int			 want_disconnect;
	int			 applying;	/* inside a remote notification */
	int			 synced_once;
	int			 bridge_unsupported; /* remote has no plugin-bridge */

	/*
	 * The last failure while not connected: an ssh error line, the
	 * remote's reply to attach, or the exit status. Shown in the
	 * placeholder window, the message log and the remote_error format.
	 */
	char			*last_error;
	int			 error_this_try;
	char			*reported_error;

	TAILQ_ENTRY(remote_link) entry;
};
TAILQ_HEAD(remote_links, remote_link);

static struct remote_links	remote_links = TAILQ_HEAD_INITIALIZER(
				    remote_links);
static u_int			next_remote_link_id = 1;

static int	remote_pane_cmp(struct remote_pane *, struct remote_pane *);
static int	remote_window_cmp(struct remote_window *,
		    struct remote_window *);
RB_GENERATE_STATIC(remote_panes, remote_pane, entry, remote_pane_cmp);
RB_GENERATE_STATIC(remote_windows, remote_window, entry, remote_window_cmp);

static void	remote_link_connect(struct remote_link *);
static void	remote_link_disconnect(struct remote_link *);
static void	remote_link_schedule_retry(struct remote_link *);
static void	remote_link_start_sync(struct remote_link *);
static void	remote_link_request_capture(struct remote_link *,
		    struct remote_pane *);
static void	remote_link_apply_layout(struct remote_link *,
		    struct remote_window *, const char *);
static void	remote_link_pane_read_callback(struct bufferevent *, void *);
static void	remote_link_pane_error_callback(struct bufferevent *, short,
		    void *);

static int
remote_pane_cmp(struct remote_pane *a, struct remote_pane *b)
{
	if (a->remote_id < b->remote_id)
		return (-1);
	if (a->remote_id > b->remote_id)
		return (1);
	return (0);
}

static int
remote_window_cmp(struct remote_window *a, struct remote_window *b)
{
	if (a->remote_id < b->remote_id)
		return (-1);
	if (a->remote_id > b->remote_id)
		return (1);
	return (0);
}

/* Lookups. */

static struct remote_pane *
remote_link_find_pane(struct remote_link *rl, u_int id)
{
	struct remote_pane	rp = { .remote_id = id };

	return (RB_FIND(remote_panes, &rl->panes, &rp));
}

static struct remote_window *
remote_link_find_window(struct remote_link *rl, u_int id)
{
	struct remote_window	rw = { .remote_id = id };

	return (RB_FIND(remote_windows, &rl->windows, &rw));
}

static struct remote_pane *
remote_link_pane_of(struct window_pane *wp)
{
	if (wp == NULL || wp->remote == NULL)
		return (NULL);
	return (remote_link_find_pane(wp->remote->link, wp->remote->remote_id));
}

static struct remote_window *
remote_link_window_of(struct window *w)
{
	if (w == NULL || w->remote == NULL)
		return (NULL);
	return (remote_link_find_window(w->remote->link, w->remote->remote_id));
}

static struct remote_ref *
remote_ref_new(struct remote_link *rl, u_int id)
{
	struct remote_ref	*ref;

	ref = xcalloc(1, sizeof *ref);
	ref->link = rl;
	ref->remote_id = id;
	return (ref);
}

/* Public accessors. */

u_int
remote_link_id(struct remote_link *rl)
{
	return (rl->id);
}

/* Record the client that ran remote-attach, for the peer grant menu. */
void
remote_link_set_menu_client(struct remote_link *rl, const char *name)
{
	free(rl->menu_client);
	rl->menu_client = name != NULL ? xstrdup(name) : NULL;
}

const char *
remote_link_menu_client(struct remote_link *rl)
{
	return (rl->menu_client);
}

const char *
remote_link_host(struct remote_link *rl)
{
	return (rl->host);
}

const char *
remote_link_remote_session(struct remote_link *rl)
{
	return (rl->remote_session);
}

struct session *
remote_link_session(struct remote_link *rl)
{
	return (rl->s);
}

int
remote_link_connected(struct remote_link *rl)
{
	return (rl->state == REMOTE_UP);
}

/* The last failure text, or "" while the link is up or untried. */
const char *
remote_link_error(struct remote_link *rl)
{
	return (rl->last_error != NULL ? rl->last_error : "");
}

/* One word for the status line: connecting, connected or disconnected. */
const char *
remote_link_state_name(struct remote_link *rl)
{
	switch (rl->state) {
	case REMOTE_CONNECTING:
	case REMOTE_SYNCING:
		return ("connecting");
	case REMOTE_UP:
		return ("connected");
	case REMOTE_DOWN:
		return ("disconnected");
	}
	return ("");
}

/* Remember a failure text, without its trailing line break. */
static void
remote_link_set_error(struct remote_link *rl, const char *text)
{
	size_t	len;

	if (text == NULL)
		return;
	len = strlen(text);
	while (len > 0 && (text[len - 1] == '\n' || text[len - 1] == '\r'))
		len--;
	if (len == 0)
		return;
	free(rl->last_error);
	rl->last_error = xstrndup(text, len);
	rl->error_this_try = 1;
	log_debug("%s: %s: %s", __func__, rl->host, rl->last_error);
}

/*
 * Show why the link is not up. Before the first sync the session has only
 * the placeholder window: put the text into its name and its pane, since
 * there are no shadow panes to write to. Each new text goes to the message
 * log once.
 */
static void
remote_link_report_error(struct remote_link *rl)
{
	struct window	*w;
	char		*name, *text;
	int		 fresh;

	if (rl->last_error == NULL)
		return;
	fresh = (rl->reported_error == NULL ||
	    strcmp(rl->reported_error, rl->last_error) != 0);
	if (fresh) {
		free(rl->reported_error);
		rl->reported_error = xstrdup(rl->last_error);
		server_add_message("remote %s: %s", rl->host, rl->last_error);
	}
	if (rl->placeholder_id == -1)
		return;
	w = window_find_by_id(rl->placeholder_id);
	if (w == NULL)
		return;
	xasprintf(&name, "connecting to %s: %s", rl->host, rl->last_error);
	window_set_name(w, name, 0);
	free(name);
	/* The pane gets each new text once; a retry with the same one adds
	 * nothing. */
	if (fresh && w->active != NULL) {
		xasprintf(&text, "\r\n[remote: %s: %s]\r\n", rl->host,
		    rl->last_error);
		input_parse_buffer(w->active, text, strlen(text));
		free(text);
	}
	server_redraw_window(w);
}

struct remote_link *
remote_link_first(void)
{
	return (TAILQ_FIRST(&remote_links));
}

struct remote_link *
remote_link_next(struct remote_link *rl)
{
	return (TAILQ_NEXT(rl, entry));
}

struct remote_link *
remote_link_find_by_id(u_int id)
{
	struct remote_link	*rl;

	TAILQ_FOREACH(rl, &remote_links, entry) {
		if (rl->id == id)
			return (rl);
	}
	return (NULL);
}

/* Find a link by host and remote session name (NULL for the default). */
struct remote_link *
remote_link_find(const char *host, const char *session)
{
	struct remote_link	*rl;

	TAILQ_FOREACH(rl, &remote_links, entry) {
		if (strcmp(rl->host, host) != 0)
			continue;
		if (session == NULL) {
			if (rl->remote_session == NULL)
				return (rl);
			continue;
		}
		if (rl->remote_session != NULL &&
		    strcmp(rl->remote_session, session) == 0)
			return (rl);
	}
	return (NULL);
}

/* Cached remote value for a pane format, or NULL. */
const char *
remote_link_pane_cache(struct window_pane *wp, int what)
{
	struct remote_pane	*rp = remote_link_pane_of(wp);

	if (rp == NULL || what < 0 || what >= 4)
		return (NULL);
	return (rp->cache[what]);
}

/* Sending. */

/* Queue a request. The reply block pops it in order. */
static struct remote_request *
remote_link_request_new(struct remote_link *rl, const char *cmd,
    remote_request_cb cb, u_int arg, struct cmdq_item *item)
{
	struct remote_request	*req;

	req = xcalloc(1, sizeof *req);
	req->cmd = xstrdup(cmd);
	req->cb = cb;
	req->arg = arg;
	req->item = item;
	TAILQ_INSERT_TAIL(&rl->requests, req, entry);
	return (req);
}

static void
remote_link_request_free(struct remote_request *req)
{
	free(req->cmd);
	free(req);
}

/* Write one command line to the remote. */
static void
remote_link_write(struct remote_link *rl, const char *cmd)
{
	struct bufferevent	*bev;

	if (rl->job == NULL)
		return;
	bev = job_get_event(rl->job);
	log_debug("%s: %s: %s", __func__, rl->host, cmd);
	bufferevent_write(bev, cmd, strlen(cmd));
	bufferevent_write(bev, "\n", 1);
}

/*
 * Send a command and queue its request. Returns NULL when the link is not
 * connected; the command line must be one command and must not contain a
 * newline, or the reply blocks would not match the request queue.
 */
static struct remote_request *
remote_link_send(struct remote_link *rl, remote_request_cb cb, u_int arg,
    struct cmdq_item *item, const char *fmt, ...)
{
	struct remote_request	*req;
	va_list			 ap;
	char			*cmd;

	if (rl->job == NULL || rl->state == REMOTE_DOWN)
		return (NULL);

	va_start(ap, fmt);
	xvasprintf(&cmd, fmt, ap);
	va_end(ap);

	if (strchr(cmd, '\n') != NULL) {
		log_debug("%s: %s: newline in command", __func__, rl->host);
		free(cmd);
		return (NULL);
	}
	req = remote_link_request_new(rl, cmd, cb, arg, item);
	remote_link_write(rl, cmd);
	free(cmd);
	return (req);
}

/* Fail every pending request. */
static void
remote_link_fail_requests(struct remote_link *rl, const char *why)
{
	struct remote_request	*req, *req1;

	TAILQ_FOREACH_SAFE(req, &rl->requests, entry, req1) {
		TAILQ_REMOVE(&rl->requests, req, entry);
		if (req->item != NULL) {
			cmdq_error(req->item, "%s", why);
			cmdq_continue(req->item);
			req->item = NULL;
		}
		remote_link_request_free(req);
	}
	rl->cur = NULL;
	rl->skip_block = 0;
	rl->unsolicited = 0;
}

/* Pane shadows. */

/* Write bytes into a shadow pane as if the remote process had printed them. */
static void
remote_link_pane_write(struct remote_pane *rp, const void *data, size_t len)
{
	if (rp->event != NULL && len != 0)
		bufferevent_write(rp->event, data, len);
}

static void
remote_link_pane_printf(struct remote_pane *rp, const char *fmt, ...)
{
	va_list	 ap;
	char	*s;
	size_t	 len;

	va_start(ap, fmt);
	len = xvasprintf(&s, fmt, ap);
	va_end(ap);
	remote_link_pane_write(rp, s, len);
	free(s);
}

/* Free the link side of a pane. The window pane itself is left alone. */
static void
remote_link_pane_free(struct remote_link *rl, struct remote_pane *rp)
{
	u_int	i;

	RB_REMOVE(remote_panes, &rl->panes, rp);
	if (rp->event != NULL)
		bufferevent_free(rp->event);
	if (rp->fd != -1)
		close(rp->fd);
	for (i = 0; i < 4; i++)
		free(rp->cache[i]);
	free(rp);
}

/*
 * Create a shadow pane for a remote pane in a shadow window. The pane is
 * appended to the window's list without a layout cell; the caller applies
 * the layout afterwards, which assigns cells in list order.
 */
static struct remote_pane *
remote_link_add_pane(struct remote_link *rl, struct remote_window *rw,
    u_int remote_id)
{
	struct remote_pane	*rp;
	struct spawn_context	 sc;
	struct window_pane	*wp;
	struct winlink		*wl;
	char			*cause = NULL;
	int			 sp[2];

	wl = winlink_find_by_window(&rl->s->windows, rw->w);
	if (wl == NULL)
		return (NULL);

	if (socketpair(AF_UNIX, SOCK_STREAM, PF_UNSPEC, sp) != 0) {
		log_debug("%s: socketpair: %s", __func__, strerror(errno));
		return (NULL);
	}

	memset(&sc, 0, sizeof sc);
	sc.s = rl->s;
	sc.wl = wl;
	sc.idx = -1;
	sc.adopt_fd = sp[0];
	sc.adopt_pid = -1;
	sc.adopt_tty = "";
	sc.flags = SPAWN_ADOPT|SPAWN_DETACHED|SPAWN_NONOTIFY;

	wp = spawn_pane(&sc, &cause);
	if (wp == NULL) {
		log_debug("%s: %%%u: %s", __func__, remote_id, cause);
		free(cause);
		close(sp[0]);
		close(sp[1]);
		return (NULL);
	}
	if (wp->shell == NULL) {
		wp->shell = xstrdup(options_get_string(rl->s->options,
		    "default-shell"));
	}
	wp->remote = remote_ref_new(rl, remote_id);
	wp->flags |= PANE_REMOTE;
	options_set_number(wp->options, "remain-on-exit", 1);

	rp = xcalloc(1, sizeof *rp);
	rp->remote_id = remote_id;
	rp->wp = wp;
	rp->fd = sp[1];
	setblocking(rp->fd, 0);
	rp->event = bufferevent_new(rp->fd, remote_link_pane_read_callback,
	    NULL, remote_link_pane_error_callback, rp);
	if (rp->event == NULL)
		fatalx("out of memory");
	bufferevent_enable(rp->event, EV_READ|EV_WRITE);
	rp->awaiting_capture = 1;
	RB_INSERT(remote_panes, &rl->panes, rp);

	log_debug("%s: %s: %%%u mirrors %%%u", __func__, rl->host, wp->id,
	    remote_id);
	return (rp);
}

/* Keys written by the local pane arrive here and go out as send-keys. */
static void
remote_link_pane_read_callback(__unused struct bufferevent *bufev, void *data)
{
	struct remote_pane	*rp = data;
	struct remote_link	*rl = rp->wp->remote->link;
	struct evbuffer		*evb = rp->event->input;
	u_char			*p;
	size_t			 len, n, i, cmdlen;
	char			*cmd;

	if (rl->state != REMOTE_UP) {
		evbuffer_drain(evb, EVBUFFER_LENGTH(evb));
		return;
	}
	while ((len = EVBUFFER_LENGTH(evb)) != 0) {
		n = len;
		if (n > REMOTE_KEY_CHUNK)
			n = REMOTE_KEY_CHUNK;
		p = EVBUFFER_DATA(evb);

		cmdlen = xasprintf(&cmd, "send-keys -H -t %%%u", rp->remote_id);
		cmd = xrealloc(cmd, cmdlen + n * 3 + 1);
		for (i = 0; i < n; i++) {
			cmdlen += xsnprintf(cmd + cmdlen, 4, " %02x", p[i]);
		}
		remote_link_request_new(rl, cmd, NULL, 0, NULL);
		remote_link_write(rl, cmd);
		free(cmd);
		evbuffer_drain(evb, n);
	}
}

static void
remote_link_pane_error_callback(__unused struct bufferevent *bufev,
    __unused short what, void *data)
{
	struct remote_pane	*rp = data;

	log_debug("%s: %%%u", __func__, rp->remote_id);
}

/* Reply to capture-pane: clear the grid and write the remote's lines. */
static void
remote_link_capture_cb(struct remote_link *rl, struct remote_request *req,
    int error, const char *body)
{
	struct remote_pane	*rp = remote_link_find_pane(rl, req->arg);
	const char		*p, *nl;

	if (rp == NULL)
		return;
	if (error) {
		log_debug("%s: %%%u: %s", __func__, req->arg, body);
		rp->awaiting_capture = 0;
		return;
	}
	remote_link_pane_write(rp, "\033[0m\033[H\033[2J\033[3J", 15);
	for (p = body; *p != '\0'; p = nl + 1) {
		nl = strchr(p, '\n');
		if (nl == NULL) {
			remote_link_pane_write(rp, p, strlen(p));
			break;
		}
		remote_link_pane_write(rp, p, nl - p);
		remote_link_pane_write(rp, "\r\n", 2);
	}
}

/* Reply to the cursor query that follows a capture. */
static void
remote_link_cursor_cb(struct remote_link *rl, struct remote_request *req,
    int error, const char *body)
{
	struct remote_pane	*rp = remote_link_find_pane(rl, req->arg);
	u_int			 cx, cy;

	if (rp == NULL)
		return;
	if (!error && sscanf(body, "%u %u", &cx, &cy) == 2)
		remote_link_pane_printf(rp, "\033[%u;%uH", cy + 1, cx + 1);
	rp->awaiting_capture = 0;
}

/*
 * Ask for the pane's contents. Output for the pane is dropped until the
 * cursor reply arrives: the remote emits blocks in order, so everything
 * dropped is already in the capture.
 */
static void
remote_link_request_capture(struct remote_link *rl, struct remote_pane *rp)
{
	rp->awaiting_capture = 1;
	if (remote_link_send(rl, remote_link_capture_cb, rp->remote_id, NULL,
	    "capture-pane -p -e -J -S - -t %%%u", rp->remote_id) == NULL) {
		rp->awaiting_capture = 0;
		return;
	}
	remote_link_send(rl, remote_link_cursor_cb, rp->remote_id, NULL,
	    "display-message -p -t %%%u '#{cursor_x} #{cursor_y}'",
	    rp->remote_id);
}

/* Window shadows. */

/* Read the root size from a layout string with checksum. */
static int
remote_link_layout_size(const char *layout, u_int *sx, u_int *sy)
{
	if (sscanf(layout, "%*4x,%ux%u", sx, sy) != 2)
		return (-1);
	if (*sx == 0 || *sy == 0)
		return (-1);
	return (0);
}

static void
remote_link_window_free(struct remote_link *rl, struct remote_window *rw)
{
	RB_REMOVE(remote_windows, &rl->windows, rw);
	free(rw);
}

/* A local floating pane taken out of a shadow window during a parse. */
struct remote_float {
	struct window_pane	*wp;
	struct layout_geometry	 g;
};

/*
 * Put the shadow panes into leaf order. Local floating panes are taken out
 * of the list so that layout_parse() sees only the tiled panes; the caller
 * puts them back with remote_link_restore_floats().
 */
static void
remote_link_order_panes(struct remote_link *rl, struct window *w,
    const u_int *ids, u_int n)
{
	struct window_panes	 tmp;
	struct window_pane	*wp, *wp1;
	struct remote_pane	*rp;
	u_int			 i;

	TAILQ_INIT(&tmp);
	TAILQ_FOREACH_SAFE(wp, &w->panes, entry, wp1) {
		TAILQ_REMOVE(&w->panes, wp, entry);
		if (wp->remote == NULL && window_pane_is_floating(wp))
			continue;
		TAILQ_INSERT_TAIL(&tmp, wp, entry);
	}
	for (i = 0; i < n; i++) {
		rp = remote_link_find_pane(rl, ids[i]);
		if (rp == NULL || rp->wp->window != w)
			continue;
		TAILQ_FOREACH(wp, &tmp, entry) {
			if (wp == rp->wp)
				break;
		}
		if (wp == NULL)
			continue;
		TAILQ_REMOVE(&tmp, wp, entry);
		TAILQ_INSERT_TAIL(&w->panes, wp, entry);
	}
	TAILQ_FOREACH_SAFE(wp, &tmp, entry, wp1) {
		TAILQ_REMOVE(&tmp, wp, entry);
		TAILQ_INSERT_TAIL(&w->panes, wp, entry);
	}
}

/*
 * Collect the local floating panes of a shadow window, top first, with the
 * geometry layout_parse() is about to free.
 */
static struct remote_float *
remote_link_collect_floats(struct window *w, u_int *n)
{
	struct remote_float	*floats = NULL;
	struct window_pane	*wp;

	*n = 0;
	TAILQ_FOREACH(wp, &w->z_index, zentry) {
		if (wp->remote != NULL || !window_pane_is_floating(wp))
			continue;
		floats = xreallocarray(floats, *n + 1, sizeof *floats);
		floats[*n].wp = wp;
		floats[*n].g = wp->layout_cell->g;
		(*n)++;
	}
	return (floats);
}

/*
 * Give floating panes back their cells and their place on top of the tiled
 * panes after layout_parse() rebuilt the tree and the z-order without them.
 */
static void
remote_link_restore_floats(struct window *w, struct remote_float *floats,
    u_int n)
{
	struct window_pane	*wp, *last = NULL, *loop;
	struct layout_cell	*lc;
	u_int			 i;

	TAILQ_FOREACH(loop, &w->panes, entry) {
		if (loop->layout_cell != NULL)
			last = loop;
	}
	for (i = 0; i < n; i++) {
		wp = floats[i].wp;
		TAILQ_INSERT_TAIL(&w->panes, wp, entry);
		if (last == NULL)
			continue;
		lc = layout_floating_pane(w, last, &floats[i].g);
		if (lc == NULL)
			continue;
		layout_assign_pane(lc, wp, 1);
	}
	for (i = n; i > 0; i--)
		TAILQ_INSERT_HEAD(&w->z_index, floats[i - 1].wp, zentry);
}

/*
 * Bring a shadow window in line with a remote layout string (without the
 * floating part). New leaf ids get panes and a capture, gone ids lose their
 * panes, then the panes are ordered like the leaves and the layout parsed.
 */
static void
remote_link_apply_layout(struct remote_link *rl, struct remote_window *rw,
    const char *stripped)
{
	struct window		*w = rw->w;
	struct window_pane	*wp, *wp1;
	struct remote_float	*floats;
	struct remote_pane	*rp, **added = NULL;
	u_int			*ids, n, i, nadded = 0, nfloats, sx, sy;
	char			*cause = NULL;
	int			 gone;

	ids = remote_parse_layout_leaf_ids(stripped, &n);
	if (n == 0) {
		free(ids);
		return;
	}
	rl->applying++;

	/* New panes first, so that killing old ones never empties the window. */
	for (i = 0; i < n; i++) {
		if (remote_link_find_pane(rl, ids[i]) != NULL)
			continue;
		rp = remote_link_add_pane(rl, rw, ids[i]);
		if (rp == NULL)
			continue;
		added = xreallocarray(added, nadded + 1, sizeof *added);
		added[nadded++] = rp;
	}

	/* Panes that the layout no longer lists. */
	TAILQ_FOREACH_SAFE(wp, &w->panes, entry, wp1) {
		if (wp->remote == NULL || wp->remote->link != rl)
			continue;
		gone = 1;
		for (i = 0; i < n; i++) {
			if (ids[i] == wp->remote->remote_id) {
				gone = 0;
				break;
			}
		}
		if (gone && window_count_panes(w, 1) > 1)
			server_kill_pane(wp);
	}

	/* Local floating panes leave the list; the parse frees their cells. */
	floats = remote_link_collect_floats(w, &nfloats);
	remote_link_order_panes(rl, w, ids, n);

	if (w->flags & WINDOW_ZOOMED)
		window_unzoom(w, 1);
	if (remote_link_layout_size(stripped, &sx, &sy) == 0 &&
	    (w->sx != sx || w->sy != sy))
		window_resize(w, sx, sy, -1, -1);
	if (layout_parse(w, stripped, &cause) != 0) {
		log_debug("%s: %s: @%u: %s", __func__, rl->host, w->id, cause);
		free(cause);
		layout_by_splitting(w);
	}
	remote_link_restore_floats(w, floats, nfloats);
	free(floats);

	for (i = 0; i < nadded; i++)
		remote_link_request_capture(rl, added[i]);
	free(added);
	free(ids);

	/*
	 * spawn_pane() on the remote makes the new pane active before it
	 * reports the layout, so the focus change may have arrived first.
	 */
	if (rw->want_active != -1) {
		rp = remote_link_find_pane(rl, rw->want_active);
		if (rp != NULL && rp->wp->window == w) {
			window_set_active_pane(w, rp->wp, 1);
			rw->want_active = -1;
		}
	}

	server_redraw_window(w);
	rl->applying--;
}

/* Reply to list-panes for one window: set the active pane. */
static void
remote_link_panes_cb(struct remote_link *rl, struct remote_request *req,
    int error, const char *body)
{
	struct remote_window	*rw = remote_link_find_window(rl, req->arg);
	struct remote_pane	*rp;
	const char		*p, *nl, *sep;
	u_int			 id;

	if (rw == NULL || error)
		return;
	for (p = body; *p != '\0'; p = (nl == NULL) ? p + strlen(p) : nl + 1) {
		nl = strchr(p, '\n');
		if (sscanf(p, "%%%u", &id) != 1)
			continue;
		sep = strchr(p, REMOTE_SEP_CHAR);
		if (sep == NULL || (nl != NULL && sep > nl) || sep[1] != '1')
			continue;
		rp = remote_link_find_pane(rl, id);
		if (rp != NULL && rp->wp->window == rw->w) {
			rl->applying++;
			window_set_active_pane(rw->w, rp->wp, 1);
			rl->applying--;
		}
		if (nl == NULL)
			break;
	}
}

/*
 * Build the local session name from the host and the remote session name.
 * A target splits on ":" and ".", so a host such as user@10.0.0.5 would make
 * the name unusable; replace both with "_". The host string itself stays as
 * given for remote_host and the ssh command.
 */
static char *
remote_link_label(const char *host, const char *session)
{
	char	*name, *cp;

	if (session != NULL)
		xasprintf(&name, "%s/%s", host, session);
	else
		name = xstrdup(host);
	for (cp = name; *cp != '\0'; cp++) {
		if (*cp == ':' || *cp == '.')
			*cp = '_';
	}
	return (name);
}

/*
 * Set the local session name, keeping the "host/" prefix.
 */
static void
remote_link_set_session_name(struct remote_link *rl, const char *name)
{
	struct session	*s = rl->s;
	char		*full;

	if (s == NULL)
		return;
	full = remote_link_label(rl->host, name);
	if (strcmp(s->name, full) == 0 || session_find(full) != NULL) {
		free(full);
		return;
	}
	RB_REMOVE(sessions, &sessions, s);
	free(s->name);
	s->name = full;
	RB_INSERT(sessions, &sessions, s);
	server_status_session(s);
	events_fire_session("session-renamed", s);
}

/*
 * One window line from list-windows or display-message:
 * id SEP index SEP active SEP layout SEP name. Creates or reconciles the
 * shadow. The local window takes the remote index when it is free.
 */
static struct remote_window *
remote_link_sync_window_line(struct remote_link *rl, const char *line,
    int *active)
{
	struct remote_window	*rw;
	struct window		*w;
	struct winlink		*wl;
	struct remote_pane	*rp;
	char			*copy, *fields[5], *p, *stripped, *name;
	u_int			 i, id, *ids, n, sx, sy;
	int			 created = 0, idx;

	copy = xstrdup(line);
	p = copy;
	for (i = 0; i < 4; i++) {
		fields[i] = p;
		p = strchr(p, REMOTE_SEP_CHAR);
		if (p == NULL) {
			free(copy);
			return (NULL);
		}
		*p++ = '\0';
	}
	fields[4] = p;
	if (sscanf(fields[0], "@%u", &id) != 1) {
		free(copy);
		return (NULL);
	}
	idx = strtonum(fields[1], 0, INT_MAX, NULL);
	*active = (fields[2][0] == '1');
	stripped = remote_parse_layout_strip(fields[3]);
	name = fields[4];

	rw = remote_link_find_window(rl, id);
	if (rw == NULL) {
		if (remote_link_layout_size(stripped, &sx, &sy) != 0) {
			sx = 80;
			sy = 24;
		}
		wl = winlink_add(&rl->s->windows, idx);
		if (wl == NULL)
			wl = winlink_add(&rl->s->windows, -1);
		if (wl == NULL) {
			free(stripped);
			free(copy);
			return (NULL);
		}
		w = window_create(sx, sy, 0, 0);
		wl->session = rl->s;
		winlink_set_window(wl, w);
		if (rl->s->curw == NULL)
			rl->s->curw = wl;
		free(w->name);
		w->name = xstrdup(name);
		options_set_number(w->options, "automatic-rename", 0);
		w->remote = remote_ref_new(rl, id);

		rw = xcalloc(1, sizeof *rw);
		rw->remote_id = id;
		rw->w = w;
		rw->want_active = -1;
		RB_INSERT(remote_windows, &rl->windows, rw);
		created = 1;
		log_debug("%s: %s: @%u mirrors @%u", __func__, rl->host, w->id,
		    id);
	} else {
		w = rw->w;
		wl = winlink_find_by_window(&rl->s->windows, w);
		if (strcmp(w->name, name) != 0)
			window_set_name(w, name, 0);
	}
	rw->seen = 1;

	remote_link_apply_layout(rl, rw, stripped);

	if (created) {
		events_fire_window("window-created", w);
		if (wl != NULL)
			events_fire_winlink("window-linked", wl);
	} else {
		/* Reconnect: every pane gets a fresh copy of the grid. */
		ids = remote_parse_layout_leaf_ids(stripped, &n);
		for (i = 0; i < n; i++) {
			rp = remote_link_find_pane(rl, ids[i]);
			if (rp != NULL && !rp->awaiting_capture)
				remote_link_request_capture(rl, rp);
		}
		free(ids);
	}
	remote_link_send(rl, remote_link_panes_cb, id, NULL,
	    "list-panes -t @%u -F '" REMOTE_PANE_FORMAT "'", id);

	free(stripped);
	free(copy);
	return (rw);
}

/* Kill the placeholder window once a shadow window exists. */
static void
remote_link_kill_placeholder(struct remote_link *rl)
{
	struct window	*w;

	if (rl->placeholder_id == -1 || RB_EMPTY(&rl->windows))
		return;
	w = window_find_by_id(rl->placeholder_id);
	rl->placeholder_id = -1;
	if (w != NULL)
		server_kill_window(w, 0);
}

/* Reply to list-windows: the whole session tree. */
static void
remote_link_windows_cb(struct remote_link *rl, __unused struct remote_request *req,
    int error, const char *body)
{
	struct remote_window	*rw, *rw1, *active_rw = NULL;
	struct winlink		*wl;
	char			*copy, *line, *next;
	int			 active;
	u_int			 nseen;

	if (rl->s == NULL)
		return;
	if (error) {
		log_debug("%s: %s: %s", __func__, rl->host, body);
		rl->want_disconnect = 1;
		return;
	}

	RB_FOREACH(rw, remote_windows, &rl->windows)
		rw->seen = 0;

	copy = xstrdup(body);
	for (line = copy; line != NULL && *line != '\0'; line = next) {
		next = strchr(line, '\n');
		if (next != NULL)
			*next++ = '\0';
		rw = remote_link_sync_window_line(rl, line, &active);
		if (rw != NULL && active)
			active_rw = rw;
		if (rl->s == NULL)
			break;
	}
	free(copy);
	if (rl->s == NULL)
		return;

	/* Windows the remote no longer has. */
	nseen = 0;
	RB_FOREACH(rw, remote_windows, &rl->windows) {
		if (rw->seen)
			nseen++;
	}
	if (nseen != 0) {
		rl->applying++;
		RB_FOREACH_SAFE(rw, remote_windows, &rl->windows, rw1) {
			if (!rw->seen)
				server_kill_window(rw->w, 1);
		}
		rl->applying--;
	}

	remote_link_kill_placeholder(rl);
	if (rl->s == NULL)
		return;

	if (active_rw != NULL) {
		wl = winlink_find_by_window(&rl->s->windows, active_rw->w);
		if (wl != NULL) {
			rl->applying++;
			session_set_current(rl->s, wl);
			rl->applying--;
		}
	}

	rl->state = REMOTE_UP;
	rl->backoff = 0;
	rl->synced_once = 1;
	/*
	 * The session exists now. From here every connect attaches; a
	 * session the remote kills on purpose must not come back.
	 */
	rl->may_create = 0;
	free(rl->last_error);
	rl->last_error = NULL;
	free(rl->reported_error);
	rl->reported_error = NULL;
	rl->bridge_unsupported = 0;
	RB_FOREACH(rw, remote_windows, &rl->windows)
		rw->sent_sx = rw->sent_sy = 0;
	recalculate_sizes();
	server_redraw_session(rl->s);
	log_debug("%s: %s: up", __func__, rl->host);
#ifdef ENABLE_PLUGINS
	plugin_bridge_link_state(rl, 1);
#endif
}

/* Reply to pause-after: an old remote cannot pause, carry on without it. */
static void
remote_link_flags_cb(struct remote_link *rl, __unused struct remote_request *req,
    int error, const char *body)
{
	if (error)
		log_debug("%s: %s: no pause-after: %s", __func__, rl->host, body);
}

/* Ask the remote for everything the shadow session needs. */
static void
remote_link_start_sync(struct remote_link *rl)
{
	static const struct {
		const char	*name;
		const char	*format;
	} subs[] = {
		{ "rl_cmd", "#{pane_current_command}" },
		{ "rl_path", "#{pane_current_path}" },
		{ "rl_pid", "#{pane_pid}" },
		{ "rl_tty", "#{pane_tty}" },
	};
	u_int	i;

	rl->state = REMOTE_SYNCING;
	remote_link_send(rl, remote_link_flags_cb, 0, NULL,
	    "refresh-client -f pause-after=%u", REMOTE_PAUSE_AFTER);
	for (i = 0; i < nitems(subs); i++) {
		remote_link_send(rl, NULL, 0, NULL,
		    "refresh-client -B '%s:%%*:%s'", subs[i].name,
		    subs[i].format);
	}
	remote_link_send(rl, remote_link_windows_cb, 0, NULL,
	    "list-windows -F '" REMOTE_WINDOW_FORMAT "'");
}

/* Parser callbacks. */

static void
remote_link_cb_begin(void *data, __unused uint64_t t, u_int number, int flags)
{
	struct remote_link	*rl = data;

	rl->skip_block = 0;
	rl->unsolicited = 0;
	rl->cur = NULL;
	if (flags != 1) {
		/* Not a reply to a control mode command of ours. */
		rl->skip_block = 1;
		return;
	}
	rl->cur = TAILQ_FIRST(&rl->requests);
	if (rl->cur == NULL) {
		log_debug("%s: %s: unsolicited block %u", __func__, rl->host,
		    number);
		rl->unsolicited = 1;
	}
}

static void
remote_link_cb_finish(struct remote_link *rl, int error, const char *body)
{
	struct remote_request	*req = rl->cur;

	rl->cur = NULL;
	if (rl->skip_block || rl->unsolicited) {
		/*
		 * The reply to the attach itself is not ours to match, but
		 * its error ("can't find session") is why the link fails.
		 */
		if (error && rl->state != REMOTE_UP)
			remote_link_set_error(rl, body);
		rl->skip_block = 0;
		rl->unsolicited = 0;
		return;
	}
	if (req == NULL)
		return;
	TAILQ_REMOVE(&rl->requests, req, entry);
	if (req->cb != NULL && !rl->dying)
		req->cb(rl, req, error, body);
	remote_link_request_free(req);
}

static void
remote_link_cb_end(void *data, __unused uint64_t t, __unused u_int number,
    __unused int flags, const char *body)
{
	remote_link_cb_finish(data, 0, body);
}

static void
remote_link_cb_error(void *data, __unused uint64_t t, __unused u_int number,
    __unused int flags, const char *body)
{
	remote_link_cb_finish(data, 1, body);
}

static void
remote_link_cb_output(void *data, u_int pane, const u_char *bytes, size_t len,
    __unused uint64_t age, __unused int extended)
{
	struct remote_link	*rl = data;
	struct remote_pane	*rp = remote_link_find_pane(rl, pane);

	if (rp == NULL || rp->awaiting_capture || rl->dying)
		return;
	remote_link_pane_write(rp, bytes, len);
}

static void
remote_link_cb_pause(void *data, u_int pane)
{
	struct remote_link	*rl = data;
	struct remote_pane	*rp = remote_link_find_pane(rl, pane);

	if (rp == NULL || rl->dying)
		return;
	rp->paused = 1;
	if (remote_link_send(rl, NULL, 0, NULL,
	    "refresh-client -A %%%u:continue", pane) != NULL)
		remote_link_request_capture(rl, rp);
}

static void
remote_link_cb_continue(void *data, u_int pane)
{
	struct remote_link	*rl = data;
	struct remote_pane	*rp = remote_link_find_pane(rl, pane);

	if (rp != NULL)
		rp->paused = 0;
}

static void
remote_link_cb_layout_change(void *data, u_int window, const char *layout,
    __unused const char *visible, __unused const char *flags)
{
	struct remote_link	*rl = data;
	struct remote_window	*rw = remote_link_find_window(rl, window);
	char			*stripped;

	if (rw == NULL || rl->dying || rl->s == NULL)
		return;
	stripped = remote_parse_layout_strip(layout);
	remote_link_apply_layout(rl, rw, stripped);
	free(stripped);
}

/* Reply to display-message for a window that was added. */
static void
remote_link_window_cb(struct remote_link *rl, __unused struct remote_request *req,
    int error, const char *body)
{
	int	active;

	if (error || rl->s == NULL)
		return;
	if (remote_link_sync_window_line(rl, body, &active) != NULL)
		server_redraw_session(rl->s);
}

static void
remote_link_cb_window_add(void *data, u_int window)
{
	struct remote_link	*rl = data;

	if (rl->dying || rl->s == NULL || rl->state != REMOTE_UP)
		return;
	if (remote_link_find_window(rl, window) != NULL)
		return;
	remote_link_send(rl, remote_link_window_cb, window, NULL,
	    "display-message -p -t @%u '" REMOTE_WINDOW_FORMAT "'", window);
}

static void
remote_link_cb_window_close(void *data, u_int window)
{
	struct remote_link	*rl = data;
	struct remote_window	*rw = remote_link_find_window(rl, window);

	if (rw == NULL || rl->dying || rl->s == NULL)
		return;
	rl->applying++;
	server_kill_window(rw->w, 1);
	rl->applying--;
}

static void
remote_link_cb_window_renamed(void *data, u_int window, const char *name)
{
	struct remote_link	*rl = data;
	struct remote_window	*rw = remote_link_find_window(rl, window);

	if (rw == NULL || rl->dying)
		return;
	if (strcmp(rw->w->name, name) != 0)
		window_set_name(rw->w, name, 0);
}

static void
remote_link_cb_window_pane_changed(void *data, u_int window, u_int pane)
{
	struct remote_link	*rl = data;
	struct remote_window	*rw = remote_link_find_window(rl, window);
	struct remote_pane	*rp = remote_link_find_pane(rl, pane);

	if (rw == NULL || rl->dying)
		return;
	if (rp == NULL || rp->wp->window != rw->w) {
		rw->want_active = pane;
		return;
	}
	rw->want_active = -1;
	rl->applying++;
	window_set_active_pane(rw->w, rp->wp, 1);
	rl->applying--;
}

static void
remote_link_cb_session_changed(void *data, u_int session, const char *name)
{
	struct remote_link	*rl = data;

	if (rl->dying)
		return;
	if (rl->have_session_id && rl->remote_session_id != session) {
		/* The control client moved to another session: start over. */
		log_debug("%s: %s: session changed to $%u", __func__, rl->host,
		    session);
		rl->want_disconnect = 1;
		return;
	}
	rl->remote_session_id = session;
	rl->have_session_id = 1;
	if (rl->s != NULL && rl->s->remote != NULL)
		rl->s->remote->remote_id = session;
	remote_link_set_session_name(rl, name);
}

static void
remote_link_cb_session_renamed(void *data, u_int session, const char *name)
{
	struct remote_link	*rl = data;

	if (rl->dying || !rl->have_session_id || rl->remote_session_id != session)
		return;
	if (rl->remote_session != NULL) {
		free(rl->remote_session);
		rl->remote_session = xstrdup(name);
	}
	remote_link_set_session_name(rl, name);
}

static void
remote_link_cb_session_window_changed(void *data, u_int session, u_int window)
{
	struct remote_link	*rl = data;
	struct remote_window	*rw = remote_link_find_window(rl, window);
	struct winlink		*wl;

	if (rw == NULL || rl->dying || rl->s == NULL)
		return;
	if (rl->have_session_id && rl->remote_session_id != session)
		return;
	wl = winlink_find_by_window(&rl->s->windows, rw->w);
	if (wl == NULL)
		return;
	rl->applying++;
	if (session_set_current(rl->s, wl) == 0)
		server_redraw_session(rl->s);
	rl->applying--;
}

static void
remote_link_cb_subscription_changed(void *data, const char *name,
    __unused u_int session, __unused int window, __unused int idx, int pane,
    const char *value)
{
	struct remote_link	*rl = data;
	struct remote_pane	*rp;
	int			 what;

	if (pane < 0 || rl->dying)
		return;
	rp = remote_link_find_pane(rl, pane);
	if (rp == NULL)
		return;
	if (strcmp(name, "rl_cmd") == 0)
		what = REMOTE_CACHE_CMD;
	else if (strcmp(name, "rl_path") == 0)
		what = REMOTE_CACHE_PATH;
	else if (strcmp(name, "rl_pid") == 0)
		what = REMOTE_CACHE_PID;
	else if (strcmp(name, "rl_tty") == 0)
		what = REMOTE_CACHE_TTY;
	else
		return;
	free(rp->cache[what]);
	rp->cache[what] = xstrdup(value);
	if (what == REMOTE_CACHE_CMD)
		server_status_window(rp->wp->window);
}

/* %bridge: a plugin bridge frame from the remote host, base64. */
static void
remote_link_cb_bridge(void *data, const char *b64)
{
	struct remote_link	*rl = data;

	if (rl->dying)
		return;
#ifdef ENABLE_PLUGINS
	plugin_bridge_recv(rl->id | REMOTE_LINK_PEER_BIT, b64);
#endif
}

static void
remote_link_cb_exit(void *data, const char *reason)
{
	struct remote_link	*rl = data;

	log_debug("%s: %s: %%exit %s", __func__, rl->host, reason);
	if (rl->state != REMOTE_UP && !rl->error_this_try)
		remote_link_set_error(rl, reason);
	rl->want_disconnect = 1;
}

/*
 * A line that is not control mode protocol. Before the link is up that is
 * ssh talking ("Host key verification failed.", "Permission denied"), the
 * reason the link fails.
 */
static void
remote_link_cb_unknown(void *data, const char *line)
{
	struct remote_link	*rl = data;

	log_debug("%s: %s: %s", __func__, rl->host, line);
	if (rl->state != REMOTE_UP)
		remote_link_set_error(rl, line);
}

static const struct remote_parse_callbacks remote_link_callbacks = {
	.begin = remote_link_cb_begin,
	.end = remote_link_cb_end,
	.error = remote_link_cb_error,
	.output = remote_link_cb_output,
	.pause = remote_link_cb_pause,
	.cont = remote_link_cb_continue,
	.layout_change = remote_link_cb_layout_change,
	.window_add = remote_link_cb_window_add,
	.window_close = remote_link_cb_window_close,
	.window_renamed = remote_link_cb_window_renamed,
	.window_pane_changed = remote_link_cb_window_pane_changed,
	.session_changed = remote_link_cb_session_changed,
	.sessions_changed = NULL,
	.session_renamed = remote_link_cb_session_renamed,
	.session_window_changed = remote_link_cb_session_window_changed,
	.pane_mode_changed = NULL,
	.subscription_changed = remote_link_cb_subscription_changed,
	.exit = remote_link_cb_exit,
	.bridge = remote_link_cb_bridge,
	.unknown = remote_link_cb_unknown,
};

/* Job and connection. */

/* Deferred work: a callback must not tear the job down from inside itself. */
static void
remote_link_deferred(__unused int fd, __unused short events, void *data)
{
	struct remote_link	*rl = data;

	if (rl->dying) {
		remote_link_destroy(rl);
		return;
	}
	if (rl->want_disconnect) {
		rl->want_disconnect = 0;
		remote_link_disconnect(rl);
	}
}

static void
remote_link_defer(struct remote_link *rl)
{
	struct timeval	tv = { .tv_sec = 0, .tv_usec = 0 };

	if (!evtimer_pending(&rl->defer_timer, NULL))
		evtimer_add(&rl->defer_timer, &tv);
}

static void
remote_link_job_update(struct job *job)
{
	struct remote_link	*rl = job_get_data(job);

	remote_parse_feed(rl->parser, job_get_event(job)->input);
	if (rl->dying || rl->want_disconnect)
		remote_link_defer(rl);
}

/*
 * The connection is gone: fail what was pending, tell the user in every
 * shadow pane and arrange to try again. The panes keep their grids and
 * their socketpair ends, so nothing local is destroyed.
 */
static void
remote_link_down(struct remote_link *rl)
{
	struct remote_pane	*rp;
	int			 was_up = (rl->state == REMOTE_UP);

	rl->state = REMOTE_DOWN;
#ifdef ENABLE_PLUGINS
	if (was_up)
		plugin_bridge_link_state(rl, 0);
#endif
	remote_parser_reset(rl->parser);
	remote_link_fail_requests(rl, "remote link disconnected");
	if (rl->dying)
		return;
	/*
	 * One line per pane when the link goes down, not on every failed
	 * retry: the grid is a mirror of the remote, and the refill on
	 * reconnect replaces the line anyway. The status line carries the
	 * live state (remote_state, remote_error). Start on a fresh line when
	 * the remote left the cursor mid-line.
	 */
	RB_FOREACH(rp, remote_panes, &rl->panes) {
		rp->awaiting_capture = 0;
		rp->paused = 0;
		if (!was_up)
			continue;
		remote_link_pane_printf(rp, "%s[remote: %s disconnected%s%s]\r\n",
		    rp->wp->base.cx != 0 ? "\r\n" : "", rl->host,
		    rl->last_error != NULL ? ": " : "",
		    rl->last_error != NULL ? rl->last_error : "");
	}
	remote_link_report_error(rl);
	if (rl->s != NULL)
		server_redraw_session(rl->s);
	remote_link_schedule_retry(rl);
}

/* The ssh process ended. job_free() follows this call. */
static void
remote_link_job_complete(struct job *job)
{
	struct remote_link	*rl = job_get_data(job);
	int			 status = job_get_status(job);
	char			*text;

	log_debug("%s: %s: status %d", __func__, rl->host, status);
	rl->job = NULL;
	if (rl->state != REMOTE_UP && !rl->error_this_try) {
		if (WIFEXITED(status) && WEXITSTATUS(status) != 0) {
			xasprintf(&text, "command exited with status %d",
			    WEXITSTATUS(status));
			remote_link_set_error(rl, text);
			free(text);
		} else if (WIFSIGNALED(status)) {
			xasprintf(&text, "command killed by signal %d",
			    WTERMSIG(status));
			remote_link_set_error(rl, text);
			free(text);
		}
	}
	remote_link_down(rl);
}

/* Kill the job ourselves, after %exit or a session switch. */
static void
remote_link_disconnect(struct remote_link *rl)
{
	struct job	*job = rl->job;

	if (job == NULL)
		return;
	rl->job = NULL;
	job_free(job);
	remote_link_down(rl);
}

static void
remote_link_retry_callback(__unused int fd, __unused short events, void *data)
{
	struct remote_link	*rl = data;

	if (rl->dying || rl->job != NULL)
		return;
	remote_link_connect(rl);
}

static void
remote_link_schedule_retry(struct remote_link *rl)
{
	struct timeval	tv = { .tv_sec = 1, .tv_usec = 0 };

	if (rl->backoff == 0)
		rl->backoff = 1;
	else if (rl->backoff < REMOTE_BACKOFF_MAX)
		rl->backoff *= 2;
	if (rl->backoff > REMOTE_BACKOFF_MAX)
		rl->backoff = REMOTE_BACKOFF_MAX;
	tv.tv_sec = rl->backoff;
	log_debug("%s: %s: retry in %u", __func__, rl->host, rl->backoff);
	evtimer_add(&rl->retry_timer, &tv);
}

/*
 * Expand the remote-ssh-command option for a host. remote_command is the
 * tmux command line for the remote end (quoted for its shell); the shadow
 * session, when there is one, lets the table formats find the link.
 */
static char *
remote_link_expand_command(const char *host, const char *session,
    struct session *s, const char *remote_command)
{
	struct format_tree	*ft;
	char			*cmd;

	ft = format_create(NULL, NULL, FORMAT_NONE, FORMAT_NOJOBS);
	/*
	 * remote_host is a table format. With a shadow session its callback
	 * finds the link; without one (remote-attach -L) it has nothing and
	 * the value added below is used.
	 */
	format_defaults(ft, NULL, s, NULL, NULL);
	format_add(ft, "remote_host", "%s", host);
	format_add(ft, "remote_session", "%s", session != NULL ? session : "");
	format_add(ft, "remote_command", "%s", remote_command);
	cmd = format_expand(ft, options_get_string(global_options,
	    "remote-ssh-command"));
	format_free(ft);
	return (cmd);
}

/*
 * The tmux command line for the remote end of this link. Until the first
 * sync it is new-session -A, which attaches the named session or creates
 * it; after that plain attach, so a session the remote killed stays dead.
 * Without a name the remote picks its current session.
 */
static char *
remote_link_remote_command(struct remote_link *rl)
{
	char	*qs, *qc, *cmd;

	if (rl->remote_session == NULL)
		return (xstrdup("-C attach"));
	qs = server_handoff_shell_quote(rl->remote_session);
	if (rl->may_create) {
		if (rl->remote_cwd != NULL) {
			qc = server_handoff_shell_quote(rl->remote_cwd);
			xasprintf(&cmd, "-C new-session -A -s %s -c %s", qs, qc);
			free(qc);
		} else
			xasprintf(&cmd, "-C new-session -A -s %s", qs);
	} else
		xasprintf(&cmd, "-C attach -t %s", qs);
	free(qs);
	return (cmd);
}

/* Build the ssh command from the remote-ssh-command option. */
static char *
remote_link_command(struct remote_link *rl)
{
	char	*remote, *cmd;

	remote = remote_link_remote_command(rl);
	cmd = remote_link_expand_command(rl->host, rl->remote_session, rl->s,
	    remote);
	free(remote);
	return (cmd);
}

/* remote-attach -L: list the sessions on a host, one name per line. */
struct remote_list {
	struct cmdq_item	*item;
	char			*host;
};

static void
remote_link_list_callback(struct job *job)
{
	struct remote_list	*rlist = job_get_data(job);
	struct bufferevent	*event = job_get_event(job);
	char			*line;
	size_t			 size;
	int			 status = job_get_status(job);

	for (;;) {
		line = evbuffer_readln(event->input, NULL, EVBUFFER_EOL_LF);
		if (line == NULL)
			break;
		cmdq_print(rlist->item, "%s", line);
		free(line);
	}
	size = EVBUFFER_LENGTH(event->input);
	if (size != 0) {
		line = xmalloc(size + 1);
		memcpy(line, EVBUFFER_DATA(event->input), size);
		line[size] = '\0';
		cmdq_print(rlist->item, "%s", line);
		free(line);
	}
	if (WIFEXITED(status) && WEXITSTATUS(status) != 0) {
		cmdq_error(rlist->item, "%s: command exited with status %d",
		    rlist->host, WEXITSTATUS(status));
	} else if (WIFSIGNALED(status)) {
		cmdq_error(rlist->item, "%s: command killed by signal %d",
		    rlist->host, WTERMSIG(status));
	}
	cmdq_continue(rlist->item);
}

static void
remote_link_list_free(void *data)
{
	struct remote_list	*rlist = data;

	free(rlist->host);
	free(rlist);
}

enum cmd_retval
remote_link_list(struct cmdq_item *item, const char *host)
{
	struct remote_list	*rlist;
	char			*cmd;

	/* The remote shell strips these quotes; tmux there sees the format. */
	cmd = remote_link_expand_command(host, NULL, NULL,
	    "list-sessions -F '#{session_name}'");
	log_debug("%s: %s: %s", __func__, host, cmd);
	rlist = xcalloc(1, sizeof *rlist);
	rlist->item = item;
	rlist->host = xstrdup(host);
	if (job_run(cmd, 0, NULL, NULL, NULL, NULL, NULL,
	    remote_link_list_callback, remote_link_list_free, rlist,
	    JOB_NOWAIT, -1, -1) == NULL) {
		cmdq_error(item, "%s: cannot run %s", host, cmd);
		free(cmd);
		remote_link_list_free(rlist);
		return (CMD_RETURN_ERROR);
	}
	free(cmd);
	return (CMD_RETURN_WAIT);
}

static void
remote_link_connect(struct remote_link *rl)
{
	char	*cmd;

	cmd = remote_link_command(rl);
	log_debug("%s: %s: %s", __func__, rl->host, cmd);
	rl->error_this_try = 0;
	/* ssh reports on stderr; the parser hands those lines to cb_unknown. */
	rl->job = job_run(cmd, 0, NULL, NULL, NULL, NULL,
	    remote_link_job_update, remote_link_job_complete, NULL, rl,
	    JOB_NOWAIT|JOB_KEEPWRITE|JOB_SHOWSTDERR, 0, 0);
	free(cmd);
	if (rl->job == NULL) {
		rl->state = REMOTE_DOWN;
		remote_link_set_error(rl, "cannot start the ssh command");
		remote_link_report_error(rl);
		remote_link_schedule_retry(rl);
		return;
	}
	rl->state = REMOTE_CONNECTING;
	rl->have_session_id = 0;
	remote_link_start_sync(rl);
}

/* Creation and destruction. */

/*
 * Create a link and its shadow session. pinned_id is a session id to reuse
 * (from a server handoff) or -1.
 */
struct remote_link *
remote_link_create(const char *host, const char *session, const char *cwd,
    int pinned_id, char **cause)
{
	struct remote_link	*rl;
	struct session		*s;
	struct spawn_context	 sc;
	struct winlink		*wl;
	char			*name, *wname;
	const char		*home;

	name = remote_link_label(host, session);
	if (session_find(name) != NULL) {
		xasprintf(cause, "duplicate session: %s", name);
		free(name);
		return (NULL);
	}

	rl = xcalloc(1, sizeof *rl);
	rl->id = next_remote_link_id++;
	rl->host = xstrdup(host);
	if (session != NULL)
		rl->remote_session = xstrdup(session);
	if (cwd != NULL && *cwd != '\0')
		rl->remote_cwd = xstrdup(cwd);
	/* A link handed over by restart-server mirrors a session that exists. */
	rl->may_create = (pinned_id < 0);
	rl->placeholder_id = -1;
	RB_INIT(&rl->windows);
	RB_INIT(&rl->panes);
	TAILQ_INIT(&rl->requests);
	rl->parser = remote_parser_create(&remote_link_callbacks, rl);
	evtimer_set(&rl->retry_timer, remote_link_retry_callback, rl);
	evtimer_set(&rl->defer_timer, remote_link_deferred, rl);
	rl->state = REMOTE_DOWN;

	if ((home = find_home()) == NULL)
		home = "/";
	if (pinned_id >= 0)
		next_session_id = pinned_id;
	s = session_create(NULL, name, home, environ_create(),
	    options_create(global_s_options), NULL);
	free(name);
	s->remote = remote_ref_new(rl, 0);
	rl->s = s;

	/* A placeholder window makes the session attachable at once. */
	xasprintf(&wname, "connecting to %s", host);
	memset(&sc, 0, sizeof sc);
	sc.s = s;
	sc.idx = REMOTE_PLACEHOLDER_INDEX;
	sc.name = wname;
	sc.flags = SPAWN_EMPTY|SPAWN_DETACHED;
	wl = spawn_window(&sc, cause);
	free(wname);
	if (wl == NULL) {
		session_destroy(s, 0, __func__);
		rl->s = NULL;
		remote_parser_free(rl->parser);
		free(rl->host);
		free(rl->remote_session);
		free(rl);
		return (NULL);
	}
	rl->placeholder_id = wl->window->id;
	events_fire_session("session-created", s);

	TAILQ_INSERT_TAIL(&remote_links, rl, entry);
	remote_link_connect(rl);
	return (rl);
}

/*
 * Tear a link down: the job, the requests, the socketpair ends and the
 * shadow session. Safe to call from a deferred event only.
 */
void
remote_link_destroy(struct remote_link *rl)
{
	struct remote_pane	*rp, *rp1;
	struct remote_window	*rw, *rw1;
	struct session		*s;
	struct job		*job;

	log_debug("%s: %s", __func__, rl->host);
#ifdef ENABLE_PLUGINS
	if (rl->state == REMOTE_UP)
		plugin_bridge_link_state(rl, 0);
#endif
	rl->dying = 1;
	evtimer_del(&rl->retry_timer);
	evtimer_del(&rl->defer_timer);
	TAILQ_REMOVE(&remote_links, rl, entry);

	if ((job = rl->job) != NULL) {
		rl->job = NULL;
		job_free(job);
	}
	remote_link_fail_requests(rl, "remote link closed");

	RB_FOREACH_SAFE(rp, remote_panes, &rl->panes, rp1) {
		free(rp->wp->remote);
		rp->wp->remote = NULL;
		rp->wp->flags &= ~PANE_REMOTE;
		remote_link_pane_free(rl, rp);
	}
	RB_FOREACH_SAFE(rw, remote_windows, &rl->windows, rw1) {
		free(rw->w->remote);
		rw->w->remote = NULL;
		remote_link_window_free(rl, rw);
	}
	if ((s = rl->s) != NULL) {
		rl->s = NULL;
		free(s->remote);
		s->remote = NULL;
		server_destroy_session(s);
		session_destroy(s, 1, __func__);
	}

	remote_parser_free(rl->parser);
	free(rl->host);
	free(rl->remote_session);
	free(rl->remote_cwd);
	free(rl->menu_client);
	free(rl->last_error);
	free(rl->reported_error);
	free(rl);
}

/* Hooks from window.c and session.c when a shadow object goes away. */

void
remote_link_pane_destroyed(struct window_pane *wp)
{
	struct remote_link	*rl;
	struct remote_pane	*rp;

	if (wp->remote == NULL)
		return;
	rl = wp->remote->link;
	rp = remote_link_find_pane(rl, wp->remote->remote_id);
	if (rp != NULL && rp->wp == wp)
		remote_link_pane_free(rl, rp);
	free(wp->remote);
	wp->remote = NULL;
}

void
remote_link_window_destroyed(struct window *w)
{
	struct remote_link	*rl;
	struct remote_window	*rw;

	if (w->remote == NULL)
		return;
	rl = w->remote->link;
	rw = remote_link_find_window(rl, w->remote->remote_id);
	if (rw != NULL && rw->w == w)
		remote_link_window_free(rl, rw);
	free(w->remote);
	w->remote = NULL;
}

void
remote_link_session_destroyed(struct session *s)
{
	struct remote_link	*rl;

	if (s->remote == NULL)
		return;
	rl = s->remote->link;
	free(s->remote);
	s->remote = NULL;
	if (rl->s == s)
		rl->s = NULL;
	if (!rl->dying) {
		rl->dying = 1;
		remote_link_defer(rl);
	}
}

/* Resize and focus, from the local side. */

/* The local size for a shadow window changed: ask the remote to follow. */
void
remote_link_window_resize(struct window *w, u_int sx, u_int sy)
{
	struct remote_window	*rw = remote_link_window_of(w);
	struct remote_link	*rl;

	if (rw == NULL)
		return;
	rl = w->remote->link;
	if (rl->state != REMOTE_UP || rl->applying)
		return;
	if (rw->sent_sx == sx && rw->sent_sy == sy)
		return;
	rw->sent_sx = sx;
	rw->sent_sy = sy;
	remote_link_send(rl, NULL, 0, NULL, "refresh-client -C @%u:%ux%u",
	    rw->remote_id, sx, sy);
}

void
remote_link_pane_focus(struct window_pane *wp)
{
	struct remote_link	*rl;

	if (wp->remote == NULL)
		return;
	rl = wp->remote->link;
	if (rl->state != REMOTE_UP || rl->applying)
		return;
	remote_link_send(rl, NULL, 0, NULL, "select-pane -t %%%u",
	    wp->remote->remote_id);
}

void
remote_link_window_focus(struct winlink *wl)
{
	struct remote_link	*rl;
	struct window		*w = wl->window;

	if (w->remote == NULL || wl->session == NULL ||
	    wl->session->remote == NULL)
		return;
	rl = w->remote->link;
	if (rl->state != REMOTE_UP || rl->applying)
		return;
	remote_link_send(rl, NULL, 0, NULL, "select-window -t @%u",
	    w->remote->remote_id);
}

/* Command forwarding. */

/*
 * The link a command target refers to, or NULL for a local target. The
 * object the command's target type names decides: a local floating pane in
 * a shadow window is a local pane, but the window around it is remote.
 */
struct remote_link *
remote_link_target(struct cmd_find_state *fs,
    const struct cmd_entry_flag *cef)
{
	switch (cef->type) {
	case CMD_FIND_PANE:
		if (fs->wp != NULL && fs->wp->remote != NULL)
			return (fs->wp->remote->link);
		return (NULL);
	case CMD_FIND_WINDOW:
		if (cef->flags & CMD_FIND_WINDOW_INDEX) {
			if (fs->s != NULL && fs->s->remote != NULL)
				return (fs->s->remote->link);
			return (NULL);
		}
		if (fs->w != NULL && fs->w->remote != NULL)
			return (fs->w->remote->link);
		return (NULL);
	case CMD_FIND_SESSION:
		if (fs->s != NULL && fs->s->remote != NULL)
			return (fs->s->remote->link);
		return (NULL);
	}
	return (NULL);
}

/* The remote id string for a target of the given type. */
static char *
remote_link_target_string(struct cmd_find_state *fs,
    const struct cmd_entry_flag *cef)
{
	char	*s = NULL;

	switch (cef->type) {
	case CMD_FIND_PANE:
		if (fs->wp != NULL && fs->wp->remote != NULL)
			xasprintf(&s, "%%%u", fs->wp->remote->remote_id);
		break;
	case CMD_FIND_WINDOW:
		if (cef->flags & CMD_FIND_WINDOW_INDEX) {
			if (fs->s == NULL || fs->s->remote == NULL)
				break;
			if (fs->idx == -1)
				xasprintf(&s, "$%u:", fs->s->remote->remote_id);
			else {
				xasprintf(&s, "$%u:%d", fs->s->remote->remote_id,
				    fs->idx);
			}
			break;
		}
		if (fs->w != NULL && fs->w->remote != NULL)
			xasprintf(&s, "@%u", fs->w->remote->remote_id);
		break;
	case CMD_FIND_SESSION:
		if (fs->s != NULL && fs->s->remote != NULL)
			xasprintf(&s, "$%u", fs->s->remote->remote_id);
		break;
	}
	return (s);
}

static void
remote_link_append(char **buf, size_t *len, const char *s)
{
	size_t	n = strlen(s);

	*buf = xrealloc(*buf, *len + n + 1);
	memcpy(*buf + *len, s, n + 1);
	*len += n;
}

static void
remote_link_append_value(char **buf, size_t *len, struct args_value *av)
{
	char	*s;

	switch (av->type) {
	case ARGS_NONE:
		break;
	case ARGS_STRING:
		s = args_escape(av->string);
		remote_link_append(buf, len, " ");
		remote_link_append(buf, len, s);
		free(s);
		break;
	case ARGS_COMMANDS:
		s = cmd_list_print(av->cmdlist, 0);
		remote_link_append(buf, len, " { ");
		remote_link_append(buf, len, s);
		remote_link_append(buf, len, " }");
		free(s);
		break;
	}
}

/*
 * Print a command with its target (and source) replaced by remote ids. The
 * text is one line the remote parser reads back into the same command.
 */
static char *
remote_link_print_command(struct cmd *cmd, const char *target,
    const char *source)
{
	const struct cmd_entry	*entry = cmd_get_entry(cmd);
	struct args		*args = cmd_get_args(cmd);
	struct args_entry	*ae;
	struct args_value	*av;
	char			*buf = NULL, flagbuf[4];
	size_t			 len = 0;
	u_char			 flag;
	u_int			 i;

	remote_link_append(&buf, &len, entry->name);
	for (flag = args_first(args, &ae); flag != 0; flag = args_next(&ae)) {
		if (flag == (u_char)entry->target.flag ||
		    (entry->source.flag != 0 &&
		    flag == (u_char)entry->source.flag))
			continue;
		xsnprintf(flagbuf, sizeof flagbuf, " -%c", flag);
		av = args_first_value(args, flag);
		if (av == NULL) {
			remote_link_append(&buf, &len, flagbuf);
			continue;
		}
		for (; av != NULL; av = args_next_value(av)) {
			remote_link_append(&buf, &len, flagbuf);
			remote_link_append_value(&buf, &len, av);
		}
	}
	if (target != NULL) {
		xsnprintf(flagbuf, sizeof flagbuf, " -%c", entry->target.flag);
		remote_link_append(&buf, &len, flagbuf);
		remote_link_append(&buf, &len, " ");
		remote_link_append(&buf, &len, target);
	}
	if (source != NULL) {
		xsnprintf(flagbuf, sizeof flagbuf, " -%c", entry->source.flag);
		remote_link_append(&buf, &len, flagbuf);
		remote_link_append(&buf, &len, " ");
		remote_link_append(&buf, &len, source);
	}
	if (args_count(args) != 0)
		remote_link_append(&buf, &len, " --");
	for (i = 0; i < args_count(args); i++)
		remote_link_append_value(&buf, &len, args_value(args, i));
	return (buf);
}

/* Reply to a forwarded command: print the body, then let the item go. */
static void
remote_link_forward_cb(__unused struct remote_link *rl,
    struct remote_request *req, int error, const char *body)
{
	struct cmdq_item	*item = req->item;
	const char		*p, *nl;

	if (item == NULL)
		return;
	req->item = NULL;
	if (error) {
		if (*body == '\0')
			body = "remote command failed";
		cmdq_error(item, "%s", body);
	} else {
		for (p = body; *p != '\0'; p = nl + 1) {
			nl = strchr(p, '\n');
			if (nl == NULL) {
				cmdq_print(item, "%s", p);
				break;
			}
			cmdq_print(item, "%.*s", (int)(nl - p), p);
		}
	}
	cmdq_continue(item);
}

/*
 * Send a command whose target is a shadow object to the remote instead of
 * running it. Returns CMD_RETURN_WAIT and the reply finishes the item.
 */
enum cmd_retval
remote_link_forward(struct cmdq_item *item, struct cmd *cmd)
{
	const struct cmd_entry	*entry = cmd_get_entry(cmd);
	struct cmd_find_state	*target = cmdq_get_target(item);
	struct cmd_find_state	*source = cmdq_get_source(item);
	struct remote_link	*tl, *sl = NULL;
	char			*ts = NULL, *ss = NULL, *text;
	int			 have_source;

	have_source = (entry->source.flag != 0 &&
	    cmd_find_valid_state(source));
	tl = remote_link_target(target, &entry->target);
	if (have_source)
		sl = remote_link_target(source, &entry->source);

	if (have_source && sl != tl) {
		cmdq_error(item, "cannot mix local and remote targets");
		return (CMD_RETURN_ERROR);
	}
	if (tl == NULL) {
		cmdq_error(item, "cannot move a remote pane to a local window");
		return (CMD_RETURN_ERROR);
	}
	if (tl->state != REMOTE_UP) {
		cmdq_error(item, "remote %s is not connected", tl->host);
		return (CMD_RETURN_ERROR);
	}

	ts = remote_link_target_string(target, &entry->target);
	if (ts == NULL) {
		cmdq_error(item, "no remote target");
		return (CMD_RETURN_ERROR);
	}
	if (have_source) {
		ss = remote_link_target_string(source, &entry->source);
		if (ss == NULL) {
			free(ts);
			cmdq_error(item, "no remote source");
			return (CMD_RETURN_ERROR);
		}
	}
	text = remote_link_print_command(cmd, ts, ss);
	free(ts);
	free(ss);

	if (strchr(text, '\n') != NULL) {
		free(text);
		cmdq_error(item, "cannot forward a command with a newline");
		return (CMD_RETURN_ERROR);
	}
	if (remote_link_send(tl, remote_link_forward_cb, 0, item, "%s",
	    text) == NULL) {
		free(text);
		cmdq_error(item, "remote %s is not connected", tl->host);
		return (CMD_RETURN_ERROR);
	}
	free(text);
	return (CMD_RETURN_WAIT);
}

/* Plugin bridge. */

/* Reply to plugin-bridge: an old remote has no such command. */
static void
remote_link_bridge_cb(struct remote_link *rl, __unused struct remote_request *req,
    int error, const char *body)
{
	if (error && !rl->bridge_unsupported) {
		log_debug("%s: %s: no plugin bridge: %s", __func__, rl->host,
		    body);
		rl->bridge_unsupported = 1;
	}
}

/*
 * Send an opaque plugin bridge frame to the remote host as a control mode
 * command. The remote decodes it and hands it to its plugin host. Returns
 * -1 when the link is down or the remote has no bridge.
 */
int
remote_link_bridge_send(struct remote_link *rl, const void *data, size_t len)
{
	char	*b64;
	size_t	 b64len;
	int	 n;

	if (rl->state != REMOTE_UP || rl->bridge_unsupported)
		return (-1);
	b64len = 4 * ((len + 2) / 3) + 1;
	b64 = xmalloc(b64len);
	n = b64_ntop(data, len, b64, b64len);
	if (n < 0) {
		free(b64);
		return (-1);
	}
	if (remote_link_send(rl, remote_link_bridge_cb, 0, NULL,
	    "plugin-bridge %s", b64) == NULL) {
		free(b64);
		return (-1);
	}
	free(b64);
	return (0);
}
