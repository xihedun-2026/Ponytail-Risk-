#ifndef PONYTAIL_RISK_SDK_H
#define PONYTAIL_RISK_SDK_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define PGR_ABI_VERSION 1u

#define PGR_OK 0
#define PGR_ERR_INVALID_ARGUMENT (-1)
#define PGR_ERR_RETRY (-2)
#define PGR_ERR_TIMEOUT (-3)
#define PGR_ERR_BUFFER_TOO_SMALL (-4)
#define PGR_ERR_AGENT_REJECTED (-5)
#define PGR_ERR_STATE (-6)
#define PGR_ERR_INTERNAL (-7)

typedef struct pgr_config_v1 {
    uint32_t abi_version;
    /* http://127.0.0.1:<port> or https://<platform-domain>/sdk/v1 */
    const char *endpoint_utf8;
    /* Local Agent token for loopback; generated SDK key for HTTPS. */
    const char *local_token_utf8;
    uint32_t emit_timeout_ms;
    uint32_t check_timeout_ms;
    uint32_t queue_capacity;
} pgr_config_v1;

/* json_utf8 is one complete plugin-event-batch.v1 JSON document. */
int32_t pgr_init(const pgr_config_v1 *config);
int32_t pgr_emit_json(const char *json_utf8, size_t json_len);
int32_t pgr_check_json(
    const char *request_utf8,
    size_t request_len,
    char *response_utf8,
    size_t *response_capacity
);
/* Remote platform command channel. Local Agent deployments may return PGR_ERR_AGENT_REJECTED. */
int32_t pgr_pull_actions(
    const char *request_utf8,
    size_t request_len,
    char *response_utf8,
    size_t *response_capacity
);
int32_t pgr_ack_action(
    const char *request_utf8,
    size_t request_len,
    char *response_utf8,
    size_t *response_capacity
);
int32_t pgr_flush(uint32_t timeout_ms);
void pgr_shutdown(void);

#ifdef __cplusplus
}
#endif

#endif
