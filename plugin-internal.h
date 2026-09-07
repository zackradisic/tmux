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

#ifndef PLUGIN_INTERNAL_H
#define PLUGIN_INTERNAL_H

/*
 * Internal interfaces between the plugin glue files (plugin.c,
 * plugin-vtable.c, plugin-events.c, cmd-plugin.c). These depend on types
 * from plugin-host.h so they cannot live in tmux.h; include this after
 * tmux.h and plugin-host.h.
 */

/* Hard cap on lines emitted by a single capture_pane call. */
#define PLUGIN_CAPTURE_MAX_LINES 2000

/*
 * panes_search: default and hard cap on the lines searched per pane
 * (from the bottom up), and the longest snippet returned per match.
 * The flag bits mirror abi-types `search_flags`; keep them in sync.
 */
#define PLUGIN_SEARCH_MAX_LINES 5000
#define PLUGIN_SEARCH_SNIPPET_MAX 240
/* flags word: low two bits are the matcher mode. */
#define PGH_SEARCH_MODE_MASK 0x3
#define PGH_SEARCH_MODE_PLAIN 0
#define PGH_SEARCH_MODE_REGEX 1
#define PGH_SEARCH_MODE_FUZZY 2
#define PGH_SEARCH_CASE_SENSITIVE 0x4
#define PGH_SEARCH_MULTILINE 0x8

/*
 * Cap on job output bytes carried in one async completion (mirrors
 * abi-types MAX_JOB_OUTPUT_BYTES; keep in sync).
 */
#define PLUGIN_JOB_OUTPUT_MAX (256 * 1024)

/* plugin-buf.c: binary emitters for the ABI wire formats. */
struct plugin_buf;
struct plugin_buf *plugin_buf_create(void);
void	 plugin_buf_free(struct plugin_buf *);
void	 plugin_buf_u8(struct plugin_buf *, uint8_t);
void	 plugin_buf_u32(struct plugin_buf *, uint32_t);
void	 plugin_buf_u64(struct plugin_buf *, uint64_t);
void	 plugin_buf_str(struct plugin_buf *, const char *);
const u_char *plugin_buf_data(struct plugin_buf *, size_t *);
struct plugin_buf *plugin_event_create(const char *);
void	 plugin_event_scope(struct plugin_buf *, int, uint32_t);
int	 plugin_event_scope_set(struct plugin_buf *, int);
void	 plugin_event_str(struct plugin_buf *, const char *, const char *);
void	 plugin_event_i64(struct plugin_buf *, const char *, long long);
void	 plugin_event_bool(struct plugin_buf *, const char *, int);
void	 plugin_event_send(struct plugin_buf *);
void	 plugin_event_send_mode(struct plugin_buf *, uint64_t);

/* plugin-vtable.c */
void	 plugin_vtable_log(int, const char *, const char *);
void	 plugin_vtable_list_objects(int, pgh_sink, void *);
int	 plugin_vtable_resolve_object(int, u_int, pgh_sink, void *);
int64_t	 plugin_vtable_obj_relation(int, u_int, u_int);
int	 plugin_vtable_format_expand(int, u_int, const char *, pgh_sink,
	     void *);
int	 plugin_vtable_send_keys(u_int, const char *, int);
int	 plugin_vtable_capture_pane(u_int, int, int, int, pgh_sink, void *);
int	 plugin_vtable_pane_env(u_int, const char *, pgh_sink, void *);
int	 plugin_vtable_pane_fds(u_int, pgh_sink, void *);
int	 plugin_vtable_panes_search(const uint32_t *, uint32_t, const char *,
	    uint32_t, uint32_t, pgh_sink, void *);
int	 plugin_vtable_pane_pid(u_int);
int	 plugin_vtable_get_option(int, u_int, const char *, pgh_sink, void *);
int	 plugin_vtable_set_option(int, u_int, const char *, const char *);
int	 plugin_vtable_display_message(int, const char *, const char *);
void	 plugin_vtable_state_changed(const char *, const char *,
	     const char *);

/* plugin-async.c */
int	 plugin_vtable_run_job(const char *, const char *, uint64_t);
int	 plugin_vtable_run_command(const char *, uint64_t);
uint64_t plugin_vtable_timer_start(uint64_t, uint64_t);
int	 plugin_vtable_timer_cancel(uint64_t);
void	 plugin_async_shutdown(void);

/* plugin-mode.c */
int64_t	 plugin_vtable_mode_open(u_int, u_int, u_int, int, int,
	     const char *);
int	 plugin_vtable_mode_write(uint64_t, const u_char *, size_t);
int	 plugin_vtable_mode_preview(uint64_t, int64_t, u_int, u_int, u_int,
	     u_int);
int	 plugin_vtable_mode_close(uint64_t);
int	 plugin_vtable_mode_move(uint64_t, u_int, int, int);
int	 plugin_vtable_mode_resize(uint64_t, u_int, u_int);
int	 plugin_mode_pending_take(uint64_t *);
void	 plugin_mode_unregister(uint64_t);
void	 plugin_mode_shutdown(void);

/* window-plugin-mode.c */
uint64_t window_plugin_mode_id(struct window_mode_entry *);
void	 window_plugin_mode_set_close_reason(struct window_mode_entry *,
	     const char *);
void	 window_plugin_mode_write(struct window_mode_entry *,
	     const u_char *, size_t);
int	 window_plugin_mode_preview(struct window_mode_entry *, int64_t,
	     u_int, u_int, u_int, u_int);

/* plugin-events.c */
void	 plugin_events_init(void);
void	 plugin_events_shutdown(void);

#endif /* PLUGIN_INTERNAL_H */
