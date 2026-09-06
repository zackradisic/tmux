/* $OpenBSD$ */

/*
 * Copyright (c) 2009 Nicholas Marriott <nicholas.marriott@gmail.com>
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
#include <sys/param.h>

#include <dirent.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"

char *
osdep_get_name(int fd, __unused char *tty)
{
	FILE	*f;
	char	*path, *buf;
	size_t	 len;
	int	 ch;
	pid_t	 pgrp;

	if ((pgrp = tcgetpgrp(fd)) == -1)
		return (NULL);

	xasprintf(&path, "/proc/%lld/cmdline", (long long) pgrp);
	if ((f = fopen(path, "r")) == NULL) {
		free(path);
		return (NULL);
	}
	free(path);

	len = 0;
	buf = NULL;
	while ((ch = fgetc(f)) != EOF) {
		if (ch == '\0')
			break;
		buf = xrealloc(buf, len + 2);
		buf[len++] = ch;
	}
	if (buf != NULL)
		buf[len] = '\0';

	fclose(f);
	return (buf);
}

char *
osdep_get_cwd(int fd)
{
	static char	 target[MAXPATHLEN + 1];
	char		*path;
	pid_t		 pgrp, sid;
	ssize_t		 n;

	if ((pgrp = tcgetpgrp(fd)) == -1)
		return (NULL);

	xasprintf(&path, "/proc/%lld/cwd", (long long) pgrp);
	n = readlink(path, target, MAXPATHLEN);
	free(path);

	if (n == -1 && ioctl(fd, TIOCGSID, &sid) != -1) {
		xasprintf(&path, "/proc/%lld/cwd", (long long) sid);
		n = readlink(path, target, MAXPATHLEN);
		free(path);
	}

	if (n > 0) {
		target[n] = '\0';
		return (target);
	}
	return (NULL);
}

char *
osdep_get_env(int fd, const char *name)
{
	FILE	*f;
	char	*path, *buf, *value;
	size_t	 len, namelen;
	int	 ch;
	pid_t	 pgrp;

	if ((pgrp = tcgetpgrp(fd)) == -1)
		return (NULL);

	xasprintf(&path, "/proc/%lld/environ", (long long) pgrp);
	if ((f = fopen(path, "r")) == NULL) {
		free(path);
		return (NULL);
	}
	free(path);

	/* /proc/<pid>/environ is NUL-separated "NAME=VALUE" entries. */
	namelen = strlen(name);
	len = 0;
	buf = NULL;
	value = NULL;
	while ((ch = fgetc(f)) != EOF) {
		if (ch != '\0') {
			buf = xrealloc(buf, len + 2);
			buf[len++] = ch;
			continue;
		}
		if (buf != NULL) {
			buf[len] = '\0';
			if (strncmp(buf, name, namelen) == 0 &&
			    buf[namelen] == '=') {
				value = xstrdup(buf + namelen + 1);
				break;
			}
		}
		free(buf);
		buf = NULL;
		len = 0;
	}
	free(buf);

	fclose(f);
	return (value);
}

char *
osdep_get_fds(int fd)
{
	DIR		*dir;
	struct dirent	*ent;
	char		*path, *out, link[MAXPATHLEN + 1];
	size_t		 outlen;
	ssize_t		 n;
	pid_t		 pgrp;

	if ((pgrp = tcgetpgrp(fd)) == -1)
		return (NULL);

	xasprintf(&path, "/proc/%lld/fd", (long long) pgrp);
	dir = opendir(path);
	free(path);
	if (dir == NULL)
		return (NULL);

	/* Return the real path of every fd that names a file, one per line. */
	out = NULL;
	outlen = 0;
	while ((ent = readdir(dir)) != NULL) {
		if (ent->d_name[0] == '.')
			continue;
		xasprintf(&path, "/proc/%lld/fd/%s", (long long) pgrp,
		    ent->d_name);
		n = readlink(path, link, MAXPATHLEN);
		free(path);
		if (n <= 0)
			continue;
		link[n] = '\0';
		if (link[0] != '/')	/* skip sockets, pipes, anon inodes */
			continue;
		out = xrealloc(out, outlen + n + 2);
		memcpy(out + outlen, link, n);
		outlen += n;
		out[outlen++] = '\n';
	}
	if (out != NULL)
		out[outlen] = '\0';

	closedir(dir);
	return (out);
}

struct event_base *
osdep_event_init(void)
{
	struct event_base	*base;

	/* On Linux, epoll doesn't work on /dev/null (yes, really). */
	setenv("EVENT_NOEPOLL", "1", 1);

	base = event_init();
	unsetenv("EVENT_NOEPOLL");
	return (base);
}
