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
 * Binary emitter for the plugin ABI wire formats (mirrored by the Rust
 * abi-types crate; keep the two in sync). Everything is little-endian,
 * packed, emit-only - the C side never parses these buffers.
 *
 * Two shapes are built here:
 *
 *  - plain record buffers (object lists for list/resolve): the caller
 *    appends u8/u32/u64/str primitives and flushes with plugin_buf_data().
 *
 *  - event buffers: fixed header (u32 event id, u64 seq written as 0 and
 *    patched by the host, four u32 scope ids with 0xffffffff = none)
 *    followed by a field block (u16 count, then fields of
 *    u32 interned key id + u8 tag + value). Built with plugin_event_*
 *    and shipped with plugin_event_send() / plugin_event_send_mode().
 */

#define PLUGIN_BUF_NONE_ID 0xffffffffU

/* Value tags; must match abi-types::tag. */
#define PLUGIN_TAG_NULL	0
#define PLUGIN_TAG_BOOL	1
#define PLUGIN_TAG_I64	2
#define PLUGIN_TAG_STR	4

struct plugin_buf {
	struct evbuffer	*evb;

	/* Event mode only. */
	int		 is_event;
	uint32_t	 event_id;
	uint32_t	 scope[4];	/* client, session, window, pane */
	uint16_t	 nfields;
};

static void
plugin_buf_le(struct plugin_buf *pb, uint64_t v, size_t n)
{
	u_char	b[8];
	size_t	i;

	for (i = 0; i < n; i++)
		b[i] = (v >> (8 * i)) & 0xff;
	evbuffer_add(pb->evb, b, n);
}

struct plugin_buf *
plugin_buf_create(void)
{
	struct plugin_buf	*pb;

	pb = xcalloc(1, sizeof *pb);
	pb->evb = evbuffer_new();
	if (pb->evb == NULL)
		fatalx("out of memory");
	return (pb);
}

void
plugin_buf_free(struct plugin_buf *pb)
{
	if (pb == NULL)
		return;
	evbuffer_free(pb->evb);
	free(pb);
}

void
plugin_buf_u8(struct plugin_buf *pb, uint8_t v)
{
	plugin_buf_le(pb, v, 1);
}

void
plugin_buf_u32(struct plugin_buf *pb, uint32_t v)
{
	plugin_buf_le(pb, v, 4);
}

void
plugin_buf_u64(struct plugin_buf *pb, uint64_t v)
{
	plugin_buf_le(pb, v, 8);
}

/* u32-length-prefixed string; NULL emits the empty string. */
void
plugin_buf_str(struct plugin_buf *pb, const char *s)
{
	size_t	len;

	if (s == NULL)
		s = "";
	len = strlen(s);
	plugin_buf_le(pb, len, 4);
	evbuffer_add(pb->evb, s, len);
}

/*
 * Contiguous view of a plain buffer, valid until the next append or free.
 */
const u_char *
plugin_buf_data(struct plugin_buf *pb, size_t *len)
{
	*len = EVBUFFER_LENGTH(pb->evb);
	return (EVBUFFER_DATA(pb->evb));
}

/* ---- event buffers ---- */

struct plugin_buf *
plugin_event_create(const char *event_name)
{
	struct plugin_buf	*pb;
	u_int			 i;

	pb = plugin_buf_create();
	pb->is_event = 1;
	pb->event_id = pgh_intern(event_name);
	for (i = 0; i < nitems(pb->scope); i++)
		pb->scope[i] = PLUGIN_BUF_NONE_ID;
	return (pb);
}

/* Set one scope slot from an object kind (PGH_OBJ_*). */
void
plugin_event_scope(struct plugin_buf *pb, int kind, uint32_t id)
{
	switch (kind) {
	case PGH_OBJ_CLIENT:
		pb->scope[0] = id;
		break;
	case PGH_OBJ_SESSION:
		pb->scope[1] = id;
		break;
	case PGH_OBJ_WINDOW:
		pb->scope[2] = id;
		break;
	case PGH_OBJ_PANE:
		pb->scope[3] = id;
		break;
	}
}

int
plugin_event_scope_set(struct plugin_buf *pb, int kind)
{
	switch (kind) {
	case PGH_OBJ_CLIENT:
		return (pb->scope[0] != PLUGIN_BUF_NONE_ID);
	case PGH_OBJ_SESSION:
		return (pb->scope[1] != PLUGIN_BUF_NONE_ID);
	case PGH_OBJ_WINDOW:
		return (pb->scope[2] != PLUGIN_BUF_NONE_ID);
	case PGH_OBJ_PANE:
		return (pb->scope[3] != PLUGIN_BUF_NONE_ID);
	}
	return (0);
}

static void
plugin_event_key(struct plugin_buf *pb, const char *key, uint8_t tag)
{
	pb->nfields++;
	plugin_buf_le(pb, pgh_intern(key), 4);
	plugin_buf_le(pb, tag, 1);
}

void
plugin_event_str(struct plugin_buf *pb, const char *key, const char *value)
{
	plugin_event_key(pb, key, PLUGIN_TAG_STR);
	plugin_buf_str(pb, value);
}

void
plugin_event_i64(struct plugin_buf *pb, const char *key, long long value)
{
	plugin_event_key(pb, key, PLUGIN_TAG_I64);
	plugin_buf_le(pb, (uint64_t)value, 8);
}

void
plugin_event_bool(struct plugin_buf *pb, const char *key, int value)
{
	plugin_event_key(pb, key, PLUGIN_TAG_BOOL);
	plugin_buf_le(pb, value != 0, 1);
}

/*
 * Assemble the final event buffer: header + field count + fields.
 * Returned buffer is malloc'd; the caller frees.
 */
static u_char *
plugin_event_finish(struct plugin_buf *pb, size_t *lenp)
{
	u_char	*out, *at;
	size_t	 body;
	u_int	 i;

	body = EVBUFFER_LENGTH(pb->evb);
	*lenp = 4 + 8 + 4 * nitems(pb->scope) + 2 + body;
	out = xmalloc(*lenp);
	at = out;

#define PUT_LE(v, n)							\
	do {								\
		uint64_t _v = (v);					\
		size_t _i;						\
		for (_i = 0; _i < (n); _i++)				\
			*at++ = (_v >> (8 * _i)) & 0xff;		\
	} while (0)

	PUT_LE(pb->event_id, 4);
	PUT_LE(0, 8);	/* seq: patched by the host at delivery */
	for (i = 0; i < nitems(pb->scope); i++)
		PUT_LE(pb->scope[i], 4);
	PUT_LE(pb->nfields, 2);

#undef PUT_LE

	memcpy(at, EVBUFFER_DATA(pb->evb), body);
	return (out);
}

/* Ship an event through pgh_notify and free the builder. */
void
plugin_event_send(struct plugin_buf *pb)
{
	u_char	*buf;
	size_t	 len;

	if (plugin_enabled()) {
		buf = plugin_event_finish(pb, &len);
		pgh_notify(buf, len);
		free(buf);
		plugin_schedule_drain();
	}
	plugin_buf_free(pb);
}

/* Ship a mode event through pgh_mode_event and free the builder. */
void
plugin_event_send_mode(struct plugin_buf *pb, uint64_t mode_id)
{
	u_char	*buf;
	size_t	 len;

	if (plugin_enabled()) {
		buf = plugin_event_finish(pb, &len);
		pgh_mode_event(mode_id, buf, len);
		free(buf);
		plugin_schedule_drain();
	}
	plugin_buf_free(pb);
}
