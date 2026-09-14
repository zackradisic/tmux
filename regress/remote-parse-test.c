/*
 * Harness for remote-parse.c. Reads a control mode transcript, feeds it to
 * the parser in chunks of the given size and prints one line per callback,
 * so the output can be compared for every chunk size.
 *
 *   remote-parse-test <chunk> <file>
 *
 * Links remote-parse.c and xmalloc.c alone, so it supplies the log and fatal
 * functions those need.
 */

#include <sys/types.h>

#include <errno.h>
#include <event.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "tmux.h"

/* Stubs for what xmalloc.c wants from log.c. */
void
log_debug(__unused const char *fmt, ...)
{
}

__dead void
fatal(const char *fmt, ...)
{
	va_list	ap;

	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fprintf(stderr, ": %s\n", strerror(errno));
	exit(1);
}

__dead void
fatalx(const char *fmt, ...)
{
	va_list	ap;

	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fprintf(stderr, "\n");
	exit(1);
}

/* Print bytes with C escapes so that the result is one line. */
static void
print_bytes(const u_char *data, size_t len)
{
	size_t	i;

	for (i = 0; i < len; i++) {
		if (data[i] == '\\')
			printf("\\\\");
		else if (data[i] >= ' ' && data[i] < 0x7f)
			putchar(data[i]);
		else
			printf("\\x%02x", data[i]);
	}
}

/* Print a body with newlines shown as \n. */
static void
print_body(const char *body)
{
	for (; *body != '\0'; body++) {
		if (*body == '\n')
			printf("\\n");
		else
			putchar(*body);
	}
}

static void
cb_begin(__unused void *data, uint64_t t, u_int number, int flags)
{
	printf("begin %llu %u %d\n", (unsigned long long)t, number, flags);
}

static void
cb_end(__unused void *data, uint64_t t, u_int number, int flags,
    const char *body)
{
	printf("end %llu %u %d [", (unsigned long long)t, number, flags);
	print_body(body);
	printf("]\n");
}

static void
cb_error(__unused void *data, uint64_t t, u_int number, int flags,
    const char *body)
{
	printf("error %llu %u %d [", (unsigned long long)t, number, flags);
	print_body(body);
	printf("]\n");
}

static void
cb_output(__unused void *data, u_int pane, const u_char *bytes, size_t len,
    uint64_t age, int extended)
{
	printf("output %%%u ext=%d age=%llu len=%zu [", pane, extended,
	    (unsigned long long)age, len);
	print_bytes(bytes, len);
	printf("]\n");
}

static void
cb_pause(__unused void *data, u_int pane)
{
	printf("pause %%%u\n", pane);
}

static void
cb_continue(__unused void *data, u_int pane)
{
	printf("continue %%%u\n", pane);
}

static void
cb_layout_change(__unused void *data, u_int window, const char *layout,
    const char *visible, const char *flags)
{
	char	*stripped;
	u_int	*ids, n, i;

	printf("layout-change @%u [%s] [%s] [%s]\n", window, layout, visible,
	    flags);
	stripped = remote_parse_layout_strip(layout);
	ids = remote_parse_layout_leaf_ids(stripped, &n);
	printf("  stripped [%s] leaves", stripped);
	for (i = 0; i < n; i++)
		printf(" %%%u", ids[i]);
	printf("\n");
	free(ids);
	free(stripped);
}

static void
cb_window_add(__unused void *data, u_int window)
{
	printf("window-add @%u\n", window);
}

static void
cb_window_close(__unused void *data, u_int window)
{
	printf("window-close @%u\n", window);
}

static void
cb_window_renamed(__unused void *data, u_int window, const char *name)
{
	printf("window-renamed @%u [%s]\n", window, name);
}

static void
cb_window_pane_changed(__unused void *data, u_int window, u_int pane)
{
	printf("window-pane-changed @%u %%%u\n", window, pane);
}

static void
cb_session_changed(__unused void *data, u_int session, const char *name)
{
	printf("session-changed $%u [%s]\n", session, name);
}

static void
cb_sessions_changed(__unused void *data)
{
	printf("sessions-changed\n");
}

static void
cb_session_renamed(__unused void *data, u_int session, const char *name)
{
	printf("session-renamed $%u [%s]\n", session, name);
}

static void
cb_session_window_changed(__unused void *data, u_int session, u_int window)
{
	printf("session-window-changed $%u @%u\n", session, window);
}

static void
cb_pane_mode_changed(__unused void *data, u_int pane)
{
	printf("pane-mode-changed %%%u\n", pane);
}

static void
cb_subscription_changed(__unused void *data, const char *name, u_int session,
    int window, int idx, int pane, const char *value)
{
	printf("subscription-changed %s $%u %d %d %d [%s]\n", name, session,
	    window, idx, pane, value);
}

static void
cb_exit(__unused void *data, const char *reason)
{
	printf("exit [%s]\n", reason);
}

static void
cb_bridge(__unused void *data, const char *b64)
{
	printf("bridge [%s]\n", b64);
}

static void
cb_unknown(__unused void *data, const char *line)
{
	printf("unknown [%s]\n", line);
}

static const struct remote_parse_callbacks callbacks = {
	.begin = cb_begin,
	.end = cb_end,
	.error = cb_error,
	.output = cb_output,
	.pause = cb_pause,
	.cont = cb_continue,
	.layout_change = cb_layout_change,
	.window_add = cb_window_add,
	.window_close = cb_window_close,
	.window_renamed = cb_window_renamed,
	.window_pane_changed = cb_window_pane_changed,
	.session_changed = cb_session_changed,
	.sessions_changed = cb_sessions_changed,
	.session_renamed = cb_session_renamed,
	.session_window_changed = cb_session_window_changed,
	.pane_mode_changed = cb_pane_mode_changed,
	.subscription_changed = cb_subscription_changed,
	.exit = cb_exit,
	.bridge = cb_bridge,
	.unknown = cb_unknown,
};

int
main(int argc, char **argv)
{
	struct remote_parser	*rp;
	struct evbuffer		*evb;
	FILE			*f;
	char			 buf[65536];
	size_t			 chunk, n, off, len;

	if (argc != 3) {
		fprintf(stderr, "usage: remote-parse-test chunk file\n");
		return (2);
	}
	chunk = strtoul(argv[1], NULL, 10);
	if (chunk == 0)
		chunk = 1;
	f = fopen(argv[2], "r");
	if (f == NULL) {
		perror(argv[2]);
		return (2);
	}

	rp = remote_parser_create(&callbacks, NULL);
	evb = evbuffer_new();
	while ((n = fread(buf, 1, sizeof buf, f)) != 0) {
		for (off = 0; off < n; off += len) {
			len = n - off;
			if (len > chunk)
				len = chunk;
			evbuffer_add(evb, buf + off, len);
			remote_parse_feed(rp, evb);
		}
	}
	if (EVBUFFER_LENGTH(evb) != 0) {
		/* A last line without a newline. */
		evbuffer_add(evb, "\n", 1);
		remote_parse_feed(rp, evb);
	}
	printf("in-block %d\n", remote_parser_in_block(rp));
	evbuffer_free(evb);
	remote_parser_free(rp);
	fclose(f);
	return (0);
}
