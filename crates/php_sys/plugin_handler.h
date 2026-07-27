#ifndef RAPIRA_PLUGIN_HANDLER_H
#define RAPIRA_PLUGIN_HANDLER_H

#include "wrapper.h"

// Counters behind HttpHandler::getInfo(). Keep in sync with RapiraRuntimeInfo
// in src/plugin_handler.rs (#[repr(C)]). `state` points at a 'static string
// owned by the Rust side.
typedef struct {
    const char *state;
    uint32_t pid;
    uint64_t queued;
    uint64_t handled;
    uint64_t errors;
    uint64_t recycles;
    uint64_t restarts;
} rapira_runtime_info;

// Called from MINIT: registers every class rapira exposes to userland.
void rapira_register_classes(void);

// gen_stub emits ext_functions[] as a static table, so it is reachable only
// from plugin_handler.c; module.c installs it into the module entry through here.
const zend_function_entry *rapira_php_functions(void);

#endif // RAPIRA_PLUGIN_HANDLER_H
