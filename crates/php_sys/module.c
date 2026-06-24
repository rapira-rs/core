#include "wrapper.h"

extern bool rapira_rs_handle_request(zend_fcall_info *fci,
                                     zend_fcall_info_cache *fcc);
extern void rapira_rs_finish_request(void);

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
    rapira_rs_finish_request();
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

// in rust with repr[c]
enum {
    OK = 0,
    BAILOUT = 1,
    EXIT = 2,
    THROW = 3,
};

int rapira_run_handler(zend_fcall_info *fci, zend_fcall_info_cache *fcc) {
    int outcome = OK;
    zval retval;
    ZVAL_UNDEF(&retval);
    fci->size = sizeof *fci;
    fci->retval = &retval;
    fci->param_count = 0;
    fci->named_params = NULL;

    zend_try {
        zend_call_function(fci, fcc);
        zval_ptr_dtor(&retval);
    }
    zend_catch { outcome = BAILOUT; }
    zend_end_try();

    // exit()/die() and uncaught exceptions land in EG(exception), not bailouts
    if (EG(exception)) {
        if (zend_is_unwind_exit(EG(exception)) ||
            zend_is_graceful_exit(EG(exception))) {
            outcome = EXIT;
            zend_clear_exception();
        } else {
            outcome = THROW;
            zend_try { zend_exception_error(EG(exception), E_ERROR); }
            zend_end_try();
        }
    }

    zend_try { php_call_shutdown_functions(); }
    zend_catch {}
    zend_end_try();

    zend_try { zend_call_destructors(); }
    zend_catch {}
    zend_end_try();

    if (EG(exception)) {
        zend_clear_exception();
    }

    php_free_shutdown_functions();
    gc_protect(0); // reset gc_protect to 0, in case _zend_bailout left it at 1
    return outcome;
}

// per-request sapi teardown, returns 1 if any of the methods bailed out,
// 0 otherwise.
// main/main.c:1985,2002,2031 (source)
int rapira_request_teardown(void) {
    int bailed = OK;

    zend_try { php_output_end_all(); }
    zend_catch { bailed = BAILOUT; }
    zend_end_try();

    zend_try { php_output_deactivate(); }
    zend_catch { bailed = BAILOUT; }
    zend_end_try();

    zend_try { sapi_deactivate(); }
    zend_catch { bailed = BAILOUT; }
    zend_end_try();

    if (bailed == BAILOUT) {
        // _zend_bailout left gc_protect(1), so all calls to gc_collect_cycles
        // becomes a permanent no-op
        // source: zend.c: 1263
        // if (!EG(bailout)) {
        //     zend_output_debug_string(
        //         1, "%s(%d) : Bailed out without a bailout address!",
        //         filename, lineno);
        //     exit(-1);
        // }
        // gc_protect(1); <-------------- boooooooom
        // CG(unclean_shutdown) = 1;
        // CG(active_class_entry) = NULL;
        // CG(in_compilation) = 0;
        // CG(memoize_mode) = 0;
        // EG(current_execute_data) = NULL;
        // LONGJMP(*EG(bailout), FAILURE);
        gc_protect(1);
    }

    return bailed;
}

// clears the last error
// main.c: 2099
// static void core_globals_dtor(php_core_globals *core_globals)
// {
// 	/* These should have been freed earlier. */
// 	ZEND_ASSERT(!core_globals->last_error_message); <---- will fire if not
// consumed

// 	ZEND_ASSERT(!core_globals->last_error_file);

// 	if (core_globals->php_binary) {
// 		free(core_globals->php_binary);
// 	}

// 	php_shutdown_ticks(core_globals);
// }

// basic_functions: 1449
/* {{{ Clear the last occurred error. */
// PHP_FUNCTION(error_clear_last)
// {
// 	ZEND_PARSE_PARAMETERS_NONE();

// 	if (PG(last_error_message)) {
// 		PG(last_error_type) = 0;
// 		PG(last_error_lineno) = 0;

// 		zend_string_release(PG(last_error_message));
// 		PG(last_error_message) = NULL;

// 		if (PG(last_error_file)) {
// 			zend_string_release(PG(last_error_file));
// 			PG(last_error_file) = NULL;
// 		}
// 	}

// 	zval_ptr_dtor(&EG(last_fatal_error_backtrace));
// 	ZVAL_UNDEF(&EG(last_fatal_error_backtrace));
// }
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
        // we leave $_ENV off, because it is not a real superglobal, and is
        // built by the auto_global_callback for $_SERVER, which is always built
        // (need to double check this)
        if (auto_global->name == _env) {
            // resule/explicit use will rebuild it, so we don't need to build it
            // here
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