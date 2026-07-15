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

/*
 * Tiny JSON emitter for the plugin bridge. Emit only - the C side never
 * parses JSON (anything coming back from the plugin host for display is
 * preformatted text).
 *
 * Bytes >= 0x80 are passed through untouched: names in tmux may be
 * arbitrary bytes and the Rust side does a lossy UTF-8 conversion before
 * parsing.
 */

#define PLUGIN_JSON_MAXDEPTH 16

struct plugin_json {
	struct evbuffer	*evb;
	u_int		 depth;
	int		 need_comma[PLUGIN_JSON_MAXDEPTH];
};

static void	plugin_json_comma(struct plugin_json *);
static void	plugin_json_key(struct plugin_json *, const char *);
static void	plugin_json_escape(struct plugin_json *, const char *);

struct plugin_json *
plugin_json_create(void)
{
	struct plugin_json	*pj;

	pj = xcalloc(1, sizeof *pj);
	pj->evb = evbuffer_new();
	if (pj->evb == NULL)
		fatalx("out of memory");
	return (pj);
}

void
plugin_json_free(struct plugin_json *pj)
{
	if (pj == NULL)
		return;
	evbuffer_free(pj->evb);
	free(pj);
}

/*
 * Return the buffer contents as a NUL-terminated string, valid until the
 * next emitter call or free.
 */
const char *
plugin_json_string(struct plugin_json *pj)
{
	evbuffer_add(pj->evb, "", 1);
	return ((const char *)EVBUFFER_DATA(pj->evb));
}

static void
plugin_json_comma(struct plugin_json *pj)
{
	if (pj->need_comma[pj->depth])
		evbuffer_add(pj->evb, ",", 1);
	pj->need_comma[pj->depth] = 1;
}

static void
plugin_json_escape(struct plugin_json *pj, const char *s)
{
	const char	*cp;
	char		 buf[8];

	evbuffer_add(pj->evb, "\"", 1);
	for (cp = s; *cp != '\0'; cp++) {
		switch (*cp) {
		case '"':
			evbuffer_add(pj->evb, "\\\"", 2);
			break;
		case '\\':
			evbuffer_add(pj->evb, "\\\\", 2);
			break;
		case '\n':
			evbuffer_add(pj->evb, "\\n", 2);
			break;
		case '\r':
			evbuffer_add(pj->evb, "\\r", 2);
			break;
		case '\t':
			evbuffer_add(pj->evb, "\\t", 2);
			break;
		default:
			if ((u_char)*cp < 0x20) {
				xsnprintf(buf, sizeof buf, "\\u%04x",
				    (u_int)(u_char)*cp);
				evbuffer_add(pj->evb, buf, 6);
			} else
				evbuffer_add(pj->evb, cp, 1);
			break;
		}
	}
	evbuffer_add(pj->evb, "\"", 1);
}

/* Emit "key": if key is not NULL (i.e. inside an object, not an array). */
static void
plugin_json_key(struct plugin_json *pj, const char *key)
{
	plugin_json_comma(pj);
	if (key != NULL) {
		plugin_json_escape(pj, key);
		evbuffer_add(pj->evb, ":", 1);
	}
}

void
plugin_json_obj_start(struct plugin_json *pj, const char *key)
{
	plugin_json_key(pj, key);
	evbuffer_add(pj->evb, "{", 1);
	if (++pj->depth >= PLUGIN_JSON_MAXDEPTH)
		fatalx("plugin JSON too deep");
	pj->need_comma[pj->depth] = 0;
}

void
plugin_json_obj_end(struct plugin_json *pj)
{
	if (pj->depth == 0)
		fatalx("plugin JSON underflow");
	pj->depth--;
	evbuffer_add(pj->evb, "}", 1);
}

void
plugin_json_arr_start(struct plugin_json *pj, const char *key)
{
	plugin_json_key(pj, key);
	evbuffer_add(pj->evb, "[", 1);
	if (++pj->depth >= PLUGIN_JSON_MAXDEPTH)
		fatalx("plugin JSON too deep");
	pj->need_comma[pj->depth] = 0;
}

void
plugin_json_arr_end(struct plugin_json *pj)
{
	if (pj->depth == 0)
		fatalx("plugin JSON underflow");
	pj->depth--;
	evbuffer_add(pj->evb, "]", 1);
}

void
plugin_json_str(struct plugin_json *pj, const char *key, const char *value)
{
	plugin_json_key(pj, key);
	plugin_json_escape(pj, value);
}

void
plugin_json_num(struct plugin_json *pj, const char *key, long long value)
{
	char	buf[32];

	plugin_json_key(pj, key);
	xsnprintf(buf, sizeof buf, "%lld", value);
	evbuffer_add(pj->evb, buf, strlen(buf));
}

void
plugin_json_bool(struct plugin_json *pj, const char *key, int value)
{
	plugin_json_key(pj, key);
	if (value)
		evbuffer_add(pj->evb, "true", 4);
	else
		evbuffer_add(pj->evb, "false", 5);
}

void
plugin_json_null(struct plugin_json *pj, const char *key)
{
	plugin_json_key(pj, key);
	evbuffer_add(pj->evb, "null", 4);
}
