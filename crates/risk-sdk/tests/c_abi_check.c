#define _CRT_SECURE_NO_WARNINGS

#include "../include/ponytail_risk_sdk.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
typedef HMODULE library_handle;
#define LOAD_LIBRARY(path) LoadLibraryA(path)
#define LOAD_SYMBOL(handle, name) GetProcAddress(handle, name)
#define CLOSE_LIBRARY(handle) FreeLibrary(handle)
#else
#include <dlfcn.h>
typedef void *library_handle;
#define LOAD_LIBRARY(path) dlopen(path, RTLD_NOW | RTLD_LOCAL)
#define LOAD_SYMBOL(handle, name) dlsym(handle, name)
#define CLOSE_LIBRARY(handle) dlclose(handle)
#endif

typedef int32_t (*pgr_init_fn)(const pgr_config_v1 *);
typedef int32_t (*pgr_emit_json_fn)(const char *, size_t);
typedef int32_t (*pgr_check_json_fn)(const char *, size_t, char *, size_t *);
typedef int32_t (*pgr_action_json_fn)(const char *, size_t, char *, size_t *);
typedef int32_t (*pgr_flush_fn)(uint32_t);
typedef void (*pgr_shutdown_fn)(void);

static char *read_file(const char *path, size_t *length) {
    FILE *file = fopen(path, "rb");
    long size;
    char *data;
    if (file == NULL || fseek(file, 0, SEEK_END) != 0) {
        if (file != NULL) fclose(file);
        return NULL;
    }
    size = ftell(file);
    if (size <= 0 || fseek(file, 0, SEEK_SET) != 0) {
        fclose(file);
        return NULL;
    }
    data = (char *)malloc((size_t)size);
    if (data == NULL || fread(data, 1, (size_t)size, file) != (size_t)size) {
        free(data);
        fclose(file);
        return NULL;
    }
    fclose(file);
    *length = (size_t)size;
    return data;
}

static void *required_symbol(library_handle library, const char *name) {
    void *symbol = (void *)LOAD_SYMBOL(library, name);
    if (symbol == NULL) fprintf(stderr, "missing SDK export: %s\n", name);
    return symbol;
}

int main(int argc, char **argv) {
    static const char decision_request[] =
        "{\"schema_version\":\"1.0\",\"request_id\":\"decision-c-abi-0001\","
        "\"occurred_at\":\"2026-07-31T00:10:22.381+08:00\",\"action_type\":\"trade.commit\","
        "\"transaction_id\":\"trade-c-abi\",\"actor\":{\"player_id\":\"10001\","
        "\"account_id\":\"account-11\",\"session_id\":\"session-781\"},"
        "\"proposed_changes\":{\"currency_changes\":[],\"asset_changes\":[]},\"timeout_ms\":20}";
    const char *token = getenv("PGR_TEST_LOCAL_TOKEN");
    library_handle library;
    pgr_init_fn sdk_init;
    pgr_emit_json_fn sdk_emit;
    pgr_check_json_fn sdk_check;
    pgr_action_json_fn sdk_pull_actions;
    pgr_action_json_fn sdk_ack_action;
    pgr_flush_fn sdk_flush;
    pgr_shutdown_fn sdk_shutdown;
    pgr_config_v1 config;
    char *batch;
    size_t batch_length = 0;
    size_t response_capacity = 0;
    char *response = NULL;
    int32_t result;
    int exit_code = 1;

    if (argc != 4 || token == NULL) {
        fprintf(stderr, "usage: c_abi_check <library> <loopback-endpoint> <batch-json>\n");
        return 2;
    }
    library = LOAD_LIBRARY(argv[1]);
    if (library == NULL) {
        fprintf(stderr, "cannot load SDK library\n");
        return 3;
    }
    sdk_init = (pgr_init_fn)required_symbol(library, "pgr_init");
    sdk_emit = (pgr_emit_json_fn)required_symbol(library, "pgr_emit_json");
    sdk_check = (pgr_check_json_fn)required_symbol(library, "pgr_check_json");
    sdk_pull_actions = (pgr_action_json_fn)required_symbol(library, "pgr_pull_actions");
    sdk_ack_action = (pgr_action_json_fn)required_symbol(library, "pgr_ack_action");
    sdk_flush = (pgr_flush_fn)required_symbol(library, "pgr_flush");
    sdk_shutdown = (pgr_shutdown_fn)required_symbol(library, "pgr_shutdown");
    if (sdk_init == NULL || sdk_emit == NULL || sdk_check == NULL || sdk_pull_actions == NULL || sdk_ack_action == NULL || sdk_flush == NULL || sdk_shutdown == NULL) {
        CLOSE_LIBRARY(library);
        return 4;
    }

    batch = read_file(argv[3], &batch_length);
    if (batch == NULL) {
        fprintf(stderr, "cannot read batch JSON\n");
        CLOSE_LIBRARY(library);
        return 5;
    }
    config.abi_version = PGR_ABI_VERSION;
    config.endpoint_utf8 = argv[2];
    config.local_token_utf8 = token;
    config.emit_timeout_ms = 1;
    config.check_timeout_ms = 1000;
    config.queue_capacity = 32;

    result = sdk_init(&config);
    if (result != PGR_OK) {
        fprintf(stderr, "pgr_init failed: %d\n", result);
        goto cleanup;
    }
    result = sdk_emit(batch, batch_length);
    if (result != PGR_OK) {
        fprintf(stderr, "pgr_emit_json failed: %d\n", result);
        sdk_shutdown();
        goto cleanup;
    }
    result = sdk_flush(5000);
    if (result != PGR_OK) {
        fprintf(stderr, "pgr_flush failed: %d\n", result);
        sdk_shutdown();
        goto cleanup;
    }

    result = sdk_check(decision_request, sizeof(decision_request) - 1, NULL, &response_capacity);
    if (result != PGR_ERR_BUFFER_TOO_SMALL || response_capacity < 2) {
        fprintf(stderr, "response sizing failed: result=%d capacity=%zu\n", result, response_capacity);
        sdk_shutdown();
        goto cleanup;
    }
    response = (char *)malloc(response_capacity);
    if (response == NULL) {
        sdk_shutdown();
        goto cleanup;
    }
    result = sdk_check(
        decision_request,
        sizeof(decision_request) - 1,
        response,
        &response_capacity
    );
    sdk_shutdown();
    if (result != PGR_OK || strstr(response, "\"decision\":\"allow\"") == NULL ||
        strstr(response, "\"mode\":\"shadow\"") == NULL) {
        fprintf(stderr, "decision check failed: result=%d response=%s\n", result, response);
        goto cleanup;
    }

    printf("C ABI check ok: emitted=7 decision=allow/shadow\n");
    exit_code = 0;

cleanup:
    free(response);
    free(batch);
    CLOSE_LIBRARY(library);
    return exit_code;
}
