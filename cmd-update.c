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
#include <sys/wait.h>

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tmux.h"

/*
 * Update the tmux2 install from a GitHub release, then restart the server
 * in place so the pane processes stay alive.
 *
 * The running binary must be <home>/bin/tmux as laid out by
 * install-tmux2.sh, with the installed tag in <home>/VERSION. The command
 * resolves the latest release (or takes a tag), asks on the client's stdin
 * unless -y is given, runs the release's install-tmux2.sh into <home>, and
 * hands the server off to the new binary.
 */

#define UPDATE_DEFAULT_REPO "zackradisic/tmux"

static enum cmd_retval	cmd_update_exec(struct cmd *, struct cmdq_item *);

const struct cmd_entry cmd_update_entry = {
	.name = "update",
	.alias = NULL,

	.args = { "ny", 0, 1, NULL },
	.usage = "[-ny] [version]",

	.flags = CMD_CLIENT_CANFAIL,
	.exec = cmd_update_exec
};

struct cmd_update_data {
	struct cmdq_item	*item;
	struct client		*c;

	/* Refs: the command flow, plus one per job or stdin read in flight. */
	int			 refs;
	int			 finished;
	int			 decided;

	int			 yes;
	int			 check_only;

	char			*repo;
	char			*home;
	char			*binary;
	char			*installed;
	char			*wanted;
	char			*target;
};

static void	cmd_update_resolve_callback(struct job *);
static void	cmd_update_install_callback(struct job *);
static void	cmd_update_stdin_callback(struct client *, const char *, int,
		    int, struct evbuffer *, void *);
static void	cmd_update_confirm(struct cmd_update_data *);
static void	cmd_update_confirm_later(int, short, void *);
static void	cmd_update_install(struct cmd_update_data *);

static void
cmd_update_unref(struct cmd_update_data *cdata)
{
	if (--cdata->refs > 0)
		return;
	free(cdata->repo);
	free(cdata->home);
	free(cdata->binary);
	free(cdata->installed);
	free(cdata->wanted);
	free(cdata->target);
	free(cdata);
}

/* Let the command finish. Runs once; later callbacks only drop refs. */
static void
cmd_update_finish(struct cmd_update_data *cdata)
{
	if (cdata->finished)
		return;
	cdata->finished = 1;
	cmdq_continue(cdata->item);
	cmd_update_unref(cdata);
}

static void
cmd_update_fail(struct cmd_update_data *cdata, const char *fmt, ...)
{
	va_list	 ap;
	char	*msg;

	if (cdata->finished)
		return;
	va_start(ap, fmt);
	xvasprintf(&msg, fmt, ap);
	va_end(ap);
	cmdq_error(cdata->item, "update: %s", msg);
	free(msg);
	cmd_update_finish(cdata);
}

/* Read the tag out of <home>/VERSION. */
static char *
cmd_update_read_version(const char *home)
{
	char	*path, line[256], *nl;
	FILE	*f;

	xasprintf(&path, "%s/VERSION", home);
	f = fopen(path, "r");
	free(path);
	if (f == NULL)
		return (NULL);
	if (fgets(line, sizeof line, f) == NULL) {
		fclose(f);
		return (NULL);
	}
	fclose(f);
	if ((nl = strchr(line, '\n')) != NULL)
		*nl = '\0';
	if (*line == '\0')
		return (NULL);
	return (xstrdup(line));
}

/* Drain a job's output into the client, one line at a time. */
static void
cmd_update_print_output(struct cmd_update_data *cdata, struct job *job)
{
	struct bufferevent	*event = job_get_event(job);
	char			*line;
	size_t			 size;

	while ((line = evbuffer_readln(event->input, NULL,
	    EVBUFFER_EOL_LF)) != NULL) {
		cmdq_print(cdata->item, "%s", line);
		free(line);
	}
	size = EVBUFFER_LENGTH(event->input);
	if (size != 0) {
		line = xmalloc(size + 1);
		memcpy(line, EVBUFFER_DATA(event->input), size);
		line[size] = '\0';
		cmdq_print(cdata->item, "%s", line);
		free(line);
	}
}

/* Exit code of a job, or -1 if it did not exit normally. */
static int
cmd_update_job_status(struct job *job)
{
	int	status = job_get_status(job);

	if (WIFEXITED(status))
		return (WEXITSTATUS(status));
	return (-1);
}

static enum cmd_retval
cmd_update_exec(struct cmd *self, struct cmdq_item *item)
{
	struct args		*args = cmd_get_args(self);
	struct client		*c = cmdq_get_client(item);
	struct cmd_update_data	*cdata;
	struct environ_entry	*envent;
	char			*cause = NULL, *cmd, *suffix;
	const char		*want;

	if (c == NULL) {
		cmdq_error(item, "update: needs a client");
		return (CMD_RETURN_ERROR);
	}

	cdata = xcalloc(1, sizeof *cdata);
	cdata->item = item;
	cdata->c = c;
	cdata->refs = 1;
	cdata->yes = args_has(args, 'y');
	cdata->check_only = args_has(args, 'n');

	envent = environ_find(global_environ, "TMUX2_REPO");
	if (envent != NULL && envent->value != NULL && *envent->value != '\0')
		cdata->repo = xstrdup(envent->value);
	else
		cdata->repo = xstrdup(UPDATE_DEFAULT_REPO);

	/* The running binary tells us where the install lives. */
	/*
	 * Errors before the command has returned CMD_RETURN_WAIT go out
	 * directly: cmdq_continue on an item that is not yet waiting would
	 * be lost, and the client would hang.
	 */
	cdata->binary = server_handoff_self_binary(&cause);
	if (cdata->binary == NULL) {
		cmdq_error(item, "update: %s", cause);
		free(cause);
		cmd_update_unref(cdata);
		return (CMD_RETURN_ERROR);
	}
	suffix = strstr(cdata->binary, "/bin/tmux");
	if (suffix == NULL || suffix == cdata->binary ||
	    suffix[sizeof "/bin/tmux" - 1] != '\0') {
		cmdq_error(item, "update: %s is not an install-tmux2.sh layout "
		    "(<home>/bin/tmux); run the install script by hand",
		    cdata->binary);
		cmd_update_unref(cdata);
		return (CMD_RETURN_ERROR);
	}
	cdata->home = xstrndup(cdata->binary, suffix - cdata->binary);
	cdata->installed = cmd_update_read_version(cdata->home);
	if (cdata->installed == NULL) {
		cmdq_error(item, "update: %s/VERSION is missing; run the "
		    "install script by hand", cdata->home);
		cmd_update_unref(cdata);
		return (CMD_RETURN_ERROR);
	}

	want = args_string(args, 0);
	if (want != NULL && *want != '\0') {
		cdata->wanted = xstrdup(want);
		cdata->target = xstrdup(want);
		/* Confirm only once this item is waiting. */
		cdata->refs++;
		event_once(-1, EV_TIMEOUT, cmd_update_confirm_later, cdata,
		    NULL);
		return (CMD_RETURN_WAIT);
	}

	/* Ask GitHub which release is the latest. */
	xasprintf(&cmd, "curl -fsSL "
	    "https://api.github.com/repos/%s/releases/latest | "
	    "sed -n 's/.*\"tag_name\": *\"\\([^\"]*\\)\".*/\\1/p' | head -1",
	    cdata->repo);
	cdata->refs++;
	if (job_run(cmd, 0, NULL, NULL, NULL, NULL, NULL,
	    cmd_update_resolve_callback, NULL, cdata, 0, -1, -1) == NULL) {
		cdata->refs--;
		cmdq_error(item, "update: failed to run %s", cmd);
		free(cmd);
		cmd_update_unref(cdata);
		return (CMD_RETURN_ERROR);
	}
	free(cmd);
	return (CMD_RETURN_WAIT);
}

/* Deferred confirm for the explicit-version path. */
static void
cmd_update_confirm_later(__unused int fd, __unused short events, void *data)
{
	struct cmd_update_data	*cdata = data;

	cmd_update_confirm(cdata);
	cmd_update_unref(cdata);
}

static void
cmd_update_resolve_callback(struct job *job)
{
	struct cmd_update_data	*cdata = job_get_data(job);
	struct bufferevent	*event = job_get_event(job);
	char			*line;

	line = evbuffer_readln(event->input, NULL, EVBUFFER_EOL_ANY);
	if (cmd_update_job_status(job) != 0 || line == NULL ||
	    *line == '\0') {
		free(line);
		cmd_update_fail(cdata, "cannot resolve the latest release of "
		    "%s (is curl installed and the network up?)", cdata->repo);
		cmd_update_unref(cdata);
		return;
	}
	cdata->target = line;
	cmd_update_confirm(cdata);
	cmd_update_unref(cdata);
}

/* Report the versions, then ask, install, or stop. */
static void
cmd_update_confirm(struct cmd_update_data *cdata)
{
	struct client	*c = cdata->c;

	if (cdata->wanted == NULL &&
	    strcmp(cdata->installed, cdata->target) == 0) {
		cmdq_print(cdata->item, "tmux2 %s is up to date (%s)",
		    cdata->installed, cdata->home);
		cmd_update_finish(cdata);
		return;
	}
	cmdq_print(cdata->item, "tmux2: installed %s, available %s (%s)",
	    cdata->installed, cdata->target, cdata->home);
	if (cdata->check_only) {
		cmd_update_finish(cdata);
		return;
	}
	if (cdata->yes) {
		cmd_update_install(cdata);
		return;
	}
	if (c->flags & (CLIENT_ATTACHED|CLIENT_CONTROL)) {
		cmd_update_fail(cdata, "cannot ask on this client; run "
		    "\"tmux update\" from a shell, use -y, or bind "
		    "\"confirm-before -p 'update tmux2? (y/n)' 'update -y'\"");
		return;
	}
	cmdq_print(cdata->item,
	    "Update to %s and restart the server (pane processes stay "
	    "alive)? [y/N]", cdata->target);
	cdata->refs++;
	if (file_read(c, "-", cmd_update_stdin_callback, cdata) == NULL) {
		/* The done callback still fires and drops the ref. */
		return;
	}
}

static void
cmd_update_stdin_callback(__unused struct client *c, __unused const char *path,
    int error, int closed, struct evbuffer *buffer, void *data)
{
	struct cmd_update_data	*cdata = data;
	char			*line;
	int			 yes;

	if (!cdata->decided) {
		line = NULL;
		if (buffer != NULL) {
			line = evbuffer_readln(buffer, NULL, EVBUFFER_EOL_ANY);
			if (line == NULL && closed &&
			    EVBUFFER_LENGTH(buffer) != 0) {
				/* EOF without a newline: take what there is. */
				line = xstrndup(EVBUFFER_DATA(buffer),
				    EVBUFFER_LENGTH(buffer));
			}
		}
		if (line != NULL || closed) {
			cdata->decided = 1;
			yes = (line != NULL && (*line == 'y' || *line == 'Y'));
			free(line);
			if (error != 0 && !yes) {
				cmd_update_fail(cdata, "reading stdin: %s",
				    strerror(error));
			} else if (yes)
				cmd_update_install(cdata);
			else {
				cmdq_print(cdata->item, "update cancelled");
				cmd_update_finish(cdata);
			}
		}
	}
	if (closed)
		cmd_update_unref(cdata);
}

/* Run the release's install script into <home>. */
static void
cmd_update_install(struct cmd_update_data *cdata)
{
	char	*cmd, *qhome, *qtag;

	/*
	 * Download the script to a file first. Piped into sh, a failed
	 * download is an empty script, and that exits 0.
	 */
	qhome = server_handoff_shell_quote(cdata->home);
	qtag = server_handoff_shell_quote(cdata->target);
	xasprintf(&cmd, "t=$(mktemp) || exit 1; "
	    "if curl -fsSL -o \"$t\" "
	    "https://github.com/%s/releases/download/%s/install-tmux2.sh; then "
	    "TMUX2_HOME=%s TMUX2_REPO=%s sh \"$t\" %s; rc=$?; "
	    "else echo \"cannot download install-tmux2.sh for %s\"; rc=1; fi; "
	    "rm -f \"$t\"; exit $rc",
	    cdata->repo, cdata->target, qhome, cdata->repo, qtag, cdata->target);
	free(qhome);
	free(qtag);

	cmdq_print(cdata->item, "downloading tmux2 %s into %s ...",
	    cdata->target, cdata->home);
	cdata->refs++;
	if (job_run(cmd, 0, NULL, NULL, NULL, NULL, NULL,
	    cmd_update_install_callback, NULL, cdata, 0, -1, -1) == NULL) {
		cdata->refs--;
		cmd_update_fail(cdata, "failed to run %s", cmd);
	}
	free(cmd);
}

static void
cmd_update_install_callback(struct job *job)
{
	struct cmd_update_data	*cdata = job_get_data(job);
	char			*cause = NULL, *installed;
	int			 rc;

	cmd_update_print_output(cdata, job);
	rc = cmd_update_job_status(job);
	if (rc != 0) {
		cmd_update_fail(cdata, "install-tmux2.sh failed (%d); the "
		    "running install is unchanged", rc);
		cmd_update_unref(cdata);
		return;
	}

	/* Trust the file, not the request: the script wrote what it got. */
	installed = cmd_update_read_version(cdata->home);
	if (installed == NULL || strcmp(installed, cdata->target) != 0) {
		cmd_update_fail(cdata, "%s/VERSION says %s, not %s; the "
		    "install did not complete and the server was not restarted",
		    cdata->home, installed != NULL ? installed : "nothing",
		    cdata->target);
		free(installed);
		cmd_update_unref(cdata);
		return;
	}
	cmdq_print(cdata->item, "tmux2 %s installed; restarting the server",
	    installed);
	free(installed);

	if (server_handoff_begin(cdata->binary, &cause) != 0) {
		cmd_update_fail(cdata, "installed, but restart-server failed: "
		    "%s; run \"tmux restart-server\" by hand", cause);
		free(cause);
		cmd_update_unref(cdata);
		return;
	}
	cmd_update_finish(cdata);
	cmd_update_unref(cdata);
}
