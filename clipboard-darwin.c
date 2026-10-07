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
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#include <sys/types.h>

#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"

/*
 * What is on the system clipboard, as a file: an image as PNG, else text
 * as UTF-8. What a plugin needs to show it (a terminal with kitty
 * graphics reads an image file itself) and to send it somewhere with
 * scp. A pasteboard is a GUI thing with no portable interface; this is
 * the macOS one, in plain C through the Objective-C runtime, with AppKit
 * loaded on first use so a server that never asks never pays for it.
 * Other platforms report -3 and a plugin falls back to wl-paste or
 * xclip.
 */

#ifdef __APPLE__

#include <dlfcn.h>

typedef struct objc_object	*cb_id;
typedef struct objc_selector	*cb_sel;

/*
 * objc_msgSend is variadic in the ABI but must be called through a
 * pointer of the exact type on arm64; one typedef per shape used.
 */
typedef cb_id	(*cb_msg_t)(cb_id, cb_sel);
typedef cb_id	(*cb_msg_id_t)(cb_id, cb_sel, cb_id);
typedef cb_id	(*cb_msg_cstr_t)(cb_id, cb_sel, const char *);
typedef cb_id	(*cb_msg_ul_id_t)(cb_id, cb_sel, unsigned long, cb_id);
typedef unsigned long (*cb_msg_ul_t)(cb_id, cb_sel);
typedef const void *(*cb_msg_ptr_t)(cb_id, cb_sel);

static struct {
	int	 state; /* 0 untried, 1 ready, -1 unavailable */
	cb_id	(*getclass)(const char *);
	cb_sel	(*sel)(const char *);
	void	*(*msgsend)(void);
} cb;

static int
clipboard_init(void)
{
	void	*handle;

	if (cb.state != 0)
		return (cb.state);
	handle = dlopen("/System/Library/Frameworks/AppKit.framework/AppKit",
	    RTLD_LAZY);
	if (handle == NULL) {
		cb.state = -1;
		return (-1);
	}
	cb.getclass = dlsym(RTLD_DEFAULT, "objc_getClass");
	cb.sel = dlsym(RTLD_DEFAULT, "sel_registerName");
	cb.msgsend = dlsym(RTLD_DEFAULT, "objc_msgSend");
	if (cb.getclass == NULL || cb.sel == NULL || cb.msgsend == NULL) {
		cb.state = -1;
		return (-1);
	}
	cb.state = 1;
	return (1);
}

#define MSG(type) ((type)cb.msgsend)

static cb_id
clipboard_nsstring(const char *s)
{
	cb_id	cls = cb.getclass("NSString");

	return (MSG(cb_msg_cstr_t)(cls, cb.sel("stringWithUTF8String:"), s));
}

/*
 * The PNG on the pasteboard, or a TIFF (a screenshot copied from an app
 * that offers only that) re-encoded as PNG. An autoreleased NSData.
 */
static cb_id
clipboard_png_data(void)
{
	cb_id	pb, data, tiff, rep;

	pb = MSG(cb_msg_t)(cb.getclass("NSPasteboard"),
	    cb.sel("generalPasteboard"));
	if (pb == NULL)
		return (NULL);
	data = MSG(cb_msg_id_t)(pb, cb.sel("dataForType:"),
	    clipboard_nsstring("public.png"));
	if (data != NULL)
		return (data);
	tiff = MSG(cb_msg_id_t)(pb, cb.sel("dataForType:"),
	    clipboard_nsstring("public.tiff"));
	if (tiff == NULL)
		return (NULL);
	rep = MSG(cb_msg_id_t)(cb.getclass("NSBitmapImageRep"),
	    cb.sel("imageRepWithData:"), tiff);
	if (rep == NULL)
		return (NULL);
	/* NSBitmapImageFileTypePNG == 4. */
	return (MSG(cb_msg_ul_id_t)(rep,
	    cb.sel("representationUsingType:properties:"), 4, NULL));
}

/* Pixel size from the IHDR chunk, which the spec puts first. */
static int
clipboard_png_size(const u_char *b, size_t n, u_int *w, u_int *h)
{
	static const u_char	sig[8] = { 0x89, 'P', 'N', 'G', '\r', '\n', 0x1a,
	    '\n' };

	if (n < 24 || memcmp(b, sig, 8) != 0 || memcmp(b + 12, "IHDR", 4) != 0)
		return (-1);
	*w = ((u_int)b[16] << 24) | ((u_int)b[17] << 16) | ((u_int)b[18] << 8) |
	    b[19];
	*h = ((u_int)b[20] << 24) | ((u_int)b[21] << 16) | ((u_int)b[22] << 8) |
	    b[23];
	return (0);
}

/* Write the bytes to path through a temporary, so a reader never sees half. */
static int
clipboard_write(const char *path, const void *bytes, size_t n)
{
	char	 tmp[PATH_MAX];
	FILE	*f;

	if (snprintf(tmp, sizeof tmp, "%s.tmp", path) >= (int)sizeof tmp)
		return (-1);
	if ((f = fopen(tmp, "wb")) == NULL)
		return (-1);
	if (fwrite(bytes, 1, n, f) != n || fclose(f) != 0) {
		unlink(tmp);
		return (-1);
	}
	if (rename(tmp, path) != 0) {
		unlink(tmp);
		return (-1);
	}
	return (0);
}

/* The text on the pasteboard as UTF-8 bytes. An autoreleased NSData. */
static cb_id
clipboard_text_data(void)
{
	cb_id	pb;

	pb = MSG(cb_msg_t)(cb.getclass("NSPasteboard"),
	    cb.sel("generalPasteboard"));
	if (pb == NULL)
		return (NULL);
	return (MSG(cb_msg_id_t)(pb, cb.sel("dataForType:"),
	    clipboard_nsstring("public.utf8-plain-text")));
}

/*
 * Write the clipboard to base + ".png" (an image: *kind = 1, *w and *h
 * its pixel size) or base + ".txt" (text: *kind = 2). An image wins when
 * both are offered, as when an image is copied from a browser along
 * with its alt text. 0 ok, -1 nothing usable on the clipboard, -2 an
 * image that could not be read, -3 no clipboard on this platform, -4 the
 * file could not be written. `base` must leave room for the suffix.
 */
int
clipboard_to_file(const char *base, int *kind, u_int *w, u_int *h,
    uint64_t *len)
{
	cb_id		 pool, data;
	const void	*bytes;
	unsigned long	 n;
	char		 path[PATH_MAX];
	int		 rc;

	if (clipboard_init() < 0)
		return (-3);
	*kind = 0;
	*w = *h = 0;

	/* Everything below is autoreleased: a pool, or a few MB leak per call. */
	pool = MSG(cb_msg_t)(cb.getclass("NSAutoreleasePool"), cb.sel("alloc"));
	pool = MSG(cb_msg_t)(pool, cb.sel("init"));

	data = clipboard_png_data();
	if (data != NULL) {
		bytes = MSG(cb_msg_ptr_t)(data, cb.sel("bytes"));
		n = MSG(cb_msg_ul_t)(data, cb.sel("length"));
		if (bytes == NULL || clipboard_png_size(bytes, n, w, h) != 0) {
			rc = -2;
			goto out;
		}
		*kind = 1;
		snprintf(path, sizeof path, "%s.png", base);
	} else {
		data = clipboard_text_data();
		if (data == NULL) {
			rc = -1;
			goto out;
		}
		bytes = MSG(cb_msg_ptr_t)(data, cb.sel("bytes"));
		n = MSG(cb_msg_ul_t)(data, cb.sel("length"));
		if (bytes == NULL || n == 0) {
			rc = -1;
			goto out;
		}
		*kind = 2;
		snprintf(path, sizeof path, "%s.txt", base);
	}
	if (clipboard_write(path, bytes, n) != 0) {
		rc = -4;
		goto out;
	}
	*len = n;
	rc = 0;

out:
	MSG(cb_msg_t)(pool, cb.sel("drain"));
	return (rc);
}

#else /* !__APPLE__ */

int
clipboard_to_file(__unused const char *base, __unused int *kind,
    __unused u_int *w, __unused u_int *h, __unused uint64_t *len)
{
	return (-3);
}

#endif /* __APPLE__ */
