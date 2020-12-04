#include <stdlib.h>
#include <pthread.h>
#include "ocalls.h"
#include "../pal_thread_counter.h"

void *exec_libos_thread(void *_thread_data) {
    sgx_enclave_id_t eid = pal_get_enclave_id();
    sgx_status_t status = occlum_ecall_exec_benchmark(eid);
    if (status != SGX_SUCCESS) {
        const char *sgx_err = pal_get_sgx_error_msg(status);
        PAL_ERROR("Failed to enter the enclave to execute the benchmark: %s", sgx_err);
        exit(EXIT_FAILURE);
    }

    pal_thread_counter_dec();
    return NULL;
}

// Start a new host OS thread and enter the enclave to execute the LibOS thread
int occlum_ocall_exec_thread_async(int libos_tid) {
    int ret = 0;
    pthread_t thread;

    pal_thread_counter_inc();
    if ((ret = pthread_create(&thread, NULL, exec_libos_thread, NULL)) < 0) {
        pal_thread_counter_dec();
        return -1;
    }
    pthread_detach(thread);

    return 0;
}
