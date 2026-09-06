/* $OpenBSD$ */

/*
 * Copyright (c) 2009 Joshua Elsasser <josh@elsasser.org>
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

#include <sys/cdefs.h>
#include <sys/types.h>
#include <sys/sysctl.h>

#include <AvailabilityMacros.h>
#if MAC_OS_X_VERSION_MIN_REQUIRED >= 1050
#include <libproc.h>
#endif
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "compat.h"

char			*osdep_get_name(int, char *);
char			*osdep_get_cwd(int);
char			*osdep_get_env(int, const char *);
char			*osdep_get_fds(int);
struct event_base	*osdep_event_init(void);

char *
osdep_get_name(int fd, __unused char *tty)
{
#if MAC_OS_X_VERSION_MIN_REQUIRED >= 1070
	struct proc_bsdshortinfo	bsdinfo;
	pid_t				pgrp;
	int				ret;

	if ((pgrp = tcgetpgrp(fd)) == -1)
		return (NULL);

	ret = proc_pidinfo(pgrp, PROC_PIDT_SHORTBSDINFO, 0,
			&bsdinfo, sizeof bsdinfo);
	if (ret == sizeof bsdinfo && *bsdinfo.pbsi_comm != '\0')
		return (strdup(bsdinfo.pbsi_comm));
	return (NULL);
#else
	int	mib[4] = { CTL_KERN, KERN_PROC, KERN_PROC_PID, 0 };
	size_t	size;
	struct kinfo_proc kp;

	if ((mib[3] = tcgetpgrp(fd)) == -1)
		return (NULL);

	size = sizeof kp;
	if (sysctl(mib, 4, &kp, &size, NULL, 0) == -1)
		return (NULL);
	if (size != (sizeof kp) || *kp.kp_proc.p_comm == '\0')
		return (NULL);

	return (strdup(kp.kp_proc.p_comm));
#endif
}

char *
osdep_get_cwd(int fd)
{
#if MAC_OS_X_VERSION_MIN_REQUIRED >= 1050
	static char			wd[PATH_MAX];
	struct proc_vnodepathinfo	pathinfo;
	pid_t				pgrp;
	int				ret;

	if ((pgrp = tcgetpgrp(fd)) == -1)
		return (NULL);

	ret = proc_pidinfo(pgrp, PROC_PIDVNODEPATHINFO, 0,
	    &pathinfo, sizeof pathinfo);
	if (ret == sizeof pathinfo) {
		strlcpy(wd, pathinfo.pvi_cdir.vip_path, sizeof wd);
		return (wd);
	}
#endif
	return (NULL);
}

char *
osdep_get_env(int fd, const char *name)
{
	int	 mib[3], argc, argmax;
	size_t	 size, namelen;
	char	*procargs, *cp, *end, *value;
	pid_t	 pgrp;

	if ((pgrp = tcgetpgrp(fd)) == -1)
		return (NULL);

	/* Size the buffer to the kernel's argument maximum. */
	mib[0] = CTL_KERN;
	mib[1] = KERN_ARGMAX;
	size = sizeof argmax;
	if (sysctl(mib, 2, &argmax, &size, NULL, 0) == -1)
		return (NULL);
	if ((procargs = malloc(argmax)) == NULL)
		return (NULL);

	mib[0] = CTL_KERN;
	mib[1] = KERN_PROCARGS2;
	mib[2] = pgrp;
	size = argmax;
	if (sysctl(mib, 3, procargs, &size, NULL, 0) == -1 ||
	    size < sizeof argc) {
		free(procargs);
		return (NULL);
	}

	/* Layout: int argc, exec path, NUL padding, argv[], then env[]. */
	memcpy(&argc, procargs, sizeof argc);
	cp = procargs + sizeof argc;
	end = procargs + size;
	for (; cp < end && *cp != '\0'; cp++)		/* skip exec path */
		;
	for (; cp < end && *cp == '\0'; cp++)		/* skip padding */
		;
	while (argc > 0 && cp < end) {			/* skip argv */
		if (*cp++ == '\0')
			argc--;
	}

	namelen = strlen(name);
	value = NULL;
	while (cp < end) {
		if (*cp == '\0') {
			cp++;
			continue;
		}
		if (strncmp(cp, name, namelen) == 0 && cp[namelen] == '=') {
			value = strdup(cp + namelen + 1);
			break;
		}
		cp += strlen(cp) + 1;
	}

	free(procargs);
	return (value);
}

char *
osdep_get_fds(int fd)
{
#if MAC_OS_X_VERSION_MIN_REQUIRED >= 1050
	struct proc_fdinfo		*fds;
	struct vnode_fdinfowithpath	 vi;
	char				*out;
	size_t				 outlen, plen;
	pid_t				 pgrp;
	int				 bufsize, nfds, i, ret;

	if ((pgrp = tcgetpgrp(fd)) == -1)
		return (NULL);

	bufsize = proc_pidinfo(pgrp, PROC_PIDLISTFDS, 0, NULL, 0);
	if (bufsize <= 0)
		return (NULL);
	if ((fds = malloc(bufsize)) == NULL)
		return (NULL);
	bufsize = proc_pidinfo(pgrp, PROC_PIDLISTFDS, 0, fds, bufsize);
	if (bufsize <= 0) {
		free(fds);
		return (NULL);
	}
	nfds = bufsize / (int) sizeof(struct proc_fdinfo);

	/* Return the path of every fd that names a vnode, one per line. */
	out = NULL;
	outlen = 0;
	for (i = 0; i < nfds; i++) {
		if (fds[i].proc_fdtype != PROX_FDTYPE_VNODE)
			continue;
		ret = proc_pidfdinfo(pgrp, fds[i].proc_fd,
		    PROC_PIDFDVNODEPATHINFO, &vi, sizeof vi);
		if (ret != sizeof vi || vi.pvip.vip_path[0] != '/')
			continue;
		plen = strlen(vi.pvip.vip_path);
		out = realloc(out, outlen + plen + 2);
		if (out == NULL)
			break;
		memcpy(out + outlen, vi.pvip.vip_path, plen);
		outlen += plen;
		out[outlen++] = '\n';
	}
	if (out != NULL)
		out[outlen] = '\0';

	free(fds);
	return (out);
#else
	return (NULL);
#endif
}

struct event_base *
osdep_event_init(void)
{
	struct event_base	*base;

	/*
	 * On OS X, kqueue and poll are both completely broken and don't
	 * work on anything except socket file descriptors (yes, really).
	 */
	setenv("EVENT_NOKQUEUE", "1", 1);
	setenv("EVENT_NOPOLL", "1", 1);

	base = event_init();
	unsetenv("EVENT_NOKQUEUE");
	unsetenv("EVENT_NOPOLL");
	return (base);
}
