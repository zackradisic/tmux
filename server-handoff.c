/* $OpenBSD$ */

/*
 * Copyright (c) 2026 Zack Radisic
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
#include <sys/stat.h>

#include <netinet/in.h>
#include <resolv.h>

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"

/*
 * Restart the server in place without killing the pane processes.
 *
 * The panes are children of the server, so the server replaces itself with
 * execve(2) rather than forking a new process: execve keeps the pid, and so
 * keeps the parent/child relationship that waitpid(2), SIGCHLD and
 * #{pane_dead_status} all depend on.
 *
 * A pane dies today only because the server closes its pty master on exit,
 * which gives the shell EOF. The server sets no descriptor close-on-exec, so
 * the pty masters survive the exec for free; only their numbers need to
 * travel. They ride a state file together with the session tree, the layouts,
 * the options, the key bindings and the scrollback. The new image then adopts
 * each pty with SPAWN_ADOPT instead of calling forkpty(). Everything else is
 * made close-on-exec on the way out, so nothing from this image leaks into the
 * next one.
 *
 * The file is a flat, line-oriented, tab-separated text format. Fields are
 * escaped so that a value can hold a tab or a newline. Records are ordered,
 * and "session", "window" and "pane" open a context that "sessionend",
 * "windowend" and the next "pane" record close.
 */

#define HANDOFF_VERSION 1

/* What a pane's "deadflags" field can hold. */
#define HANDOFF_EXITED 0x1
#define HANDOFF_STATUSREADY 0x2
#define HANDOFF_STATUSDRAWN 0x4
#define HANDOFF_EMPTY 0x8

/* How long to wait for the clients to leave before we exec anyway. */
#define HANDOFF_CLIENT_TIMEOUT 5

/* State of a restart that is waiting for its clients to detach. */
static int	 handoff_pending;
static char	*handoff_binary;
static char	*handoff_state_path;
static time_t	 handoff_deadline;

/*
 * Commands that loaded the plugins, in the order they ran. Guest memory does
 * not survive the exec, so the new image runs them again.
 */
static char	**handoff_plugin_cmds;
static char	**handoff_plugin_names;
static u_int	 handoff_plugin_count;

/* Pane being rebuilt, held until the layout is applied. */
struct handoff_pane {
	struct window_pane	*wp;
	char			*replay;
	size_t			 replaylen;
	size_t			 replaysize;
	u_int			 cx;
	u_int			 cy;
	u_int			 rupper;
	u_int			 rlower;
	int			 mode;
	int			 haveline;
	int			 prevwrapped;
};

/* One saved key binding, held until the default bindings have run. */
struct handoff_bind {
	char	*table;
	char	*key;
	char	*note;
	char	*cmd;
	int	 flags;
};

struct handoff_binds {
	struct handoff_bind	*b;
	u_int			 n;
};

/* Reader state. */
struct handoff_ctx {
	const char		*path;
	u_int			 line;

	int			 socketfd;
	int			 haveversion;

	u_int			 nextsession;
	u_int			 nextwindow;
	u_int			 nextpane;

	char			**plugins;
	u_int			 nplugins;

	struct handoff_bind	*binds;
	u_int			 nbinds;

	/* Sessions the file holds, to tell "empty" from "all failed". */
	u_int			 nsessions;

	struct session		*s;
	struct winlink		*wl;
	struct window		*w;
	char			*layout;
	int			 zoomed;

	struct handoff_pane	*panes;
	u_int			 npanes;

	/* The active pane's id, looked up once every pane exists. */
	u_int			 activeid;

	/* The pane the option and screen records belong to. */
	struct handoff_pane	*curpane;

	/* Set when an array option has already been emptied. */
	struct options		*arrayowner;
	char			*arrayname;
};

static void	 handoff_field(FILE *, const char *);
static void	 handoff_number(FILE *, long long);
static void	 handoff_save_options(FILE *, const char *, const char *,
		     struct options *);
static void	 handoff_save_environ(FILE *, const char *, struct environ *);
static void	 handoff_save_pane(FILE *, struct window_pane *);
static int	 handoff_save(const char *, char **);
static void	 handoff_exec(void);

/*
 * Writing.
 */

/* Write a tab and then an escaped field. */
static void
handoff_field(FILE *f, const char *s)
{
	fputc('\t', f);
	if (s == NULL)
		return;
	for (; *s != '\0'; s++) {
		switch (*s) {
		case '\\':
			fputs("\\\\", f);
			break;
		case '\t':
			fputs("\\t", f);
			break;
		case '\n':
			fputs("\\n", f);
			break;
		case '\r':
			fputs("\\r", f);
			break;
		default:
			fputc((unsigned char)*s, f);
			break;
		}
	}
}

/* Write a tab and then a number. */
static void
handoff_number(FILE *f, long long n)
{
	fprintf(f, "\t%lld", n);
}

/* Write every option set on an options object. */
static void
handoff_save_options(FILE *f, const char *scalar, const char *array,
    struct options *oo)
{
	struct options_entry		*o;
	struct options_array_item	*a;
	const char			*name;
	char				*value;

	for (o = options_first(oo); o != NULL; o = options_next(o)) {
		name = options_name(o);
		if (!options_is_array(o)) {
			value = options_to_string(o, NULL, 0);
			fputs(scalar, f);
			handoff_field(f, name);
			handoff_field(f, value);
			fputc('\n', f);
			free(value);
			continue;
		}

		/*
		 * An empty array still needs a record: the new image starts
		 * from the built-in default, which may not be empty.
		 */
		a = options_array_first(o);
		if (a == NULL) {
			fputs(array, f);
			handoff_field(f, name);
			handoff_field(f, "");
			handoff_field(f, "");
			fputc('\n', f);
			continue;
		}
		for (; a != NULL; a = options_array_next(a)) {
			value = options_to_string(o,
			    options_array_item_key(a), 0);
			fputs(array, f);
			handoff_field(f, name);
			handoff_field(f, options_array_item_key(a));
			handoff_field(f, value);
			fputc('\n', f);
			free(value);
		}
	}
}

/* Write an environment. */
static void
handoff_save_environ(FILE *f, const char *key, struct environ *env)
{
	struct environ_entry	*ee;

	for (ee = environ_first(env); ee != NULL; ee = environ_next(ee)) {
		fputs(key, f);
		handoff_field(f, ee->name);
		handoff_field(f, ee->value);
		handoff_number(f, ee->flags);
		fputc('\n', f);
	}
}

/* Write every key binding, in every table. */
static void
handoff_save_key_bindings(FILE *f)
{
	struct key_table	*kt;
	struct key_binding	*bd;
	char			*cmd;

	for (kt = key_bindings_first_table(); kt != NULL;
	    kt = key_bindings_next_table(kt)) {
		for (bd = key_bindings_first(kt); bd != NULL;
		    bd = key_bindings_next(kt, bd)) {
			if (bd->cmdlist == NULL)
				continue;
			cmd = cmd_list_print(bd->cmdlist, 0);
			fputs("bind", f);
			handoff_field(f, kt->name);
			handoff_field(f, key_string_lookup_key(bd->key, 1));
			handoff_number(f, bd->flags);
			handoff_field(f, bd->note);
			handoff_field(f, cmd);
			fputc('\n', f);
			free(cmd);
		}
	}
}

/* Write the paste buffers. */
static void
handoff_save_buffers(FILE *f)
{
	struct paste_buffer	*pb;
	const char		*data;
	size_t			 size, need;
	char			*encoded;

	pb = NULL;
	while ((pb = paste_walk(pb)) != NULL) {
		data = paste_buffer_data(pb, &size);

		/* Base64 because a buffer may hold NUL bytes. */
		need = ((size + 2) / 3) * 4 + 1;
		encoded = xmalloc(need);
		if (b64_ntop((const u_char *)data, size, encoded, need) == -1) {
			free(encoded);
			continue;
		}

		fputs("buffer", f);
		handoff_field(f, paste_buffer_name(pb));
		handoff_number(f, paste_buffer_order(pb));
		handoff_field(f, encoded);
		fputc('\n', f);
		free(encoded);
	}
}

/*
 * Is this pane a floating one? While the window is zoomed every pane's cell
 * moves to saved_layout_cell, so window_pane_is_floating() says no for all of
 * them.
 */
static int
handoff_pane_is_floating(struct window_pane *wp)
{
	struct layout_cell	*lc = wp->layout_cell;

	if (lc == NULL)
		lc = wp->saved_layout_cell;
	if (lc == NULL || (~lc->flags & LAYOUT_CELL_FLOATING))
		return (0);
	return (1);
}

/*
 * Write one pane, then its options, then its screen. The scrollback goes out
 * as one "paneline" record per grid line, with the escape sequences that
 * capture-pane -e would emit, so that the new image can replay it through the
 * normal input parser.
 */
static void
handoff_save_pane(FILE *f, struct window_pane *wp)
{
	struct screen		*s = &wp->base;
	struct grid		*gd = s->grid;
	const struct grid_line	*gl;
	struct grid_cell	*gc = NULL;
	char			*line;
	u_int			 i, total, sx, flags;
	int			 wrapped, dead = 0;

	if (wp->flags & PANE_EXITED)
		dead |= HANDOFF_EXITED;
	if (wp->flags & PANE_STATUSREADY)
		dead |= HANDOFF_STATUSREADY;
	if (wp->flags & PANE_STATUSDRAWN)
		dead |= HANDOFF_STATUSDRAWN;
	if (wp->flags & PANE_EMPTY)
		dead |= HANDOFF_EMPTY;

	fputs("pane", f);
	handoff_number(f, wp->id);
	handoff_number(f, wp->fd);
	handoff_number(f, (long long)wp->pid);
	handoff_field(f, wp->tty);
	handoff_field(f, wp->cwd);
	handoff_field(f, wp->shell);
	handoff_number(f, dead);
	handoff_number(f, wp->status);
	handoff_number(f, handoff_pane_is_floating(wp));
	handoff_number(f, wp->xoff);
	handoff_number(f, wp->yoff);
	handoff_number(f, wp->sx);
	handoff_number(f, wp->sy);
	handoff_number(f, wp->argc);
	for (i = 0; i < (u_int)wp->argc; i++)
		handoff_field(f, wp->argv[i]);
	fputc('\n', f);

	handoff_save_options(f, "opt-pane", "arr-pane", wp->options);

	fputs("panescreen", f);
	handoff_number(f, s->cx);
	handoff_number(f, s->cy);
	handoff_number(f, s->mode);
	handoff_number(f, s->rupper);
	handoff_number(f, s->rlower);
	handoff_field(f, s->title);
	fputc('\n', f);

	sx = screen_size_x(s);
	total = gd->hsize + gd->sy;
	for (i = 0; i < total; i++) {
		gl = grid_peek_line(gd, i);
		wrapped = (gl->flags & GRID_LINE_WRAPPED) ? 1 : 0;

		/*
		 * A wrapped line has to keep every cell so that the replay
		 * wraps at the same column. An unwrapped one can lose its
		 * trailing spaces.
		 */
		flags = GRID_STRING_WITH_SEQUENCES|GRID_STRING_EMPTY_CELLS;
		if (!wrapped)
			flags |= GRID_STRING_TRIM_SPACES;

		/*
		 * grid_string_cells() may hand back a pointer to its own
		 * static cell through gc, so never free it.
		 */
		line = grid_string_cells(gd, 0, i, sx, &gc, flags, s);
		fputs("paneline", f);
		handoff_number(f, wrapped);
		handoff_field(f, line);
		fputc('\n', f);
		free(line);
	}
}

/* Write the whole server state. */
static int
handoff_save(const char *path, char **cause)
{
	FILE			*f;
	struct session		*s;
	struct winlink		*wl;
	struct window		*w;
	struct window_pane	*wp;
	u_int			*seen = NULL, nseen = 0, j;
	u_int			 nextw, nextp, i;
	int			 fd;

	fd = open(path, O_WRONLY|O_CREAT|O_TRUNC, 0600);
	if (fd == -1) {
		xasprintf(cause, "%s: %s", path, strerror(errno));
		return (-1);
	}
	f = fdopen(fd, "w");
	if (f == NULL) {
		xasprintf(cause, "%s: %s", path, strerror(errno));
		close(fd);
		return (-1);
	}

	window_get_next_ids(&nextw, &nextp);

	fputs("version", f);
	handoff_number(f, HANDOFF_VERSION);
	fputc('\n', f);

	fputs("socketfd", f);
	handoff_number(f, server_get_socket_fd());
	fputc('\n', f);

	fputs("starttime", f);
	handoff_number(f, (long long)start_time.tv_sec);
	handoff_number(f, (long long)start_time.tv_usec);
	fputc('\n', f);

	fputs("nextids", f);
	handoff_number(f, next_session_id);
	handoff_number(f, nextw);
	handoff_number(f, nextp);
	fputc('\n', f);

	handoff_save_options(f, "opt-server", "arr-server", global_options);
	handoff_save_options(f, "opt-gsession", "arr-gsession",
	    global_s_options);
	handoff_save_options(f, "opt-gwindow", "arr-gwindow", global_w_options);
	handoff_save_environ(f, "env-global", global_environ);
	handoff_save_key_bindings(f);
	handoff_save_buffers(f);

	for (i = 0; i < handoff_plugin_count; i++) {
		fputs("plugin", f);
		handoff_field(f, handoff_plugin_cmds[i]);
		fputc('\n', f);
	}

	/*
	 * A window may be linked into more than one session. Write it out
	 * under the first session that links it, and write a reference from
	 * the others.
	 */
	RB_FOREACH(s, sessions, &sessions) {
		fputs("session", f);
		handoff_number(f, s->id);
		handoff_field(f, s->name);
		handoff_field(f, s->cwd);
		handoff_number(f, (long long)s->creation_time.tv_sec);
		fputc('\n', f);

		handoff_save_options(f, "opt-session", "arr-session",
		    s->options);
		handoff_save_environ(f, "env-session", s->environ);

		RB_FOREACH(wl, winlinks, &s->windows) {
			w = wl->window;

			for (j = 0; j < nseen; j++) {
				if (seen[j] == w->id)
					break;
			}
			if (j != nseen) {
				fputs("link", f);
				handoff_number(f, wl->idx);
				handoff_number(f, w->id);
				fputc('\n', f);
				continue;
			}
			seen = xreallocarray(seen, nseen + 1, sizeof *seen);
			seen[nseen++] = w->id;

			fputs("window", f);
			handoff_number(f, wl->idx);
			handoff_number(f, w->id);
			handoff_field(f, w->name);
			handoff_number(f, w->sx);
			handoff_number(f, w->sy);
			handoff_number(f, w->xpixel);
			handoff_number(f, w->ypixel);
			handoff_number(f, (w->flags & WINDOW_ZOOMED) ? 1 : 0);
			fputc('\n', f);

			handoff_save_options(f, "opt-window", "arr-window",
			    w->options);

			if (w->layout_root != NULL) {
				char	*layout;

				/*
				 * Without the floating panes: layout_parse()
				 * cannot read them, and they are restored one
				 * at a time from their own records.
				 */
				layout = layout_dump_part(w,
				    w->saved_layout_root != NULL ?
				    w->saved_layout_root : w->layout_root, 0);
				fputs("layout", f);
				handoff_field(f, layout);
				fputc('\n', f);
				free(layout);
			}

			if (w->active != NULL) {
				fputs("wactive", f);
				handoff_number(f, w->active->id);
				fputc('\n', f);
			}

			/*
			 * The tiled panes first, because layout_parse() wants
			 * exactly as many panes as the layout has cells and it
			 * assigns them in list order. "tiledend" is where the
			 * reader applies the layout; the floating panes then
			 * arrive with their own geometry.
			 */
			TAILQ_FOREACH(wp, &w->panes, entry) {
				if (!handoff_pane_is_floating(wp))
					handoff_save_pane(f, wp);
			}
			fputs("tiledend", f);
			fputc('\n', f);
			TAILQ_FOREACH(wp, &w->panes, entry) {
				if (handoff_pane_is_floating(wp))
					handoff_save_pane(f, wp);
			}

			fputs("windowend", f);
			fputc('\n', f);
		}

		if (s->curw != NULL) {
			fputs("scurw", f);
			handoff_number(f, s->curw->idx);
			fputc('\n', f);
		}
		/*
		 * Backwards, because winlink_stack_push() puts each one at the
		 * head: replaying the list in order would reverse the stack.
		 */
		TAILQ_FOREACH_REVERSE(wl, &s->lastw, winlink_stack, sentry) {
			fputs("slastw", f);
			handoff_number(f, wl->idx);
			fputc('\n', f);
		}

		fputs("sessionend", f);
		fputc('\n', f);
	}

	free(seen);

	if (ferror(f) != 0) {
		xasprintf(cause, "%s: write failed", path);
		fclose(f);
		return (-1);
	}
	if (fclose(f) != 0) {
		xasprintf(cause, "%s: %s", path, strerror(errno));
		return (-1);
	}
	return (0);
}

/*
 * Exec.
 */

/*
 * Set close-on-exec on every descriptor except the ones the new image needs.
 * The pty masters must survive, because they are what keeps the pane processes
 * alive; the listening socket must survive, so that the clients reconnect to
 * the same socket without a window where it does not exist. Everything else -
 * the libevent internals, the client sockets, the plugin host - belongs to this
 * image only.
 */
static void
handoff_seal_fds(int socketfd)
{
	struct window		*w;
	struct window_pane	*wp;
	int			*keep = NULL, flags, fd, max;
	u_int			 nkeep = 0, i;

	keep = xreallocarray(keep, nkeep + 1, sizeof *keep);
	keep[nkeep++] = socketfd;
	RB_FOREACH(w, windows, &windows) {
		TAILQ_FOREACH(wp, &w->panes, entry) {
			if (wp->fd == -1)
				continue;
			keep = xreallocarray(keep, nkeep + 1, sizeof *keep);
			keep[nkeep++] = wp->fd;
		}
	}

	max = getdtablesize();
	if (max < 0 || max > 65536)
		max = 65536;
	for (fd = 3; fd < max; fd++) {
		flags = fcntl(fd, F_GETFD);
		if (flags == -1)
			continue;
		for (i = 0; i < nkeep; i++) {
			if (keep[i] == fd)
				break;
		}
		if (i != nkeep) {
			if (flags & FD_CLOEXEC)
				fcntl(fd, F_SETFD, flags & ~FD_CLOEXEC);
		} else if (~flags & FD_CLOEXEC)
			fcntl(fd, F_SETFD, flags|FD_CLOEXEC);
	}
	free(keep);
}

/* Write the state and replace this image. Only returns if exec failed. */
static void
handoff_exec(void)
{
	char		**argv;
	char		 *cause = NULL;
	u_int		  argc, i, level;
	int		  socketfd;

	socketfd = server_get_socket_fd();
	if (socketfd == -1) {
		log_debug("%s: no server socket", __func__);
		return;
	}

	if (handoff_save(handoff_state_path, &cause) != 0) {
		log_debug("%s: save failed: %s", __func__, cause);
		free(cause);
		return;
	}

	level = log_get_level();
	argc = 5 + level;
	argv = xcalloc(argc + 1, sizeof *argv);
	i = 0;
	argv[i++] = handoff_binary;
	argv[i++] = (char *)"-S";
	argv[i++] = (char *)socket_path;
	argv[i++] = (char *)"-Z";
	argv[i++] = handoff_state_path;
	while (level-- > 0)
		argv[i++] = (char *)"-v";
	argv[i] = NULL;

	log_debug("%s: exec %s for socket %s (fd %d)", __func__,
	    handoff_binary, socket_path, socketfd);

	handoff_seal_fds(socketfd);
	execv(handoff_binary, argv);

	/*
	 * execve() fails atomically, so this image is still intact and every
	 * pane is still running. The clients have already gone, though.
	 */
	log_debug("%s: execv %s failed: %s", __func__, handoff_binary,
	    strerror(errno));
	free(argv);
	unlink(handoff_state_path);
}

/*
 * Reading.
 */

/* Undo the field escaping, in place. */
static char *
handoff_unescape(char *s)
{
	char	*in = s, *out = s;

	while (*in != '\0') {
		if (*in != '\\') {
			*out++ = *in++;
			continue;
		}
		in++;
		switch (*in) {
		case 't':
			*out++ = '\t';
			in++;
			break;
		case 'n':
			*out++ = '\n';
			in++;
			break;
		case 'r':
			*out++ = '\r';
			in++;
			break;
		case '\0':
			break;
		default:
			*out++ = *in++;
			break;
		}
	}
	*out = '\0';
	return (s);
}

/* Split a line on tabs. The fields point into the line. */
static u_int
handoff_split(char *line, char ***fieldsp)
{
	char	**fields = NULL, *cp = line, *next;
	u_int	  n = 0;

	for (;;) {
		next = strchr(cp, '\t');
		if (next != NULL)
			*next = '\0';
		fields = xreallocarray(fields, n + 1, sizeof *fields);
		fields[n++] = handoff_unescape(cp);
		if (next == NULL)
			break;
		cp = next + 1;
	}
	*fieldsp = fields;
	return (n);
}

/* Read a number out of a field. */
static long long
handoff_num(const char *s, long long minval, long long maxval,
    long long fallback)
{
	long long	 n;
	const char	*errstr;

	n = strtonum(s, minval, maxval, &errstr);
	if (errstr != NULL)
		return (fallback);
	return (n);
}

/* Apply one saved option. */
static void
handoff_option(struct handoff_ctx *ctx, struct options *oo, const char *name,
    const char *key, const char *value)
{
	const struct options_table_entry	*oe;
	struct options_entry			*o;
	char					*cause = NULL;

	oe = options_search(name);
	if (oe == NULL && *name != '@') {
		log_debug("%s: %s:%u: unknown option %s", __func__, ctx->path,
		    ctx->line, name);
		return;
	}

	if (key == NULL) {
		/*
		 * A user option has no table entry, and options_from_string()
		 * reads the old value before it writes, so create it first.
		 */
		if (oe == NULL) {
			options_set_string(oo, name, 0, "%s", value);
			return;
		}
		if (options_from_string(oo, oe, name, value, 0, &cause) != 0) {
			log_debug("%s: %s:%u: %s: %s", __func__, ctx->path,
			    ctx->line, name, cause);
			free(cause);
		}
		return;
	}

	/*
	 * The first record for an array replaces it, because the new image
	 * starts from the built-in default. Records for one array are always
	 * next to each other.
	 */
	if (oe == NULL) {
		log_debug("%s: %s:%u: %s is not an array", __func__, ctx->path,
		    ctx->line, name);
		return;
	}
	if (ctx->arrayowner != oo || ctx->arrayname == NULL ||
	    strcmp(ctx->arrayname, name) != 0) {
		o = options_empty(oo, oe);
		ctx->arrayowner = oo;
		free(ctx->arrayname);
		ctx->arrayname = xstrdup(name);
	} else
		o = options_get_only(oo, name);
	if (o == NULL || *key == '\0')
		return;
	if (options_array_set(o, key, value, 0, &cause) != 0) {
		log_debug("%s: %s:%u: %s[%s]: %s", __func__, ctx->path,
		    ctx->line, name, key, cause);
		free(cause);
	}
}

/* Drop every key binding so that the saved set is the whole set. */
static void
handoff_clear_key_bindings(void)
{
	struct key_table	 *kt;
	struct key_binding	 *bd;
	char			**names = NULL;
	key_code		  key;
	u_int			  n = 0, i;

	for (kt = key_bindings_first_table(); kt != NULL;
	    kt = key_bindings_next_table(kt)) {
		names = xreallocarray(names, n + 1, sizeof *names);
		names[n++] = xstrdup(kt->name);
	}
	for (i = 0; i < n; i++) {
		kt = key_bindings_get_table(names[i], 0);
		while (kt != NULL) {
			bd = key_bindings_first(kt);
			if (bd == NULL)
				break;
			key = bd->key;

			/*
			 * The table itself goes away with its last binding
			 * when it has no defaults, so look it up again.
			 */
			key_bindings_remove(names[i], key);
			kt = key_bindings_get_table(names[i], 0);
		}
		free(names[i]);
	}
	free(names);
}

/* Add bytes to a pane's replay buffer. */
static void
handoff_append(struct handoff_pane *hp, const char *data, size_t len)
{
	while (hp->replaylen + len + 1 > hp->replaysize) {
		if (hp->replaysize == 0)
			hp->replaysize = 8192;
		else
			hp->replaysize *= 2;
		hp->replay = xrealloc(hp->replay, hp->replaysize);
	}
	memcpy(hp->replay + hp->replaylen, data, len);
	hp->replaylen += len;
	hp->replay[hp->replaylen] = '\0';
}

/*
 * Push the saved scrollback back through the input parser. The lines scroll
 * off the top of the screen into the history exactly as they did the first
 * time, so the pane ends up with the same history size and the same last
 * screen.
 */
static void
handoff_replay(struct handoff_pane *hp)
{
	struct window_pane	*wp = hp->wp;
	char			*seq;
	int			 n;

	if (wp->ictx == NULL)
		return;

	if (hp->replaylen != 0) {
		input_parse_buffer(wp, (u_char *)hp->replay,
		    hp->replaylen);
	}

	/* Reset the attributes and put the cursor back. */
	n = xasprintf(&seq, "\033[m\033[%u;%uH", hp->cy + 1, hp->cx + 1);
	input_parse_buffer(wp, (u_char *)seq, n);
	free(seq);

	/*
	 * The terminal modes go back last, because the replay changed them.
	 * A program in the pane may have turned on bracketed paste, mouse
	 * reporting or the application keypad, and it still thinks it has.
	 */
	wp->base.mode = hp->mode;
	if (hp->rlower < screen_size_y(&wp->base)) {
		wp->base.rupper = hp->rupper;
		wp->base.rlower = hp->rlower;
	}

	wp->flags &= ~(PANE_ACTIVITY|PANE_CHANGED|PANE_UNSEENCHANGES);
}

/* Free the per-window pane list. */
static void
handoff_free_panes(struct handoff_ctx *ctx)
{
	u_int	i;

	for (i = 0; i < ctx->npanes; i++)
		free(ctx->panes[i].replay);
	free(ctx->panes);
	ctx->panes = NULL;
	ctx->npanes = 0;
	ctx->curpane = NULL;
}

/*
 * Rebuild the layout by splitting, one pane at a time. This is the fallback
 * for a layout string the window cannot take, and it exists so that a pane
 * never ends up without a cell: layout_set_tiled() and the redraw code both
 * read wp->layout_cell without checking it.
 */
static void
handoff_layout_by_splitting(struct window *w)
{
	struct window_pane	*wp, *prev;
	struct layout_cell	*lc;
	u_int			 n, sy;

	n = window_count_panes(w, 1);
	prev = TAILQ_FIRST(&w->panes);
	if (prev == NULL)
		return;

	/*
	 * Make the window tall enough for every pane first, or a split runs
	 * out of room half way through. recalculate_sizes() puts the size
	 * back when a client attaches.
	 */
	sy = n * (PANE_MINIMUM + 1);
	if (sy > w->sy)
		window_resize(w, w->sx, sy, -1, -1);

	layout_init(w, prev);
	for (wp = TAILQ_NEXT(prev, entry); wp != NULL;
	    wp = TAILQ_NEXT(wp, entry)) {
		lc = layout_split_pane(prev, LAYOUT_TOPBOTTOM, -1, 0);
		if (lc == NULL) {
			log_debug("%s: @%u no room for %%%u", __func__, w->id,
			    wp->id);
			continue;
		}
		layout_assign_pane(lc, wp, 0);
		prev = wp;
	}
	layout_fix_offsets(w);
	layout_fix_panes(w, NULL);
	recalculate_sizes();
}

/* Apply the saved layout, once every tiled pane exists. */
static void
handoff_apply_layout(struct handoff_ctx *ctx)
{
	struct window	*w = ctx->w;
	char		*cause = NULL;

	if (w == NULL || ctx->layout == NULL)
		return;

	if (layout_parse(w, ctx->layout, &cause) != 0) {
		log_debug("%s: @%u layout: %s", __func__, w->id, cause);
		free(cause);
		handoff_layout_by_splitting(w);
	}
	free(ctx->layout);
	ctx->layout = NULL;
}

/* Finish a window: set the active pane, re-zoom and replay the scrollback. */
static void
handoff_close_window(struct handoff_ctx *ctx)
{
	struct window		*w = ctx->w;
	struct window_pane	*active = NULL;
	u_int			 i;

	if (w == NULL) {
		free(ctx->layout);
		ctx->layout = NULL;
		handoff_free_panes(ctx);
		return;
	}

	if (ctx->npanes == 0) {
		/* Nothing to put in it, so let the window go. */
		log_debug("%s: @%u has no panes", __func__, w->id);
		winlink_remove(&ctx->s->windows, ctx->wl);
		goto out;
	}

	/* A window with no floating panes never reached "tiledend". */
	handoff_apply_layout(ctx);

	for (i = 0; i < ctx->npanes; i++) {
		if (ctx->panes[i].wp->id == ctx->activeid)
			active = ctx->panes[i].wp;
	}
	if (active == NULL)
		active = TAILQ_FIRST(&w->panes);
	window_set_active_pane(w, active, 0);

	for (i = 0; i < ctx->npanes; i++)
		handoff_replay(&ctx->panes[i]);

	if (ctx->zoomed)
		window_zoom(w->active);

	w->flags &= ~WINDOW_ALERTFLAGS;

	events_fire_window("window-created", w);
	events_fire_winlink("window-linked", ctx->wl);

out:
	free(ctx->layout);
	ctx->layout = NULL;
	ctx->w = NULL;
	ctx->wl = NULL;
	ctx->zoomed = 0;
	ctx->activeid = 0;
	handoff_free_panes(ctx);
}

/* Finish a session. */
static void
handoff_close_session(struct handoff_ctx *ctx)
{
	handoff_close_window(ctx);
	if (ctx->s != NULL && ctx->s->curw == NULL)
		ctx->s->curw = RB_MIN(winlinks, &ctx->s->windows);
	ctx->s = NULL;
}

/* Create a session. */
static void
handoff_open_session(struct handoff_ctx *ctx, char **fields, u_int nfields)
{
	struct environ	*env;
	struct options	*oo;
	u_int		 id;

	handoff_close_session(ctx);
	if (nfields < 4)
		return;
	ctx->nsessions++;

	id = handoff_num(fields[1], 0, UINT_MAX, 0);
	env = environ_create();
	oo = options_create(global_s_options);

	/*
	 * session_create() takes the next id, so hand it the saved one: an id
	 * that changes across a restart breaks every #{session_id} the user
	 * has in a script.
	 */
	next_session_id = id;
	ctx->s = session_create(NULL, fields[2], fields[3], env, oo, NULL);
	if (nfields > 4) {
		ctx->s->creation_time.tv_sec = handoff_num(fields[4], 0,
		    LLONG_MAX, ctx->s->creation_time.tv_sec);
	}
	log_debug("%s: restored $%u %s", __func__, ctx->s->id, ctx->s->name);
}

/*
 * Create a window and link it. The panes arrive as separate records, so this
 * does what spawn_window() does minus the pane, which lets the layout and the
 * window options land before the first pane exists.
 */
static void
handoff_open_window(struct handoff_ctx *ctx, char **fields, u_int nfields)
{
	struct window	*w;
	u_int		 id, sx, sy, xpixel, ypixel;
	int		 idx;

	handoff_close_window(ctx);
	if (ctx->s == NULL || nfields < 9)
		return;

	idx = handoff_num(fields[1], INT_MIN, INT_MAX, 0);
	id = handoff_num(fields[2], 0, UINT_MAX, 0);
	sx = handoff_num(fields[4], 1, USHRT_MAX, 80);
	sy = handoff_num(fields[5], 1, USHRT_MAX, 24);
	xpixel = handoff_num(fields[6], 0, USHRT_MAX, 0);
	ypixel = handoff_num(fields[7], 0, USHRT_MAX, 0);
	ctx->zoomed = handoff_num(fields[8], 0, 1, 0);

	ctx->wl = winlink_add(&ctx->s->windows, idx);
	if (ctx->wl == NULL) {
		log_debug("%s: cannot add window %d", __func__, idx);
		return;
	}

	/* Take the saved window id, for the same reason as the session id. */
	window_set_next_id(id);
	w = window_create(sx, sy, xpixel, ypixel);
	if (w == NULL) {
		winlink_remove(&ctx->s->windows, ctx->wl);
		ctx->wl = NULL;
		return;
	}
	free(w->name);
	w->name = xstrdup(fields[3]);

	ctx->wl->session = ctx->s;
	winlink_set_window(ctx->wl, w);
	if (ctx->s->curw == NULL)
		ctx->s->curw = ctx->wl;
	ctx->w = w;

	log_debug("%s: restored @%u %s at %d", __func__, w->id, w->name, idx);
}

/* Link a window that another session already restored. */
static void
handoff_link_window(struct handoff_ctx *ctx, char **fields, u_int nfields)
{
	struct window	*w;
	char		*cause = NULL;
	int		 idx;

	handoff_close_window(ctx);
	if (ctx->s == NULL || nfields < 3)
		return;

	idx = handoff_num(fields[1], INT_MIN, INT_MAX, 0);
	w = window_find_by_id(handoff_num(fields[2], 0, UINT_MAX, 0));
	if (w == NULL) {
		log_debug("%s: no window %s", __func__, fields[2]);
		return;
	}
	if (session_attach(ctx->s, w, idx, &cause) == NULL) {
		log_debug("%s: %s", __func__, cause);
		free(cause);
	}
}

/* Adopt one pane. */
static void
handoff_open_pane(struct handoff_ctx *ctx, char **fields, u_int nfields)
{
	struct spawn_context	 sc;
	struct handoff_pane	*hp;
	struct window_pane	*wp;
	char			*cause = NULL;
	struct layout_geometry	 lg;
	struct window_pane	*last;
	u_int			 id, i;
	int			 argc = 0, dead, status, floating;

	ctx->curpane = NULL;
	if (ctx->w == NULL || nfields < 15)
		return;

	id = handoff_num(fields[1], 0, UINT_MAX, 0);
	dead = handoff_num(fields[7], 0, INT_MAX, 0);
	status = handoff_num(fields[8], INT_MIN, INT_MAX, 0);
	floating = handoff_num(fields[9], 0, 1, 0);
	lg.xoff = handoff_num(fields[10], INT_MIN, INT_MAX, 0);
	lg.yoff = handoff_num(fields[11], INT_MIN, INT_MAX, 0);
	lg.sx = handoff_num(fields[12], 1, USHRT_MAX, 1);
	lg.sy = handoff_num(fields[13], 1, USHRT_MAX, 1);
	argc = handoff_num(fields[14], 0, 1024, 0);
	if (nfields < 15 + (u_int)argc)
		argc = nfields - 15;

	memset(&sc, 0, sizeof sc);
	sc.s = ctx->s;
	sc.wl = ctx->wl;
	sc.idx = -1;
	sc.cwd = fields[5];
	sc.adopt_fd = handoff_num(fields[2], -1, INT_MAX, -1);
	sc.adopt_pid = handoff_num(fields[3], -1, INT_MAX, -1);
	sc.adopt_tty = fields[4];
	sc.flags = SPAWN_ADOPT|SPAWN_DETACHED|SPAWN_NONOTIFY;
	if (argc > 0) {
		sc.argc = argc;
		sc.argv = xcalloc(argc, sizeof *sc.argv);
		for (i = 0; i < (u_int)argc; i++)
			sc.argv[i] = fields[15 + i];
	}

	/*
	 * A floating pane arrives after the layout, and brings its own cell
	 * with the geometry it had. Without a cell it would count towards the
	 * tiled layout and make layout_parse() reject it.
	 */
	if (floating) {
		/*
		 * Hang the cell off the last pane that has one, the way
		 * split-window does. Passing NULL instead would wrap the
		 * layout root in another node and resize the window with it;
		 * taking the last pane keeps the floating cells together at
		 * the end of the tree, where layout_dump() puts them.
		 */
		sc.wp0 = NULL;
		TAILQ_FOREACH(last, &ctx->w->panes, entry) {
			if (last->layout_cell != NULL)
				sc.wp0 = last;
		}
		if (sc.wp0 == NULL) {
			log_debug("%s: %%%u: nothing to float beside", __func__,
			    id);
			free(sc.argv);
			return;
		}
		sc.lc = layout_floating_pane(ctx->w, sc.wp0, &lg);
		if (sc.lc == NULL) {
			log_debug("%s: %%%u: no floating cell", __func__, id);
			free(sc.argv);
			return;
		}
		sc.flags |= SPAWN_FLOATING;
	}

	/* Take the saved pane id, for the same reason as the session id. */
	window_set_next_pane_id(id);

	wp = spawn_pane(&sc, &cause);
	free(sc.argv);
	if (wp == NULL) {
		log_debug("%s: %%%u: %s", __func__, id, cause);
		free(cause);
		return;
	}
	if (*fields[6] != '\0') {
		free(wp->shell);
		wp->shell = xstrdup(fields[6]);
	}

	/*
	 * Put back what spawn_pane() cleared. A pane whose process had already
	 * gone must come back dead, with the same exit status, or
	 * #{pane_dead_status} and remain-on-exit both go wrong.
	 */
	wp->status = status;
	if (dead & HANDOFF_EXITED)
		wp->flags |= PANE_EXITED;
	if (dead & HANDOFF_STATUSREADY)
		wp->flags |= PANE_STATUSREADY;
	if (dead & HANDOFF_STATUSDRAWN)
		wp->flags |= PANE_STATUSDRAWN;
	if (dead & HANDOFF_EMPTY) {
		wp->flags |= PANE_EMPTY;
		wp->base.mode &= ~MODE_CURSOR;
		wp->base.mode |= MODE_CRLF;
	}

	ctx->panes = xreallocarray(ctx->panes, ctx->npanes + 1,
	    sizeof *ctx->panes);
	hp = &ctx->panes[ctx->npanes++];
	memset(hp, 0, sizeof *hp);
	hp->wp = wp;
	hp->mode = wp->base.mode;
	hp->rlower = screen_size_y(&wp->base) - 1;
	ctx->curpane = hp;

	log_debug("%s: adopted %%%u pid %ld on fd %d", __func__, wp->id,
	    (long)wp->pid, wp->fd);
}

/* Handle one record. Returns -1 only for an unusable file. */
static int
handoff_record(struct handoff_ctx *ctx, char **fields, u_int nfields)
{
	const char		*key = fields[0];
	struct handoff_pane	*hp;
	u_char			*data;
	size_t			 size;

#define F(n) ((n) < nfields ? fields[n] : (char *)"")

	if (strcmp(key, "version") == 0) {
		if (handoff_num(F(1), 0, INT_MAX, -1) != HANDOFF_VERSION)
			return (-1);
		ctx->haveversion = 1;
		return (0);
	}
	if (!ctx->haveversion) {
		/* The version record comes first, so the file is truncated. */
		return (-1);
	}
	if (strcmp(key, "socketfd") == 0) {
		ctx->socketfd = handoff_num(F(1), -1, INT_MAX, -1);
		return (0);
	}
	if (strcmp(key, "starttime") == 0) {
		start_time.tv_sec = handoff_num(F(1), 0, LLONG_MAX,
		    start_time.tv_sec);
		start_time.tv_usec = handoff_num(F(2), 0, LLONG_MAX, 0);
		return (0);
	}
	if (strcmp(key, "nextids") == 0) {
		/*
		 * Remembered and applied at the end: creating the objects
		 * moves the counters, and each one takes its own saved id.
		 */
		ctx->nextsession = handoff_num(F(1), 0, UINT_MAX, 0);
		ctx->nextwindow = handoff_num(F(2), 0, UINT_MAX, 0);
		ctx->nextpane = handoff_num(F(3), 0, UINT_MAX, 0);
		return (0);
	}

	if (strcmp(key, "opt-server") == 0)
		handoff_option(ctx, global_options, F(1), NULL, F(2));
	else if (strcmp(key, "arr-server") == 0)
		handoff_option(ctx, global_options, F(1), F(2), F(3));
	else if (strcmp(key, "opt-gsession") == 0)
		handoff_option(ctx, global_s_options, F(1), NULL, F(2));
	else if (strcmp(key, "arr-gsession") == 0)
		handoff_option(ctx, global_s_options, F(1), F(2), F(3));
	else if (strcmp(key, "opt-gwindow") == 0)
		handoff_option(ctx, global_w_options, F(1), NULL, F(2));
	else if (strcmp(key, "arr-gwindow") == 0)
		handoff_option(ctx, global_w_options, F(1), F(2), F(3));
	else if (strcmp(key, "opt-session") == 0) {
		if (ctx->s != NULL)
			handoff_option(ctx, ctx->s->options, F(1), NULL, F(2));
	} else if (strcmp(key, "arr-session") == 0) {
		if (ctx->s != NULL)
			handoff_option(ctx, ctx->s->options, F(1), F(2), F(3));
	} else if (strcmp(key, "opt-window") == 0) {
		if (ctx->w != NULL)
			handoff_option(ctx, ctx->w->options, F(1), NULL, F(2));
	} else if (strcmp(key, "arr-window") == 0) {
		if (ctx->w != NULL)
			handoff_option(ctx, ctx->w->options, F(1), F(2), F(3));
	} else if (strcmp(key, "opt-pane") == 0) {
		if (ctx->curpane != NULL) {
			handoff_option(ctx, ctx->curpane->wp->options, F(1),
			    NULL, F(2));
		}
	} else if (strcmp(key, "arr-pane") == 0) {
		if (ctx->curpane != NULL) {
			handoff_option(ctx, ctx->curpane->wp->options, F(1),
			    F(2), F(3));
		}
	} else if (strcmp(key, "env-global") == 0) {
		environ_set(global_environ, F(1),
		    handoff_num(F(3), 0, INT_MAX, 0), "%s", F(2));
	} else if (strcmp(key, "env-session") == 0) {
		if (ctx->s != NULL) {
			environ_set(ctx->s->environ, F(1),
			    handoff_num(F(3), 0, INT_MAX, 0), "%s", F(2));
		}
	} else if (strcmp(key, "bind") == 0) {
		struct handoff_bind	*hb;

		ctx->binds = xreallocarray(ctx->binds, ctx->nbinds + 1,
		    sizeof *ctx->binds);
		hb = &ctx->binds[ctx->nbinds++];
		hb->table = xstrdup(F(1));
		hb->key = xstrdup(F(2));
		hb->flags = handoff_num(F(3), 0, INT_MAX, 0);
		hb->note = (*F(4) == '\0' ? NULL : xstrdup(F(4)));
		hb->cmd = xstrdup(F(5));
	} else if (strcmp(key, "buffer") == 0) {
		char	*cause = NULL;
		int	 len;

		size = strlen(F(3));
		data = xmalloc(size + 1);
		len = b64_pton(F(3), data, size + 1);
		if (len <= 0)
			free(data);
		else if (paste_set((char *)data, len, F(1), &cause) != 0) {
			log_debug("%s: %s:%u: %s", __func__, ctx->path,
			    ctx->line, cause);
			free(cause);
			free(data);
		}
	} else if (strcmp(key, "plugin") == 0) {
		ctx->plugins = xreallocarray(ctx->plugins, ctx->nplugins + 1,
		    sizeof *ctx->plugins);
		ctx->plugins[ctx->nplugins++] = xstrdup(F(1));
	} else if (strcmp(key, "session") == 0)
		handoff_open_session(ctx, fields, nfields);
	else if (strcmp(key, "window") == 0)
		handoff_open_window(ctx, fields, nfields);
	else if (strcmp(key, "link") == 0)
		handoff_link_window(ctx, fields, nfields);
	else if (strcmp(key, "layout") == 0) {
		free(ctx->layout);
		ctx->layout = xstrdup(F(1));
	} else if (strcmp(key, "wactive") == 0)
		ctx->activeid = handoff_num(F(1), 0, UINT_MAX, 0);
	else if (strcmp(key, "pane") == 0)
		handoff_open_pane(ctx, fields, nfields);
	else if (strcmp(key, "panescreen") == 0) {
		if ((hp = ctx->curpane) != NULL) {
			hp->cx = handoff_num(F(1), 0, USHRT_MAX, 0);
			hp->cy = handoff_num(F(2), 0, USHRT_MAX, 0);
			hp->mode = handoff_num(F(3), INT_MIN, INT_MAX,
			    hp->wp->base.mode);
			hp->rupper = handoff_num(F(4), 0, USHRT_MAX, 0);
			hp->rlower = handoff_num(F(5), 0, USHRT_MAX, 0);
			if (*F(6) != '\0')
				screen_set_title(&hp->wp->base, F(6), 0);
		}
	} else if (strcmp(key, "paneline") == 0) {
		if ((hp = ctx->curpane) != NULL) {
			if (hp->haveline && !hp->prevwrapped)
				handoff_append(hp, "\r\n", 2);
			handoff_append(hp, F(2), strlen(F(2)));
			hp->haveline = 1;
			hp->prevwrapped = handoff_num(F(1), 0, 1, 0);
		}
	} else if (strcmp(key, "tiledend") == 0)
		handoff_apply_layout(ctx);
	else if (strcmp(key, "windowend") == 0)
		handoff_close_window(ctx);
	else if (strcmp(key, "scurw") == 0) {
		if (ctx->s != NULL) {
			struct winlink	*wl;

			wl = winlink_find_by_index(&ctx->s->windows,
			    handoff_num(F(1), INT_MIN, INT_MAX, 0));
			if (wl != NULL)
				ctx->s->curw = wl;
		}
	} else if (strcmp(key, "slastw") == 0) {
		if (ctx->s != NULL) {
			struct winlink	*wl;

			wl = winlink_find_by_index(&ctx->s->windows,
			    handoff_num(F(1), INT_MIN, INT_MAX, 0));
			if (wl != NULL)
				winlink_stack_push(&ctx->s->lastw, wl);
		}
	} else if (strcmp(key, "sessionend") == 0)
		handoff_close_session(ctx);
	else {
		log_debug("%s: %s:%u: unknown record %s", __func__, ctx->path,
		    ctx->line, key);
	}

#undef F
	return (0);
}

/*
 * Install the saved key bindings. This runs from the command queue, after the
 * default bindings that key_bindings_init() queued, because the saved set is
 * the whole set: a key the user unbound at runtime must stay unbound.
 */
static enum cmd_retval
handoff_apply_binds(__unused struct cmdq_item *item, void *data)
{
	struct handoff_binds	*hbs = data;
	struct handoff_bind	*hb;
	struct cmd_parse_result	*pr;
	u_int			 i;

	handoff_clear_key_bindings();

	for (i = 0; i < hbs->n; i++) {
		hb = &hbs->b[i];

		pr = cmd_parse_from_string(hb->cmd, NULL);
		if (pr->status != CMD_PARSE_SUCCESS) {
			log_debug("%s: %s: %s", __func__, hb->key, pr->error);
			free(pr->error);
		} else {
			key_bindings_add(hb->table,
			    key_string_lookup_string(hb->key), hb->note,
			    hb->flags, pr->cmdlist);
		}

		free(hb->table);
		free(hb->key);
		free(hb->note);
		free(hb->cmd);
	}

	log_debug("%s: %u bindings", __func__, hbs->n);
	free(hbs->b);
	free(hbs);
	return (CMD_RETURN_NORMAL);
}

/* Read the state file and rebuild the server. */
int
server_handoff_restore(const char *path, int *socketfd, char **cause)
{
	struct handoff_ctx	 ctx;
	struct handoff_binds	*hbs;
	FILE			*f;
	char			*line = NULL, **fields;
	size_t			 linesize = 0;
	ssize_t			 linelen;
	u_int			 nfields, i;
	int			 rc = 0;

	memset(&ctx, 0, sizeof ctx);
	ctx.path = path;
	ctx.socketfd = -1;

	f = fopen(path, "r");
	if (f == NULL) {
		xasprintf(cause, "%s: %s", path, strerror(errno));
		return (-1);
	}

	while ((linelen = getline(&line, &linesize, f)) != -1) {
		ctx.line++;
		if (linelen > 0 && line[linelen - 1] == '\n')
			line[linelen - 1] = '\0';
		if (*line == '\0')
			continue;

		nfields = handoff_split(line, &fields);
		rc = handoff_record(&ctx, fields, nfields);
		free(fields);
		if (rc != 0) {
			xasprintf(cause, "%s:%u: unusable state file", path,
			    ctx.line);
			break;
		}
	}
	free(line);
	fclose(f);

	handoff_close_session(&ctx);

	if (rc == 0 && !ctx.haveversion) {
		xasprintf(cause, "%s: no version record", path);
		rc = -1;
	}
	if (rc == 0 && ctx.nsessions != 0 && RB_EMPTY(&sessions)) {
		xasprintf(cause, "%s: none of the %u sessions came back", path,
		    ctx.nsessions);
		rc = -1;
	}

	if (rc == 0) {
		/*
		 * Put the id counters back where they were, so that the next
		 * object made gets an id that never existed before.
		 */
		next_session_id = ctx.nextsession;
		window_set_next_id(ctx.nextwindow);
		window_set_next_pane_id(ctx.nextpane);

		*socketfd = ctx.socketfd;

		/*
		 * The bindings go on the queue behind the defaults that
		 * key_bindings_init() put there.
		 */
		hbs = xcalloc(1, sizeof *hbs);
		hbs->b = ctx.binds;
		hbs->n = ctx.nbinds;
		ctx.binds = NULL;
		ctx.nbinds = 0;
		cmdq_append(NULL, cmdq_get_callback(handoff_apply_binds, hbs));

		/* Reload the plugins. Their guest memory did not survive. */
		for (i = 0; i < ctx.nplugins; i++) {
			struct cmd_parse_result	*pr;

			log_debug("%s: %s", __func__, ctx.plugins[i]);
			pr = cmd_parse_from_string(ctx.plugins[i], NULL);
			if (pr->status != CMD_PARSE_SUCCESS) {
				log_debug("%s: %s", __func__, pr->error);
				free(pr->error);
				continue;
			}
			cmdq_append(NULL, cmdq_get_command(pr->cmdlist, NULL));
			cmd_list_free(pr->cmdlist);
		}
	}

	for (i = 0; i < ctx.nplugins; i++)
		free(ctx.plugins[i]);
	free(ctx.plugins);
	for (i = 0; i < ctx.nbinds; i++) {
		free(ctx.binds[i].table);
		free(ctx.binds[i].key);
		free(ctx.binds[i].note);
		free(ctx.binds[i].cmd);
	}
	free(ctx.binds);
	free(ctx.arrayname);
	free(ctx.layout);

	return (rc);
}

/*
 * Plugin bookkeeping. The commands that loaded the plugins run again in the
 * new image, in the order they ran here.
 */

void
server_handoff_record_plugin(const char *name, const char *cmd)
{
	u_int	i;

	if (name != NULL) {
		for (i = 0; i < handoff_plugin_count; i++) {
			if (handoff_plugin_names[i] != NULL &&
			    strcmp(handoff_plugin_names[i], name) == 0) {
				free(handoff_plugin_cmds[i]);
				handoff_plugin_cmds[i] = xstrdup(cmd);
				return;
			}
		}
	}

	handoff_plugin_cmds = xreallocarray(handoff_plugin_cmds,
	    handoff_plugin_count + 1, sizeof *handoff_plugin_cmds);
	handoff_plugin_names = xreallocarray(handoff_plugin_names,
	    handoff_plugin_count + 1, sizeof *handoff_plugin_names);
	handoff_plugin_cmds[handoff_plugin_count] = xstrdup(cmd);
	handoff_plugin_names[handoff_plugin_count] =
	    (name == NULL ? NULL : xstrdup(name));
	handoff_plugin_count++;
}

void
server_handoff_forget_plugin(const char *name)
{
	u_int	i, j;

	for (i = 0; i < handoff_plugin_count; i++) {
		if (handoff_plugin_names[i] == NULL ||
		    strcmp(handoff_plugin_names[i], name) != 0)
			continue;
		free(handoff_plugin_cmds[i]);
		free(handoff_plugin_names[i]);
		for (j = i; j + 1 < handoff_plugin_count; j++) {
			handoff_plugin_cmds[j] = handoff_plugin_cmds[j + 1];
			handoff_plugin_names[j] = handoff_plugin_names[j + 1];
		}
		handoff_plugin_count--;
		return;
	}
}

/*
 * Starting a restart.
 */

/* Work out which binary to exec. */
static char *
handoff_find_binary(const char *wanted, char **cause)
{
	char	 path[PATH_MAX], *deleted;
	ssize_t	 n;

	if (wanted != NULL && *wanted != '\0') {
		if (access(wanted, X_OK) != 0) {
			xasprintf(cause, "%s: %s", wanted, strerror(errno));
			return (NULL);
		}
		return (xstrdup(wanted));
	}

	/*
	 * Our own path, which is the whole point: a rebuild replaces the file
	 * there, so this picks up the new binary. Linux marks the link
	 * "<path> (deleted)" once the old file is gone, and that suffix has to
	 * come off - what we want is the name, not the file we are running.
	 */
	n = readlink("/proc/self/exe", path, (sizeof path) - 1);
	if (n > 0) {
		path[n] = '\0';
		deleted = strstr(path, " (deleted)");
		if (deleted != NULL && deleted[sizeof " (deleted)" - 1] == '\0')
			*deleted = '\0';
		if (access(path, X_OK) == 0)
			return (xstrdup(path));
	}
	if (tmux_binary != NULL && access(tmux_binary, X_OK) == 0)
		return (xstrdup(tmux_binary));

	xasprintf(cause, "cannot find a tmux binary to run, give one with -b");
	return (NULL);
}

/*
 * Quote a string for the shell that runs the client's replacement command.
 * Single quotes protect everything, and a literal quote becomes '\''.
 */
static char *
handoff_shell_quote(const char *s)
{
	char	*out, *cp;
	size_t	 size;

	size = 4 * strlen(s) + 3;
	out = xmalloc(size);
	cp = out;

	*cp++ = '\'';
	for (; *s != '\0'; s++) {
		if (*s != '\'') {
			*cp++ = *s;
			continue;
		}
		*cp++ = '\'';
		*cp++ = '\\';
		*cp++ = '\'';
		*cp++ = '\'';
	}
	*cp++ = '\'';
	*cp = '\0';

	return (out);
}

/* Send each client away, with the command that brings it back. */
static void
handoff_release_clients(const char *binary)
{
	struct client	*c, *c1;
	char		*cmd, *qbinary, *qsocket, *target, *name;

	qbinary = handoff_shell_quote(binary);
	qsocket = handoff_shell_quote(socket_path);

	TAILQ_FOREACH_SAFE(c, &clients, entry, c1) {
		/*
		 * A client with no session is running a command, and it goes
		 * away by itself once the command finishes. Leave it alone.
		 */
		if (c->session == NULL)
			continue;

		/*
		 * A control client drives another program, so an interactive
		 * attach is no use to it. Detach it and let whatever owns it
		 * reconnect.
		 */
		if (c->flags & CLIENT_CONTROL) {
			server_client_detach(c, MSG_DETACH);
			continue;
		}

		xasprintf(&name, "$%u", c->session->id);
		target = handoff_shell_quote(name);
		xasprintf(&cmd, "exec %s -S %s attach -t %s", qbinary, qsocket,
		    target);
		free(name);
		log_debug("%s: client %p: %s", __func__, c, cmd);
		server_client_exec(c, cmd);
		proc_flush_peer(c->peer);
		free(cmd);
		free(target);
	}

	free(qsocket);
	free(qbinary);
}

/* Start a restart. The exec happens once the clients have gone. */
int
server_handoff_begin(const char *binary, char **cause)
{
	char	*found;

	if (handoff_pending) {
		xasprintf(cause, "a restart is already in progress");
		return (-1);
	}

	found = handoff_find_binary(binary, cause);
	if (found == NULL)
		return (-1);

	free(handoff_state_path);
	xasprintf(&handoff_state_path, "%s.handoff", socket_path);

	handoff_binary = found;
	handoff_deadline = time(NULL) + HANDOFF_CLIENT_TIMEOUT;
	handoff_pending = 1;

	handoff_release_clients(handoff_binary);
	return (0);
}

int
server_handoff_pending(void)
{
	return (handoff_pending);
}

/*
 * Called from the server loop. Execs once every client has gone, or once the
 * timeout runs out. Only returns if the exec failed.
 */
void
server_handoff_check(void)
{
	if (!handoff_pending)
		return;
	if (!TAILQ_EMPTY(&clients) && time(NULL) < handoff_deadline)
		return;

	if (!TAILQ_EMPTY(&clients))
		log_debug("%s: clients are still here, going anyway", __func__);

	handoff_exec();

	/* The exec failed, so stay where we are. */
	handoff_pending = 0;
	free(handoff_binary);
	handoff_binary = NULL;
}
