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

#include <ctype.h>
#include <event.h>
#include <stdlib.h>
#include <string.h>

#include "tmux.h"

/*
 * Parser for the control mode line protocol that a remote "tmux -C attach"
 * writes on its stdout. This file reads lines and calls back; it does not
 * touch server state, so a test harness can link it with xmalloc.c alone.
 *
 * A command reply is a %begin line, body lines and a matching %end or %error
 * line. Events fire synchronously on the remote, so a notification such as
 * %window-add can land inside a reply block; the parser dispatches known
 * notification lines wherever they appear and keeps every other line as body
 * (list-panes prints lines that start with "%3:", which is body).
 */

struct remote_parser {
	const struct remote_parse_callbacks	*cb;
	void					*data;

	int					 in_block;
	uint64_t				 block_time;
	u_int					 block_number;
	int					 block_flags;
	struct evbuffer				*body;
};

typedef void (*remote_parse_handler)(struct remote_parser *, const char *);

static void	remote_parse_line(struct remote_parser *, char *);
static int	remote_parse_leaf(const char **, u_int *);

/* Read an id with a prefix character ($, @ or %) and skip a space. */
static int
remote_parse_id(const char **s, char prefix, u_int *id)
{
	const char	*p = *s;
	char		*end;
	unsigned long	 n;

	if (*p != prefix || !isdigit((u_char)p[1]))
		return (-1);
	n = strtoul(p + 1, &end, 10);
	if (*end != '\0' && *end != ' ')
		return (-1);
	*id = n;
	if (*end == ' ')
		end++;
	*s = end;
	return (0);
}

/* Read an id or a lone "-" (which becomes -1) and skip a space. */
static int
remote_parse_id_or_dash(const char **s, char prefix, int *id)
{
	const char	*p = *s;
	u_int		 n;

	if (*p == '-' && (p[1] == ' ' || p[1] == '\0')) {
		*id = -1;
		*s = (p[1] == ' ') ? p + 2 : p + 1;
		return (0);
	}
	if (remote_parse_id(&p, prefix, &n) != 0)
		return (-1);
	*id = n;
	*s = p;
	return (0);
}

/* Read a decimal number or a lone "-" and skip a space. */
static int
remote_parse_num_or_dash(const char **s, int *n)
{
	const char	*p = *s;
	char		*end;

	if (*p == '-' && (p[1] == ' ' || p[1] == '\0')) {
		*n = -1;
		*s = (p[1] == ' ') ? p + 2 : p + 1;
		return (0);
	}
	if (!isdigit((u_char)*p))
		return (-1);
	*n = strtoul(p, &end, 10);
	if (*end != '\0' && *end != ' ')
		return (-1);
	*s = (*end == ' ') ? end + 1 : end;
	return (0);
}

/* Copy the next space-separated token. */
static char *
remote_parse_token(const char **s)
{
	const char	*p = *s, *end;
	char		*out;

	end = strchr(p, ' ');
	if (end == NULL) {
		out = xstrdup(p);
		*s = p + strlen(p);
	} else {
		out = xmalloc(end - p + 1);
		memcpy(out, p, end - p);
		out[end - p] = '\0';
		*s = end + 1;
	}
	return (out);
}

/* Parse the time, number and flags of a guard line. */
static int
remote_parse_guard(const char *s, uint64_t *t, u_int *number, int *flags)
{
	unsigned long long	 tt;
	char			*end;

	if (!isdigit((u_char)*s))
		return (-1);
	tt = strtoull(s, &end, 10);
	if (*end != ' ' || !isdigit((u_char)end[1]))
		return (-1);
	*t = tt;
	s = end + 1;
	*number = strtoul(s, &end, 10);
	if (*end != ' ')
		return (-1);
	s = end + 1;
	if (*s != '-' && !isdigit((u_char)*s))
		return (-1);
	*flags = strtol(s, &end, 10);
	if (*end != '\0')
		return (-1);
	return (0);
}

/*
 * Decode the \ooo escapes control mode uses for bytes below space and for
 * backslash. In place; returns the new length. A backslash that is not
 * followed by three octal digits stays as it is.
 */
size_t
remote_parse_unescape(char *s)
{
	char	*in = s, *out = s;
	u_int	 v;

	while (*in != '\0') {
		if (in[0] == '\\' &&
		    in[1] >= '0' && in[1] <= '7' &&
		    in[2] >= '0' && in[2] <= '7' &&
		    in[3] >= '0' && in[3] <= '7') {
			v = ((in[1] - '0') << 6) | ((in[2] - '0') << 3) |
			    (in[3] - '0');
			*out++ = (char)v;
			in += 4;
		} else
			*out++ = *in++;
	}
	*out = '\0';
	return (out - s);
}

/* Layout checksum, the same as layout_checksum() in layout-custom.c. */
static u_short
remote_parse_layout_checksum(const char *layout)
{
	u_short	csum;

	csum = 0;
	for (; *layout != '\0'; layout++) {
		csum = (csum >> 1) + ((csum & 1) << 15);
		csum += *layout;
	}
	return (csum);
}

/* Does the string start with "hhhh,"? */
static int
remote_parse_layout_has_checksum(const char *layout)
{
	u_int	i;

	for (i = 0; i < 4; i++) {
		if (!isxdigit((u_char)layout[i]))
			return (0);
	}
	return (layout[4] == ',');
}

/*
 * Remove one leaf cell "WxH,X,Y,ID" from a layout body, with the comma that
 * joined it to its neighbour. start points at the W, end just past the ID.
 */
static void
remote_parse_layout_cut(char *body, char *start, char *end)
{
	if (start > body && start[-1] == ',')
		start--;
	else if (*end == ',')
		end++;
	memmove(start, end, strlen(end) + 1);
}

/*
 * Cut the floating part ("<...>") off a layout string and put a checksum for
 * what remains in front. layout_parse() rejects the floating part. tmux2
 * also lists a floating pane's cell inline in the tiled tree, so any leaf
 * whose id the floating part names is cut out of the tree as well.
 */
char *
remote_parse_layout_strip(const char *layout)
{
	const char	*body = layout, *lt, *p;
	char		*copy, *out, *s, *start, *end;
	u_int		*floats = NULL, nfloats = 0, i, id;

	if (remote_parse_layout_has_checksum(body))
		body += 5;
	lt = strchr(body, '<');
	if (lt == NULL)
		copy = xstrdup(body);
	else {
		copy = xmalloc(lt - body + 1);
		memcpy(copy, body, lt - body);
		copy[lt - body] = '\0';
		floats = remote_parse_layout_leaf_ids(lt + 1, &nfloats);
	}

	/* Cut the floating leaves out of the tiled tree. */
	for (i = 0; i < nfloats; i++) {
		s = copy;
		while (*s != '\0') {
			if (!isdigit((u_char)*s)) {
				s++;
				continue;
			}
			start = s;
			p = s;
			if (remote_parse_leaf(&p, &id) != 0) {
				while (isdigit((u_char)*s))
					s++;
				continue;
			}
			end = copy + (p - copy);
			if (id == floats[i]) {
				remote_parse_layout_cut(copy, start, end);
				break;
			}
			s = end;
		}
	}
	free(floats);

	xasprintf(&out, "%04hx,%s", remote_parse_layout_checksum(copy), copy);
	free(copy);
	return (out);
}

/* Skip a run of digits. Returns 0 if there was none. */
static int
remote_parse_skip_digits(const char **s)
{
	const char	*p = *s;

	if (!isdigit((u_char)*p))
		return (0);
	while (isdigit((u_char)*p))
		p++;
	*s = p;
	return (1);
}

/*
 * Read a cell "WxH,X,Y" at *s and, if a pane id follows, the id. A leaf is
 * "WxH,X,Y,ID"; a node is "WxH,X,Y{...}" or "WxH,X,Y[...]", and the id is
 * told from a following cell's width by the "x" after the digits, the same
 * test layout_construct_cell() uses. Returns 0 for a leaf with *s moved past
 * the id, 1 for a node or a cell without an id with *s moved past the cell,
 * and -1 for no cell at all with *s moved past the digits.
 */
static int
remote_parse_leaf(const char **s, u_int *id)
{
	const char	*p = *s, *q, *r;

	if (!remote_parse_skip_digits(&p) || *p != 'x') {
		*s = p;
		return (-1);
	}
	p++;
	if (!remote_parse_skip_digits(&p) || *p != ',') {
		*s = p;
		return (-1);
	}
	p++;
	if (!remote_parse_skip_digits(&p) || *p != ',') {
		*s = p;
		return (-1);
	}
	p++;
	if (!remote_parse_skip_digits(&p)) {
		*s = p;
		return (-1);
	}
	if (*p == ',' && isdigit((u_char)p[1])) {
		q = p + 1;
		r = q;
		remote_parse_skip_digits(&r);
		if (*r != 'x') {
			*id = strtoul(q, NULL, 10);
			*s = r;
			return (0);
		}
	}
	*s = p;
	return (1);
}

/*
 * Collect the pane ids of the leaf cells in a layout string, in string order.
 * That is the order layout_assign() walks the window's pane list in. Stops
 * at the floating part.
 */
u_int *
remote_parse_layout_leaf_ids(const char *layout, u_int *n)
{
	const char	*s = layout;
	u_int		*ids = NULL, id;

	*n = 0;
	if (remote_parse_layout_has_checksum(s))
		s += 5;
	while (*s != '\0' && *s != '<' && *s != '>') {
		if (!isdigit((u_char)*s)) {
			s++;
			continue;
		}
		if (remote_parse_leaf(&s, &id) == 0) {
			ids = xreallocarray(ids, *n + 1, sizeof *ids);
			ids[(*n)++] = id;
		}
	}
	return (ids);
}

/* Handlers for each notification. */

static void
remote_parse_output(struct remote_parser *rp, const char *rest)
{
	u_int	 pane;
	char	*data;
	size_t	 len;

	if (remote_parse_id(&rest, '%', &pane) != 0)
		return;
	data = xstrdup(rest);
	len = remote_parse_unescape(data);
	if (rp->cb->output != NULL)
		rp->cb->output(rp->data, pane, (u_char *)data, len, 0, 0);
	free(data);
}

static void
remote_parse_extended_output(struct remote_parser *rp, const char *rest)
{
	u_int			 pane;
	unsigned long long	 age;
	char			*data, *end;
	size_t			 len;

	if (remote_parse_id(&rest, '%', &pane) != 0)
		return;
	if (!isdigit((u_char)*rest))
		return;
	age = strtoull(rest, &end, 10);
	if (strncmp(end, " : ", 3) == 0)
		end += 3;
	else if (strcmp(end, " :") == 0)
		end += 2;
	else
		return;
	data = xstrdup(end);
	len = remote_parse_unescape(data);
	if (rp->cb->output != NULL)
		rp->cb->output(rp->data, pane, (u_char *)data, len, age, 1);
	free(data);
}

static void
remote_parse_pause(struct remote_parser *rp, const char *rest)
{
	u_int	pane;

	if (remote_parse_id(&rest, '%', &pane) == 0 && rp->cb->pause != NULL)
		rp->cb->pause(rp->data, pane);
}

static void
remote_parse_continue(struct remote_parser *rp, const char *rest)
{
	u_int	pane;

	if (remote_parse_id(&rest, '%', &pane) == 0 && rp->cb->cont != NULL)
		rp->cb->cont(rp->data, pane);
}

static void
remote_parse_layout_change(struct remote_parser *rp, const char *rest)
{
	u_int	 window;
	char	*layout, *visible, *flags;

	if (remote_parse_id(&rest, '@', &window) != 0)
		return;
	layout = remote_parse_token(&rest);
	visible = remote_parse_token(&rest);
	flags = remote_parse_token(&rest);
	if (*layout != '\0' && rp->cb->layout_change != NULL) {
		rp->cb->layout_change(rp->data, window, layout,
		    *visible != '\0' ? visible : layout, flags);
	}
	free(layout);
	free(visible);
	free(flags);
}

static void
remote_parse_window_add(struct remote_parser *rp, const char *rest)
{
	u_int	window;

	if (remote_parse_id(&rest, '@', &window) == 0 &&
	    rp->cb->window_add != NULL)
		rp->cb->window_add(rp->data, window);
}

static void
remote_parse_window_close(struct remote_parser *rp, const char *rest)
{
	u_int	window;

	if (remote_parse_id(&rest, '@', &window) == 0 &&
	    rp->cb->window_close != NULL)
		rp->cb->window_close(rp->data, window);
}

static void
remote_parse_window_renamed(struct remote_parser *rp, const char *rest)
{
	u_int	window;

	if (remote_parse_id(&rest, '@', &window) == 0 &&
	    rp->cb->window_renamed != NULL)
		rp->cb->window_renamed(rp->data, window, rest);
}

static void
remote_parse_window_pane_changed(struct remote_parser *rp, const char *rest)
{
	u_int	window, pane;

	if (remote_parse_id(&rest, '@', &window) != 0)
		return;
	if (remote_parse_id(&rest, '%', &pane) != 0)
		return;
	if (rp->cb->window_pane_changed != NULL)
		rp->cb->window_pane_changed(rp->data, window, pane);
}

static void
remote_parse_session_changed(struct remote_parser *rp, const char *rest)
{
	u_int	session;

	if (remote_parse_id(&rest, '$', &session) == 0 &&
	    rp->cb->session_changed != NULL)
		rp->cb->session_changed(rp->data, session, rest);
}

static void
remote_parse_sessions_changed(struct remote_parser *rp,
    __unused const char *rest)
{
	if (rp->cb->sessions_changed != NULL)
		rp->cb->sessions_changed(rp->data);
}

static void
remote_parse_session_renamed(struct remote_parser *rp, const char *rest)
{
	u_int	session;

	if (remote_parse_id(&rest, '$', &session) == 0 &&
	    rp->cb->session_renamed != NULL)
		rp->cb->session_renamed(rp->data, session, rest);
}

static void
remote_parse_session_window_changed(struct remote_parser *rp,
    const char *rest)
{
	u_int	session, window;

	if (remote_parse_id(&rest, '$', &session) != 0)
		return;
	if (remote_parse_id(&rest, '@', &window) != 0)
		return;
	if (rp->cb->session_window_changed != NULL)
		rp->cb->session_window_changed(rp->data, session, window);
}

static void
remote_parse_pane_mode_changed(struct remote_parser *rp, const char *rest)
{
	u_int	pane;

	if (remote_parse_id(&rest, '%', &pane) == 0 &&
	    rp->cb->pane_mode_changed != NULL)
		rp->cb->pane_mode_changed(rp->data, pane);
}

/* %subscription-changed <name> $<s> @<w>|- <idx>|- %<p>|- : <value> */
static void
remote_parse_subscription_changed(struct remote_parser *rp, const char *rest)
{
	char	*name;
	u_int	 session;
	int	 window, idx, pane;

	name = remote_parse_token(&rest);
	if (remote_parse_id(&rest, '$', &session) != 0 ||
	    remote_parse_id_or_dash(&rest, '@', &window) != 0 ||
	    remote_parse_num_or_dash(&rest, &idx) != 0 ||
	    remote_parse_id_or_dash(&rest, '%', &pane) != 0) {
		free(name);
		return;
	}
	if (strncmp(rest, ": ", 2) == 0)
		rest += 2;
	else if (strcmp(rest, ":") == 0)
		rest += 1;
	else {
		free(name);
		return;
	}
	if (rp->cb->subscription_changed != NULL) {
		rp->cb->subscription_changed(rp->data, name, session, window,
		    idx, pane, rest);
	}
	free(name);
}

static void
remote_parse_exit(struct remote_parser *rp, const char *rest)
{
	if (rp->cb->exit != NULL)
		rp->cb->exit(rp->data, rest);
}

static void
remote_parse_ignore(__unused struct remote_parser *rp,
    __unused const char *rest)
{
}

/* Notification table, without the guards. */
static const struct {
	const char		*name;
	remote_parse_handler	 handler;
} remote_parse_table[] = {
	{ "output", remote_parse_output },
	{ "extended-output", remote_parse_extended_output },
	{ "pause", remote_parse_pause },
	{ "continue", remote_parse_continue },
	{ "layout-change", remote_parse_layout_change },
	{ "window-add", remote_parse_window_add },
	{ "window-close", remote_parse_window_close },
	{ "window-renamed", remote_parse_window_renamed },
	{ "window-pane-changed", remote_parse_window_pane_changed },
	{ "session-changed", remote_parse_session_changed },
	{ "sessions-changed", remote_parse_sessions_changed },
	{ "session-renamed", remote_parse_session_renamed },
	{ "session-window-changed", remote_parse_session_window_changed },
	{ "pane-mode-changed", remote_parse_pane_mode_changed },
	{ "subscription-changed", remote_parse_subscription_changed },
	{ "exit", remote_parse_exit },
	{ "unlinked-window-add", remote_parse_ignore },
	{ "unlinked-window-close", remote_parse_ignore },
	{ "unlinked-window-renamed", remote_parse_ignore },
	{ "client-session-changed", remote_parse_ignore },
	{ "client-detached", remote_parse_ignore },
	{ "paste-buffer-changed", remote_parse_ignore },
	{ "paste-buffer-deleted", remote_parse_ignore },
};

/*
 * Find the handler for a "%name rest" line. Fills in rest with the text after
 * the name (and one space). Returns NULL for an unknown name and for the
 * guards.
 */
static remote_parse_handler
remote_parse_lookup(const char *line, const char **rest)
{
	const char	*name = line + 1, *end;
	size_t		 len, i;

	end = strchr(name, ' ');
	if (end == NULL)
		len = strlen(name);
	else
		len = end - name;
	for (i = 0; i < nitems(remote_parse_table); i++) {
		if (strlen(remote_parse_table[i].name) != len ||
		    strncmp(remote_parse_table[i].name, name, len) != 0)
			continue;
		*rest = (end == NULL) ? name + len : end + 1;
		return (remote_parse_table[i].handler);
	}
	return (NULL);
}

/* Finish the current block. */
static void
remote_parse_finish_block(struct remote_parser *rp, int error, uint64_t t,
    u_int number, int flags)
{
	char	*body;
	size_t	 len = EVBUFFER_LENGTH(rp->body);

	body = xmalloc(len + 1);
	if (len != 0)
		memcpy(body, EVBUFFER_DATA(rp->body), len);
	body[len] = '\0';
	evbuffer_drain(rp->body, len);
	rp->in_block = 0;

	if (error) {
		if (rp->cb->error != NULL)
			rp->cb->error(rp->data, t, number, flags, body);
	} else {
		if (rp->cb->end != NULL)
			rp->cb->end(rp->data, t, number, flags, body);
	}
	free(body);
}

/* Handle one line. Owns nothing; the caller frees the line. */
static void
remote_parse_line(struct remote_parser *rp, char *line)
{
	size_t			 len = strlen(line);
	const char		*rest;
	remote_parse_handler	 handler;
	uint64_t		 t;
	u_int			 number;
	int			 flags;

	if (len != 0 && line[len - 1] == '\r')
		line[--len] = '\0';

	if (rp->in_block) {
		if (strncmp(line, "%end ", 5) == 0 &&
		    remote_parse_guard(line + 5, &t, &number, &flags) == 0 &&
		    number == rp->block_number) {
			remote_parse_finish_block(rp, 0, t, number, flags);
			return;
		}
		if (strncmp(line, "%error ", 7) == 0 &&
		    remote_parse_guard(line + 7, &t, &number, &flags) == 0 &&
		    number == rp->block_number) {
			remote_parse_finish_block(rp, 1, t, number, flags);
			return;
		}
		if (*line == '%' &&
		    (handler = remote_parse_lookup(line, &rest)) != NULL) {
			handler(rp, rest);
			return;
		}
		if (EVBUFFER_LENGTH(rp->body) != 0)
			evbuffer_add(rp->body, "\n", 1);
		evbuffer_add(rp->body, line, len);
		return;
	}

	if (strncmp(line, "%begin ", 7) == 0 &&
	    remote_parse_guard(line + 7, &t, &number, &flags) == 0) {
		rp->in_block = 1;
		rp->block_time = t;
		rp->block_number = number;
		rp->block_flags = flags;
		if (rp->cb->begin != NULL)
			rp->cb->begin(rp->data, t, number, flags);
		return;
	}
	if (*line == '%' && (handler = remote_parse_lookup(line, &rest)) != NULL) {
		handler(rp, rest);
		return;
	}
	if (rp->cb->unknown != NULL)
		rp->cb->unknown(rp->data, line);
}

/* Create a parser. */
struct remote_parser *
remote_parser_create(const struct remote_parse_callbacks *cb, void *data)
{
	struct remote_parser	*rp;

	rp = xcalloc(1, sizeof *rp);
	rp->cb = cb;
	rp->data = data;
	rp->body = evbuffer_new();
	if (rp->body == NULL)
		fatalx("out of memory");
	return (rp);
}

/* Free a parser. */
void
remote_parser_free(struct remote_parser *rp)
{
	evbuffer_free(rp->body);
	free(rp);
}

/* Drop any half-read block, for a new connection. */
void
remote_parser_reset(struct remote_parser *rp)
{
	evbuffer_drain(rp->body, EVBUFFER_LENGTH(rp->body));
	rp->in_block = 0;
}

/* Is the parser inside a reply block? */
int
remote_parser_in_block(struct remote_parser *rp)
{
	return (rp->in_block);
}

/*
 * Consume every complete line in the buffer. A partial line stays for the
 * next call, so the caller can feed data in any chunk size.
 */
void
remote_parse_feed(struct remote_parser *rp, struct evbuffer *evb)
{
	char	*line;

	for (;;) {
		line = evbuffer_readln(evb, NULL, EVBUFFER_EOL_LF);
		if (line == NULL)
			break;
		remote_parse_line(rp, line);
		free(line);
	}
}
