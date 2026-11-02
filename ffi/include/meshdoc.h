/*
 * meshdoc.h — C ABI for the MeshDoc core.
 *
 * Every function returns a heap-allocated NUL-terminated JSON string, or NULL on
 * error. NULL is always accompanied by a message from meshdoc_last_error() valid on
 * the calling thread until the next failing call. Release returned strings with
 * meshdoc_free(). No function ever unwinds across this boundary.
 *
 * Stateless: each call opens the encrypted store, performs one operation and closes.
 * See docs/INTERFACES.md section A for the JSON shapes.
 */
#ifndef MESHDOC_H
#define MESHDOC_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

const char *meshdoc_version(void);
void        meshdoc_free(char *s);
const char *meshdoc_last_error(void);

/* Identity */
char *meshdoc_init(const char *store, const char *pass);
char *meshdoc_identity(const char *store, const char *pass);

/* Workspace lifecycle */
char *meshdoc_workspace_create(const char *store, const char *pass, const char *name);
char *meshdoc_workspace_list(const char *store, const char *pass);
char *meshdoc_workspace_invite(const char *store, const char *pass,
                               const char *ws, const char *joiner_public_hex);
char *meshdoc_workspace_join(const char *store, const char *pass, const char *token);

/* Documents. `ws` may be "" to mean the only workspace in the store. */
char *meshdoc_state(const char *store, const char *pass, const char *ws);
char *meshdoc_document(const char *store, const char *pass, const char *ws,
                       const char *doc_id);
char *meshdoc_doc_create(const char *store, const char *pass, const char *ws,
                         const char *type, const char *title);
char *meshdoc_checklist_add(const char *store, const char *pass, const char *ws,
                            const char *doc_id, const char *text);
char *meshdoc_checklist_toggle(const char *store, const char *pass, const char *ws,
                               const char *doc_id, const char *item_id, int32_t done);

/* Sync. The socket is opened by Rust; callers never handle a file descriptor. */
char *meshdoc_sync_pull(const char *store, const char *pass, const char *ws,
                        const char *host, uint16_t port);
char *meshdoc_sync_serve(const char *store, const char *pass, const char *ws,
                         uint16_t port, int32_t max_sessions);

/* Rotating BLE discovery tag for this workspace at a unix timestamp. */
char *meshdoc_discovery_key(const char *store, const char *pass, const char *ws,
                            uint64_t unix_seconds);

#ifdef __cplusplus
}
#endif

#endif /* MESHDOC_H */
