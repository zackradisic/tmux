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

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "tmux.h"

/*
 * `tmux ocr [-n] file.png`: the text in an image, through the system's
 * recogniser, one observation per line on stdout:
 *
 *	x y w h confidence <TAB> text
 *
 * The box is normalised to the image, origin bottom-left, as Vision
 * reports it; the caller puts the lines in reading order and rebuilds
 * the layout. -n turns language correction off (code, not prose).
 *
 * This is a mode of the tmux binary rather than a server call because a
 * recognition takes a few hundred milliseconds (and the first one ever
 * on a machine takes a minute while the models are set up): a plugin
 * runs it as a job and the server never waits. macOS only: Vision is
 * reached through the Objective-C runtime, with no completion block (a
 * request performed synchronously has its results on return) and no
 * framework linked at build time. Exit 0 on success, 1 with the error on
 * stderr, 3 when there is no recogniser on this platform.
 */

#ifdef __APPLE__

#include <dlfcn.h>

typedef struct objc_object	*ocr_id;
typedef struct objc_selector	*ocr_sel;

struct ocr_point { double x, y; };
struct ocr_size { double w, h; };
struct ocr_rect { struct ocr_point origin; struct ocr_size size; };

typedef ocr_id		(*ocr_msg_t)(ocr_id, ocr_sel);
typedef ocr_id		(*ocr_msg_id_t)(ocr_id, ocr_sel, ocr_id);
typedef ocr_id		(*ocr_msg_id_id_t)(ocr_id, ocr_sel, ocr_id, ocr_id);
typedef ocr_id		(*ocr_msg_cstr_t)(ocr_id, ocr_sel, const char *);
typedef ocr_id		(*ocr_msg_ul_t)(ocr_id, ocr_sel, unsigned long);
typedef void		(*ocr_msg_setl_t)(ocr_id, ocr_sel, long);
typedef void		(*ocr_msg_setb_t)(ocr_id, ocr_sel, signed char);
typedef unsigned long	(*ocr_msg_count_t)(ocr_id, ocr_sel);
typedef signed char	(*ocr_msg_perform_t)(ocr_id, ocr_sel, ocr_id, ocr_id *);
typedef const char	*(*ocr_msg_utf8_t)(ocr_id, ocr_sel);
typedef struct ocr_rect	(*ocr_msg_rect_t)(ocr_id, ocr_sel);
typedef float		(*ocr_msg_float_t)(ocr_id, ocr_sel);

static struct {
	ocr_id	(*getclass)(const char *);
	ocr_sel	(*sel)(const char *);
	void	*(*msgsend)(void);
	void	*(*msgsend_rect)(void);
} ocr;

#define MSG(type) ((type)(void *)ocr.msgsend)

static ocr_id
ocr_nsstring(const char *s)
{
	return (MSG(ocr_msg_cstr_t)(ocr.getclass("NSString"),
	    ocr.sel("stringWithUTF8String:"), s));
}

static int
ocr_init(void)
{
	if (dlopen("/System/Library/Frameworks/Vision.framework/Vision",
	    RTLD_LAZY) == NULL)
		return (-1);
	ocr.getclass = dlsym(RTLD_DEFAULT, "objc_getClass");
	ocr.sel = dlsym(RTLD_DEFAULT, "sel_registerName");
	ocr.msgsend = dlsym(RTLD_DEFAULT, "objc_msgSend");
	/*
	 * A CGRect comes back in registers on arm64; x86_64 returns a
	 * struct that size through memory, with its own entry point.
	 */
#if defined(__x86_64__)
	ocr.msgsend_rect = dlsym(RTLD_DEFAULT, "objc_msgSend_stret");
#else
	ocr.msgsend_rect = ocr.msgsend;
#endif
	if (ocr.getclass == NULL || ocr.sel == NULL || ocr.msgsend == NULL ||
	    ocr.msgsend_rect == NULL)
		return (-1);
	return (0);
}

int
ocr_main(int argc, char **argv)
{
	const char	*path = NULL, *s;
	int		 correct = 1, i;
	ocr_id		 pool, url, req, handler, reqs, err = NULL, results;
	ocr_id		 obs, cands, cand, text, desc;
	unsigned long	 n, j;
	struct ocr_rect	 r;
	float		 conf;

	for (i = 0; i < argc; i++) {
		if (strcmp(argv[i], "-n") == 0)
			correct = 0;
		else if (path == NULL)
			path = argv[i];
		else {
			fprintf(stderr, "usage: tmux ocr [-n] file\n");
			return (2);
		}
	}
	if (path == NULL) {
		fprintf(stderr, "usage: tmux ocr [-n] file\n");
		return (2);
	}
	if (ocr_init() != 0) {
		fprintf(stderr, "the Vision framework is not available\n");
		return (3);
	}

	pool = MSG(ocr_msg_t)(ocr.getclass("NSAutoreleasePool"),
	    ocr.sel("alloc"));
	pool = MSG(ocr_msg_t)(pool, ocr.sel("init"));

	url = MSG(ocr_msg_id_t)(ocr.getclass("NSURL"),
	    ocr.sel("fileURLWithPath:"), ocr_nsstring(path));
	req = MSG(ocr_msg_t)(ocr.getclass("VNRecognizeTextRequest"),
	    ocr.sel("alloc"));
	req = MSG(ocr_msg_t)(req, ocr.sel("init"));
	if (url == NULL || req == NULL) {
		fprintf(stderr, "cannot set up the recogniser\n");
		return (1);
	}
	/* VNRequestTextRecognitionLevelAccurate == 0. */
	MSG(ocr_msg_setl_t)(req, ocr.sel("setRecognitionLevel:"), 0);
	MSG(ocr_msg_setb_t)(req, ocr.sel("setUsesLanguageCorrection:"),
	    correct);
	MSG(ocr_msg_setb_t)(req, ocr.sel("setAutomaticallyDetectsLanguage:"),
	    1);

	handler = MSG(ocr_msg_t)(ocr.getclass("VNImageRequestHandler"),
	    ocr.sel("alloc"));
	handler = MSG(ocr_msg_id_id_t)(handler,
	    ocr.sel("initWithURL:options:"), url,
	    MSG(ocr_msg_t)(ocr.getclass("NSDictionary"),
	    ocr.sel("dictionary")));
	reqs = MSG(ocr_msg_id_t)(ocr.getclass("NSArray"),
	    ocr.sel("arrayWithObject:"), req);
	if (!MSG(ocr_msg_perform_t)(handler, ocr.sel("performRequests:error:"),
	    reqs, &err)) {
		s = "recognition failed";
		if (err != NULL) {
			desc = MSG(ocr_msg_t)(err,
			    ocr.sel("localizedDescription"));
			if (desc != NULL)
				s = MSG(ocr_msg_utf8_t)(desc,
				    ocr.sel("UTF8String"));
		}
		fprintf(stderr, "%s\n", s);
		MSG(ocr_msg_t)(pool, ocr.sel("drain"));
		return (1);
	}

	results = MSG(ocr_msg_t)(req, ocr.sel("results"));
	n = results == NULL ? 0 : MSG(ocr_msg_count_t)(results,
	    ocr.sel("count"));
	for (j = 0; j < n; j++) {
		obs = MSG(ocr_msg_ul_t)(results, ocr.sel("objectAtIndex:"), j);
		cands = MSG(ocr_msg_ul_t)(obs, ocr.sel("topCandidates:"), 1);
		if (cands == NULL ||
		    MSG(ocr_msg_count_t)(cands, ocr.sel("count")) == 0)
			continue;
		cand = MSG(ocr_msg_ul_t)(cands, ocr.sel("objectAtIndex:"), 0);
		text = MSG(ocr_msg_t)(cand, ocr.sel("string"));
		if (text == NULL)
			continue;
		s = MSG(ocr_msg_utf8_t)(text, ocr.sel("UTF8String"));
		if (s == NULL)
			continue;
		r = ((ocr_msg_rect_t)(void *)ocr.msgsend_rect)(obs,
		    ocr.sel("boundingBox"));
		conf = MSG(ocr_msg_float_t)(cand, ocr.sel("confidence"));
		printf("%.4f %.4f %.4f %.4f %.2f\t%s\n", r.origin.x, r.origin.y,
		    r.size.w, r.size.h, conf, s);
	}
	MSG(ocr_msg_t)(pool, ocr.sel("drain"));
	return (0);
}

#else /* !__APPLE__ */

int
ocr_main(__unused int argc, __unused char **argv)
{
	fprintf(stderr, "no text recogniser on this platform\n");
	return (3);
}

#endif /* __APPLE__ */
