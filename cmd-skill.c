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

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"
#ifdef ENABLE_PLUGINS
#include "plugin-host.h"
#endif

/*
 * Skills: the guides an agent needs to use tmux2, compiled into the binary
 * from skills/<name>/SKILL.md so every host that has tmux2 has the text
 * that matches it, with nothing to sync.
 *
 *   skill list                     names and descriptions
 *   skill [-t pane] show <name>    print one; the region between
 *                                  <!-- live --> and <!-- /live --> is a
 *                                  format expanded against the pane, so
 *                                  the agent sees its own id, this server's
 *                                  peer name and the linked servers
 *   skill install [-d dir] [name]  write a stub SKILL.md per skill into a
 *                                  harness skills directory (default
 *                                  ~/.claude/skills) whose body says to run
 *                                  "skill show"; the stub is static, so it
 *                                  is written once and never synced
 */

static enum cmd_retval	cmd_skill_exec(struct cmd *, struct cmdq_item *);

const struct cmd_entry cmd_skill_entry = {
	.name = "skill",
	.alias = NULL,

	.args = { "d:t:", 1, 2, NULL },
	.usage = "[-d directory] [-t target-pane] list|show|install [skill]",

	.target = { 't', CMD_FIND_PANE, CMD_FIND_CANFAIL },

	.flags = CMD_STARTSERVER|CMD_AFTERHOOK,
	.exec = cmd_skill_exec
};

#define SKILL_LIVE_BEGIN "<!-- live -->"
#define SKILL_LIVE_END "<!-- /live -->"

static const struct skill *
cmd_skill_find(const char *name)
{
	const struct skill	*sk;

	for (sk = skills; sk->name != NULL; sk++) {
		if (strcmp(sk->name, name) == 0)
			return (sk);
	}
	return (NULL);
}

#ifdef ENABLE_PLUGINS
static void
cmd_skill_sink(void *ctx, const char *ptr, size_t len)
{
	struct evbuffer	*evb = ctx;

	evbuffer_add(evb, ptr, len);
}
#endif

/*
 * What the live region may name beyond the ordinary formats: this
 * server's peer name and its grant table, both from the plugin host. A
 * build without plugins leaves them empty, and the text says so.
 */
static void
cmd_skill_add_live(struct format_tree *ft)
{
#ifdef ENABLE_PLUGINS
	struct evbuffer	*evb;
	char		*line, *p, *grants = NULL, *next;

	if (!plugin_enabled())
		return;

	evb = evbuffer_new();
	if (evb == NULL)
		fatalx("out of memory");
	pgh_peer_name(cmd_skill_sink, evb);
	evbuffer_add(evb, "", 1);
	format_add(ft, "peer_name", "%s",
	    (const char *)EVBUFFER_DATA(evb));
	evbuffer_free(evb);

	evb = evbuffer_new();
	if (evb == NULL)
		fatalx("out of memory");
	pgh_peers_list(cmd_skill_sink, evb);
	while ((line = evbuffer_readline(evb)) != NULL) {
		if (strcmp(line, "no peer grants") == 0) {
			free(line);
			continue;
		}
		for (p = line; *p != '\0'; p++) {
			if (*p == '\t')
				*p = ' ';
		}
		if (grants == NULL)
			grants = xstrdup(line);
		else {
			xasprintf(&next, "%s; %s", grants, line);
			free(grants);
			grants = next;
		}
		free(line);
	}
	evbuffer_free(evb);
	format_add(ft, "peer_grants", "%s", grants != NULL ? grants : "");
	free(grants);
#else
	(void)ft;
#endif
}

static enum cmd_retval
cmd_skill_list(struct cmdq_item *item)
{
	const struct skill	*sk;

	for (sk = skills; sk->name != NULL; sk++)
		cmdq_print(item, "%s\t%s", sk->name, sk->description);
	return (CMD_RETURN_NORMAL);
}

static enum cmd_retval
cmd_skill_show(struct cmdq_item *item, const struct skill *sk)
{
	struct format_tree	*ft;
	const char		*p = sk->text, *nl;
	char			*line, *out;
	int			 live = 0;

	ft = format_create_from_target(item);
	cmd_skill_add_live(ft);
	while (*p != '\0') {
		nl = strchr(p, '\n');
		if (nl == NULL)
			nl = p + strlen(p);
		line = xstrndup(p, nl - p);
		if (strcmp(line, SKILL_LIVE_BEGIN) == 0)
			live = 1;
		else if (strcmp(line, SKILL_LIVE_END) == 0)
			live = 0;
		else if (live) {
			out = format_expand(ft, line);
			cmdq_print(item, "%s", out);
			free(out);
		} else
			cmdq_print(item, "%s", line);
		free(line);
		p = (*nl == '\n') ? nl + 1 : nl;
	}
	format_free(ft);
	return (CMD_RETURN_NORMAL);
}

/* mkdir -p. */
static int
cmd_skill_mkdirs(const char *path)
{
	char		*copy, *p;
	struct stat	 sb;

	copy = xstrdup(path);
	for (p = copy + 1; *p != '\0'; p++) {
		if (*p != '/')
			continue;
		*p = '\0';
		if (mkdir(copy, 0755) != 0 && errno != EEXIST) {
			free(copy);
			return (-1);
		}
		*p = '/';
	}
	if (mkdir(copy, 0755) != 0 && errno != EEXIST) {
		free(copy);
		return (-1);
	}
	free(copy);
	return (stat(path, &sb) == 0 && S_ISDIR(sb.st_mode) ? 0 : -1);
}

/* The stub a harness loads: its frontmatter, then "run skill show". */
static char *
cmd_skill_stub(const struct skill *sk)
{
	char	*s;

	xasprintf(&s,
	    "---\n"
	    "name: %s\n"
	    "description: %s\n"
	    "---\n"
	    "\n"
	    "This skill ships inside the tmux2 binary, so the text below is\n"
	    "never stale. Print it, with your own agent id, this server's name\n"
	    "and the live links filled in for your pane, and follow it:\n"
	    "\n"
	    "```bash\n"
	    "tmux2 skill -t \"$TMUX_PANE\" show %s\n"
	    "```\n"
	    "\n"
	    "Run that every time rather than working from memory: the text\n"
	    "tracks the installed tmux2, and the live block changes as links\n"
	    "come and go.\n",
	    sk->stub, sk->description, sk->name);
	return (s);
}

/* Write the file only when its content differs: 1 written, 0 same, -1 error. */
static int
cmd_skill_write(const char *path, const char *content)
{
	char	*old = NULL;
	size_t	 len = strlen(content), n = 0, cap = 0;
	ssize_t	 got;
	int	 fd, rc;

	fd = open(path, O_RDONLY);
	if (fd != -1) {
		for (;;) {
			if (n == cap) {
				cap = cap == 0 ? 4096 : cap * 2;
				old = xrealloc(old, cap);
			}
			got = read(fd, old + n, cap - n);
			if (got <= 0)
				break;
			n += got;
		}
		close(fd);
		if (n == len && memcmp(old, content, len) == 0) {
			free(old);
			return (0);
		}
		free(old);
	}

	fd = open(path, O_WRONLY|O_CREAT|O_TRUNC, 0644);
	if (fd == -1)
		return (-1);
	rc = 0;
	n = 0;
	while (n < len) {
		got = write(fd, content + n, len - n);
		if (got <= 0) {
			rc = -1;
			break;
		}
		n += got;
	}
	close(fd);
	return (rc == 0 ? 1 : -1);
}

static enum cmd_retval
cmd_skill_install(struct cmdq_item *item, const char *dir,
    const struct skill *only)
{
	const struct skill	*sk;
	const char		*home;
	char			*base, *path, *stub;
	int			 rc;

	if (dir != NULL)
		base = xstrdup(dir);
	else {
		home = find_home();
		if (home == NULL) {
			cmdq_error(item, "skill install: no home directory; "
			    "give -d");
			return (CMD_RETURN_ERROR);
		}
		xasprintf(&base, "%s/.claude/skills", home);
	}

	for (sk = skills; sk->name != NULL; sk++) {
		if (only != NULL && sk != only)
			continue;
		xasprintf(&path, "%s/%s", base, sk->stub);
		if (cmd_skill_mkdirs(path) != 0) {
			cmdq_error(item, "skill install: mkdir %s: %s", path,
			    strerror(errno));
			free(path);
			free(base);
			return (CMD_RETURN_ERROR);
		}
		free(path);
		xasprintf(&path, "%s/%s/SKILL.md", base, sk->stub);
		stub = cmd_skill_stub(sk);
		rc = cmd_skill_write(path, stub);
		free(stub);
		if (rc < 0) {
			cmdq_error(item, "skill install: write %s: %s", path,
			    strerror(errno));
			free(path);
			free(base);
			return (CMD_RETURN_ERROR);
		}
		cmdq_print(item, "%s: %s", path,
		    rc == 1 ? "written" : "up to date");
		free(path);
	}
	free(base);
	return (CMD_RETURN_NORMAL);
}

static enum cmd_retval
cmd_skill_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args		*args = cmd_get_args(self);
	const char		*verb = args_string(args, 0);
	const char		*name;
	const struct skill	*sk = NULL;

	name = args_count(args) > 1 ? args_string(args, 1) : NULL;
	if (name != NULL) {
		sk = cmd_skill_find(name);
		if (sk == NULL) {
			cmdq_error(item, "unknown skill: %s (see \"skill "
			    "list\")", name);
			return (CMD_RETURN_ERROR);
		}
	}

	if (strcmp(verb, "list") == 0)
		return (cmd_skill_list(item));
	if (strcmp(verb, "show") == 0) {
		if (sk == NULL) {
			cmdq_error(item, "usage: skill [-t pane] show <skill>");
			return (CMD_RETURN_ERROR);
		}
		return (cmd_skill_show(item, sk));
	}
	if (strcmp(verb, "install") == 0)
		return (cmd_skill_install(item, args_get(args, 'd'), sk));

	cmdq_error(item, "unknown verb: %s", verb);
	return (CMD_RETURN_ERROR);
}
