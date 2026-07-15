/* kevy/c_api.h — a C-callable façade over the C++ kevy CLIENT layer.
 *
 * This is the CLIENT door for C programs: connect(url) to embedded or remote,
 * run raw commands, scalar get/set. (The lower-level EMBEDDED door is the
 * existing crates/kevy-ffi/include/kevy.h; this sits on top of the C++ client
 * so C callers get URL routing + both backends behind one entry point.)
 *
 * Error model (contract §2, C row): functions return an int status — 0 on
 * success, negative = a KevyError kind (see KEVY_STATUS_*). A protocol -ERR
 * reply is a SUCCESSFUL call (status 0) carrying a RESP error frame in the
 * out-buffer; inspect it with any RESP parser. kevy_client_last_error() gives
 * the last error's text for diagnostics.
 */
#ifndef KEVY_CLIENT_C_API_H
#define KEVY_CLIENT_C_API_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct KevyClient KevyClient;

/* A heap buffer owned by this library; free with kevy_client_free_bytes(). */
typedef struct {
  char* ptr;
  size_t len;
} KevyBytes;

/* Status codes: 0 = ok; the negatives mirror KevyError::kind (§2.2). */
enum {
  KEVY_STATUS_OK = 0,
  KEVY_STATUS_STORE = -1,
  KEVY_STATUS_IO = -2,
  KEVY_STATUS_PROTOCOL = -3,
  KEVY_STATUS_READONLY = -4,
  KEVY_STATUS_INVALID_INPUT = -5,
  KEVY_STATUS_NOT_FOUND = -6,
  KEVY_STATUS_UNSUPPORTED = -7,
  KEVY_STATUS_TIMED_OUT = -8,
  KEVY_STATUS_CLOSED = -9,
  KEVY_STATUS_MISUSE = -100 /* null args etc. */
};

/* Open a client for url. On success *out is set and 0 returned. */
int kevy_client_connect(const char* url, KevyClient** out);

/* Close + free. NULL is a no-op; call exactly once. */
void kevy_client_close(KevyClient* c);

/* Run one raw command (argv[0] is the verb). The RESP-encoded reply is
 * written to *out (caller frees via kevy_client_free_bytes). A -ERR reply is
 * status 0 with the error frame in *out. */
int kevy_client_command(KevyClient* c, size_t argc, const char* const* argv,
                        const size_t* argv_len, KevyBytes* out);

/* Scalar GET: on success *found is 1 (value in *out) or 0 (miss). */
int kevy_client_get(KevyClient* c, const char* key, size_t key_len, KevyBytes* out, int* found);

/* Scalar SET (ttl_ms == 0 = no TTL). */
int kevy_client_set(KevyClient* c, const char* key, size_t key_len, const char* val,
                    size_t val_len, unsigned long long ttl_ms);

/* Last error text for c (empty string if none). Valid until the next call. */
const char* kevy_client_last_error(KevyClient* c);

void kevy_client_free_bytes(KevyBytes b);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* KEVY_CLIENT_C_API_H */
