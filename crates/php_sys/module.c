#include "wrapper.h"

extern bool rapira_rs_handle_request(
    zend_fcall_info *fci,
    zend_fcall_info_cache *fcc); // Rust: main handler: rapira_worker.rs
extern void rapira_rs_finish_response(void); // Rust: just ctx.finish()

// in rust with repr[c]
enum {
    OK = 0,
    BAILOUT = 1,
    EXIT = 2,
    THROW = 3,
};

// found out, that php_output_end_all can also bailout
// so wrap it in a zend_try block, and return BAILOUT if it does
int rapira_finish_output(void) {
    zend_try {
        php_output_end_all();
        php_header(); // no-op if SG(headers_sent) is true
    }
    zend_catch { return BAILOUT; }
    zend_end_try();

    return OK;
}

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_handle_request, 0, 1,
                                        _IS_BOOL, 0)
ZEND_ARG_INFO(0, handler)
ZEND_END_ARG_INFO()
ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_finish_request, 0, 0,
                                        _IS_BOOL, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(rapira_handle_request) {
    zend_fcall_info fci;
    zend_fcall_info_cache fcc;
    ZEND_PARSE_PARAMETERS_START(1, 1) // min-max
    Z_PARAM_FUNC(fci, fcc)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(rapira_rs_handle_request(&fci, &fcc));
}

PHP_FUNCTION(rapira_finish_request) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (rapira_finish_output()) { // non-zero == BAILOUT: re-raise so
        zend_bailout(); // rapira_run_handler classifies + 500s + recycles
    }
    rapira_rs_finish_response(); // commit the response stream to the client now
    RETURN_TRUE;
}

static const zend_function_entry rapira_functions[] = {
    PHP_FE(rapira_handle_request, arginfo_rapira_handle_request)
        PHP_FE(rapira_finish_request, arginfo_rapira_finish_request)
            PHP_FE_END};

zend_module_entry rapira_module_entry = {
    STANDARD_MODULE_HEADER,
    "rapira",
    rapira_functions,
    NULL,
    NULL,
    NULL,
    NULL,
    NULL, /* MINIT, MSHUTDOWN, RINIT, RSHUTDOWN, MINFO */
    "0.1.0",
    STANDARD_MODULE_PROPERTIES};

/* ext/filter caches raw input copies in its module globals; only its RSHUTDOWN
frees them (filter.c:190-196), and sapi_activate()'s input_filter_init()
UNDEFs them WITHOUT freeing (filter.c:232-240, wired via SAPI.c:484). The
worker path runs sapi_activate per job but RSHUTDOWN only at cycle end, so
each job would orphan the previous job's arrays - run the module's own
request cycle per job.
*/
static const char *RELOAD_MODULES[] = {"filter", NULL};

static void rapira_modules_rshutdown(void) {
    zend_module_entry *module;
    for (const char **name = RELOAD_MODULES; *name; name++) {
        module = zend_hash_str_find_ptr(&module_registry, *name, strlen(*name));
        if (module && module->request_shutdown_func) {
            module->request_shutdown_func(module->type, module->module_number);
        }
    }
}

static void rapira_modules_rinit(void) {
    zend_module_entry *module;
    for (const char **name = RELOAD_MODULES; *name; name++) {
        module = zend_hash_str_find_ptr(&module_registry, *name, strlen(*name));
        if (module && module->request_startup_func) {
            module->request_startup_func(module->type, module->module_number);
        }
    }
}

// A bailout longjmps past the observer end handlers of every frame it abandons.
// php_request_shutdown repairs this via zend_observer_fcall_end_all(), which
// worker mode never reaches. Close only frames above `base` - the resident
// script's own observed frames must stay open, so plain end_all() would be
// wrong here.
static void rapira_observer_end_to(zend_execute_data *base) {
    if (!ZEND_OBSERVER_ENABLED) {
        return;
    }
    zend_execute_data *orig = EG(current_execute_data);
    while (EG(current_observed_frame) && EG(current_observed_frame) != base) {
        EG(current_execute_data) = EG(current_observed_frame);
        zend_observer_fcall_end_prechecked(EG(current_observed_frame), NULL);
    }
    EG(current_execute_data) = orig;
}

/* Per-request state php_request_startup() resets that the worker path skips. */
void rapira_request_init(void) {
    PG(connection_status) = PHP_CONNECTION_NORMAL;
    PG(header_is_being_sent) = 0;
    // recursion/context guards a bailout can strand: a fatal inside a userland
    // include-wrapper's stream_open leaves in_user_include=1 (later URL opens
    // rejected as includes); a fatal inside php_log_err leaves in_error_log=1
    // (later error-log writes dropped).
    PG(in_error_log) = 0;
    PG(in_user_include) = 0;
    // php-src clears this per request via init_compiler (zend_compile.c:461); a
    // resident worker runs many jobs per cycle, and a non-recycling client
    // abort leaves it =1 (zend.c:1264), defeating rapira_run_handler's
    // clean_at_entry detector for later jobs. Re-clear it at each job start.
    CG(unclean_shutdown) = 0;

#if defined(ZEND_MAX_EXECUTION_TIMERS) || !defined(ZTS)
    /* per-request execution timer; teardown unsets it */
    if (PG(max_input_time) == -1) {
        zend_set_timeout(EG(timeout_seconds), 1);
    } else {
        zend_set_timeout(PG(max_input_time), 1);
    }
#else
    zend_unset_timeout();
#endif

    if (PG(expose_php)) {
        sapi_add_header(SAPI_PHP_VERSION_HEADER,
                        sizeof(SAPI_PHP_VERSION_HEADER) - 1, 1);
    }

    /* main/main.c php_request_startup(): honor the output INIs per request */
    if (PG(output_handler) && PG(output_handler)[0]) {
        zval oh;
        ZVAL_STRING(&oh, PG(output_handler));
        php_output_start_user(&oh, 0, PHP_OUTPUT_HANDLER_STDFLAGS);
        zval_ptr_dtor(&oh);
    } else if (PG(output_buffering)) {
        php_output_start_user(
            NULL, PG(output_buffering) > 1 ? PG(output_buffering) : 0,
            PHP_OUTPUT_HANDLER_STDFLAGS);
    } else if (PG(implicit_flush)) {
        php_output_set_implicit_flush(1);
    }
}

// all superglobals are registered as a zend_auto_global in the CG(auto_globals)
// typedef bool (*zend_auto_global_callback)(zend_string *name);
// typedef struct _zend_auto_global {
// 	zend_string *name;
// 	zend_auto_global_callback auto_global_callback;
// 	bool jit;
// 	bool armed; <-- means, that superglobal is still needs to be build
// (true) or not (false)
// } zend_auto_global;

// ^ this is the struct that holds the superglobals
//
void rapira_activate_auto_globals(void) {
    zend_auto_global *auto_global;
    // 	_(ZEND_STR_AUTOGLOBAL_ENV,  "_ENV")
    zend_string *_env = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_ENV);

    // re-arm all = true for all superglobals, to be rebuilt
    ZEND_HASH_MAP_FOREACH_PTR(CG(auto_globals), auto_global) {
        auto_global->armed =
            // jit here is bool
            // auto_global_callback is a function pointer, which returns bool
            auto_global->jit || auto_global->auto_global_callback;
    }
    ZEND_HASH_FOREACH_END();

    // build only the non-jit once, leave jit armed, but not built
    // because jit once will be set when script compiles in zend_is_auto_global
    // zend_compile.c:2887,7798,8063,8098
    // if (zend_is_auto_global(name)) {
    // 	return FAILURE;
    // }
    // ----------
    // if (zend_is_auto_global(name)) {
    // 	zend_error_noreturn(E_COMPILE_ERROR, "Cannot re-assign auto-global
    // variable %s", 		ZSTR_VAL(name));
    // }
    ZEND_HASH_MAP_FOREACH_PTR(CG(auto_globals), auto_global) {
        // $_ENV is left armed-but-unbuilt and skipped on purpose: the process
        // environment is constant across requests and reset_super_globals()
        // doesn't drop $_ENV from the symbol table, so the array built on first
        // use persists and stays correct without a per-request rebuild. (Its
        // own create_env callback - not $_SERVER's - would rebuild it if a
        // script forced it.)
        if (auto_global->name == _env) {
            continue;
        }
        if (auto_global->auto_global_callback) {
            auto_global->armed =
                // static bool php_auto_globals_create_get(zend_string *name)
                // for example, how this works
                // _GET is a pointer to php_auto_globals_create_get
                // (php_variables.c: 799)
                // static bool php_auto_globals_create_get(zend_string *name)
                // {
                //     if (PG(variables_order) &&
                //     (strchr(PG(variables_order),'G') || ...)) {
                //         sapi_module.treat_data(PARSE_GET, NULL, NULL);
                //     } else {
                //         zval_ptr_dtor_nogc(&PG(http_globals)[TRACK_VARS_GET]);
                //         array_init(&PG(http_globals)[TRACK_VARS_GET]);
                //     }
                //     zend_hash_update(&EG(symbol_table),
                //     &PG(http_globals)[TRACK_VARS_GET]);
                //     Z_ADDREF(PG(http_globals)[TRACK_VARS_GET]); // <- ref
                //     counter
                //    return false; <-- means - don't rearm
                // }
                auto_global->auto_global_callback(auto_global->name);
        }
    }
    ZEND_HASH_FOREACH_END();
}
// handle php sessions
#ifdef HAVE_PHP_SESSION
// Worker mode bypasses module RSHUTDOWN, so the session module never flushes/
// closes an active session between requests - do the work session_rshutdown
// would have done, or PS(id)/session_status leak across requests (one request
// reusing the previous one's session).
//
// Nothing here is guarded on purpose. A bailing save handler is a fatal: it
// must reach rapira_request_teardown's zend_catch and recycle the worker, so
// php_request_shutdown runs the module's own RSHUTDOWN and closes the observer
// frames the longjmp abandoned. Swallowing it here and then calling more PHP
// (s_close) over those frames is what corrupts EG(current_observed_frame).
static void rapira_reset_session(void) {
    if (PS(session_status) == php_session_active) {
        php_session_flush(1); // write + close the active session
    }
    if (!Z_ISUNDEF(PS(http_session_vars))) {
        zval_ptr_dtor(&PS(http_session_vars));
        ZVAL_UNDEF(&PS(http_session_vars));
    }
    if (PS(mod_data) || PS(mod_user_implemented)) {
        PS(mod)->s_close(&PS(mod_data));
    }
    if (PS(id)) {
        zend_string_release_ex(PS(id), 0);
        PS(id) = NULL;
    }
    if (PS(session_vars)) {
        zend_string_release_ex(PS(session_vars), 0);
        PS(session_vars) = NULL;
    }
    if (PS(session_started_filename)) {
        zend_string_release(PS(session_started_filename));
        PS(session_started_filename) = NULL;
        PS(session_started_lineno) = 0;
    }
    PS(session_status) = php_session_none;
    // Transient guards that php_rinit_session_globals() scrubs at each request
    // start. Worker mode skips RINIT, so a userland save handler that bails
    // mid-call (or uses partial parent:: delegation) can leave them set. Cheap
    // defensive parity:
    PS(mod_user_is_open) = 0;
    PS(in_save_handler) = 0;
    PS(set_handler) = 0;
}
#else
static void rapira_reset_session(void) {}
#endif

static void rapira_reset_super_global(void) {
    zval *files = &PG(http_globals)[TRACK_VARS_FILES];
    zval_ptr_dtor(files);
    ZVAL_UNDEF(files);
    zend_hash_str_del(&EG(symbol_table), "_SESSION", sizeof("_SESSION") - 1);
}
// runs per-request PHP work under a zend bailout guard
// we need to catch here zend_bailout(), but exit/die in php 8.4+ are not
// bailouts:
/*
// php-src Zend/zend_exceptions.c:1061
ZEND_API ZEND_COLD void zend_throw_unwind_exit(void) {
    ZEND_ASSERT(!EG(exception));
    EG(exception) = zend_create_unwind_exit();
    EG(opline_before_exception) = EG(current_execute_data)->opline;
    EG(current_execute_data)->opline = EG(exception_op);
}

exit calls zend_buildin_functions.c:142, and
1. zend_call_function returns normally with EG(exception) set
2. zend_throw_unwind_exit asserts that EG(exception)
*/

int rapira_run_handler(zend_fcall_info *fci, zend_fcall_info_cache *fcc) {
    int outcome = OK;
    zval retval;
    ZVAL_UNDEF(&retval);
    fci->size = sizeof *fci;
    fci->retval = &retval;
    fci->param_count = 0;
    fci->named_params = NULL;

    // _zend_bailout is CG(unclean_shutdown)'s only request-time setter
    // (zend.c:1264) and init_compiler at cycle startup its only reset
    // (zend_compile.c:461) - a 0->1 flip during this run proves a bailout.
    // Read BEFORE the handler/shutdown-fn/destructor calls: a swallowed bailout
    // has already set it by the time control reaches the tail.
    bool clean_at_entry = !CG(unclean_shutdown);

    zend_execute_data *observed_base = EG(current_observed_frame);
    zend_try {
        zend_call_function(fci, fcc);
        zval_ptr_dtor(&retval);
    }
    zend_catch {
        outcome = BAILOUT;
        rapira_observer_end_to(
            observed_base); // close any frames the bailout skipped
    }
    zend_end_try();

    zend_try {
        // exit()/die() and uncaught exceptions land in EG(exception), not
        // bailouts
        if (EG(exception)) {
            if (zend_is_unwind_exit(EG(exception)) ||
                zend_is_graceful_exit(EG(exception))) {
                outcome = EXIT;
                zend_clear_exception();
            } else {
                // give set_exception_handler a chance
                // we use zend_try to protect from the bailout on the exception
                // in the exception handler
                zend_try_exception_handler();
                if (EG(exception)) {
                    outcome = THROW;
                    // Throwable path is E_DONT_BAIL and releases the object
                    // itself
                    zend_exception_error(EG(exception), E_ERROR);
                }
            }
        }
    }
    zend_catch {
        outcome = BAILOUT;
        rapira_observer_end_to(
            observed_base); // close any frames the bailout skipped
    }
    zend_end_try();

    zend_try { php_call_shutdown_functions(); }
    zend_catch {
        rapira_observer_end_to(
            observed_base); // close any frames the bailout skipped
    }
    zend_end_try();

    zend_try { zend_call_destructors(); }
    zend_catch {
        rapira_observer_end_to(
            observed_base); // close any frames the bailout skipped
    }
    zend_end_try();

    if (EG(exception)) {
        zend_clear_exception();
    }

    php_free_shutdown_functions();
    gc_protect(0); // reset gc_protect to 0, in case _zend_bailout left it at 1

    if (outcome != BAILOUT && clean_at_entry && CG(unclean_shutdown)) {
        outcome = BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    return outcome;
}

int rapira_request_activate(void) {
    int outcome = OK;
    zend_try {
        php_output_activate();
        sapi_activate();
        rapira_modules_rinit();
        rapira_request_init();
        rapira_reset_super_global();
        rapira_activate_auto_globals();
    }
    zend_catch { outcome = BAILOUT; }
    zend_end_try();

    if (outcome == BAILOUT) {
        gc_protect(0);
    }

    return outcome;
}

// per-request sapi teardown, returns 1 if any of the methods bailed out,
// 0 otherwise.
// main/main.c:1985,2002,2031 (source)
int rapira_request_teardown(void) {
    int bailed = OK;
    // a bailout here abandons the observer frames of everything it longjmps
    // over. php_request_shutdown's zend_observer_fcall_end_all() only reaches
    // them while the VM stack still holds them, and the PHP worker loop pops
    // that stack the moment rapira_handle_request returns - close them here,
    // where they're intact
    zend_execute_data *observed_base = EG(current_observed_frame);

    zend_try { php_output_end_all(); }
    zend_catch {
        bailed = BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    zend_end_try();

    zend_try { rapira_modules_rshutdown(); }
    zend_catch {
        bailed = BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    zend_end_try();

    zend_try { rapira_reset_session(); }
    zend_catch {
        bailed = BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    zend_end_try();

    zend_try { php_output_deactivate(); }
    zend_catch {
        bailed = BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    zend_end_try();

    zend_try { sapi_deactivate(); }
    zend_catch {
        bailed = BAILOUT;
        rapira_observer_end_to(observed_base);
    }
    zend_end_try();

#if defined(ZEND_MAX_EXECUTION_TIMERS) || !defined(ZTS)
    zend_try { zend_unset_timeout(); }
    zend_end_try();
#endif

    // _zend_bailout leaves gc_protect(1); reset unconditionally or the next
    // request runs with cycle GC disabled
    gc_protect(0);

    SG(request_info).request_method = NULL;
    SG(request_info).query_string = NULL;
    SG(request_info).request_uri = NULL;
    SG(request_info).path_translated = NULL;
    SG(request_info).content_type = NULL;
    SG(request_info).cookie_data = NULL;
    SG(request_info).current_user = NULL;
    SG(request_info).content_type_dup = NULL;

    return bailed;
}

// clears the last error
// main.c: 2099
// static void core_globals_dtor(php_core_globals *core_globals)
// {
// 	/* These should have been freed earlier. */
// 	ZEND_ASSERT(!core_globals->last_error_message); <---- will fire if not
// consumed
void rapira_clear_last_error(void) {
    if (PG(last_error_message)) {
        PG(last_error_type) = 0;
        PG(last_error_lineno) = 0;
        zend_string_release(PG(last_error_message));
        PG(last_error_message) = NULL;

        if (PG(last_error_file)) {
            zend_string_release(PG(last_error_file));
            PG(last_error_file) = NULL;
        }
    }
#if PHP_VERSION_ID >= 80500
    // shutdown_executor frees this every request; worker mode skips it, so the
    // captured trace (frame args) pins request objects across jobs. 8.5-only+
    // field.
    zend_try {
        zval_ptr_dtor(&EG(last_fatal_error_backtrace));
        ZVAL_UNDEF(&EG(last_fatal_error_backtrace));
    }
    zend_catch { ZVAL_UNDEF(&EG(last_fatal_error_backtrace)); }
    zend_end_try();
#endif
}

// once per process, before sapi_startup and after tsrm startup (on ZTS builds)
void rapira_process_init(void) {
#if defined(SIGPIPE) && defined(SIG_IGN)
    signal(SIGPIPE, SIG_IGN);
#endif
    zend_signal_startup();
}

/* Temp streams (POST request_body) are only NULLed by sapi_deactivate_module();
  nothing reclaims the resource in a resident request, so sweep dead ones
  before serving the next job. Safe here: the previous request is finished. */
void rapira_release_temporary_streams(void) {
    zend_resource *val;
    int stream_type = php_file_le_stream();
    ZEND_HASH_FOREACH_PTR(&EG(regular_list), val) {
        if (val->type == stream_type) {
            php_stream *stream = (php_stream *)val->ptr;
            if (stream != NULL && stream->ops == &php_stream_temp_ops &&
                stream->__exposed == 0 && GC_REFCOUNT(val) == 1) {
                zend_list_delete(val);
            }
        }
    }
    ZEND_HASH_FOREACH_END();
}

// Full teardown for the worker restart loop. The only unguarded step inside
// php_request_shutdown is zend_observer_fcall_end_all (main.c:1971); it NULLs
// EG(current_observed_frame) before walking (zend_observer.c:322), so on a
// bailout the retry skips it and finishes the remaining teardown steps.
int rapira_request_shutdown(void) {
    volatile int bailed = OK;
#ifdef HAVE_PHP_SESSION
    // a bailout inside a user save handler skips the cleanup that clears
    // PS(in_save_handler); the module RSHUTDOWN's recursion guard then refuses
    // to run the handler's close() and whatever it holds leaks (mod_user.c:29)
    PS(in_save_handler) = 0;
#endif
    zend_try { php_request_shutdown((void *)0); }
    zend_catch {
        bailed = BAILOUT;
        zend_try { php_request_shutdown((void *)0); }
        zend_end_try();
    }
    zend_end_try();
    return bailed;
}
