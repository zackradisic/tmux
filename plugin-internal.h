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

/* plugin-vtable.c */
void	 plugin_vtable_log(int, const char *, const char *);
void	 plugin_vtable_list_objects(int, pgh_sink, void *);
int	 plugin_vtable_resolve_object(int, u_int, pgh_sink, void *);
int	 plugin_vtable_send_keys(u_int, const char *, int);
int	 plugin_vtable_capture_pane(u_int, int, int, int, pgh_sink, void *);
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
int	 plugin_mode_pending_take(uint64_t *);
void	 plugin_mode_event(uint64_t, const char *, const char *);
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
