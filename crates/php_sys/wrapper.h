#ifndef RAPIRA_WRAPPER_H
#define RAPIRA_WRAPPER_H

/* bindgen/libclang parse ONLY: build.rs defines RAPIRA_BINDGEN solely for the bindgen invocation.
 * The real compiler never sees it, so neither rewrite touches the shim's own codegen — gating on
 * __clang__ would also fire when cc falls back to clang-cl for the real build, silently changing
 * the shim's ABI (PHP_FUNCTIONs compiled cdecl while the engine calls them __vectorcall).
 * https://learn.microsoft.com/en-us/cpp/cpp/cdecl
 *
 * 1. zend_operators.h (PHP 8.5+) does overflow-checked math via intsafe.h's LongLongAdd/LongLongSub,
 *    which libclang doesn't declare -> implicit-declaration errors. Take PHP's __builtin_*_overflow
 *    path instead by defining the PHP_HAVE_BUILTIN_* macros the Windows config.w32.h omits.
 *    https://github.com/php/php-src/pull/17472
 *    https://learn.microsoft.com/en-us/windows/win32/api/intsafe/nf-intsafe-longlongadd
 *    https://learn.microsoft.com/en-us/windows/win32/api/intsafe/nf-intsafe-longlongsub
 *    https://gcc.gnu.org/onlinedocs/gcc/Integer-Overflow-Builtins.html
 * 2. ZEND_FASTCALL = __vectorcall under _MSC_VER makes zif_handler a __vectorcall function pointer.
 *    bindgen 0.72.1 on a stable rust target can't emit `extern "vectorcall"`; it drops the handler
 *    field, so _zend_function_entry / _zend_internal_function generate 8 bytes short and the
 *    layout-test const assert underflows (E0080: 40 - 48). Blank ZEND_FASTCALL for the parse so
 *    handler is a plain 8-byte extern "C" pointer - identical size, so layout_tests pass for real.
 *    cl.exe keeps the true __vectorcall for the C shim; nothing in Rust invokes a handler/fastcall.
 *    https://learn.microsoft.com/en-us/cpp/cpp/vectorcall */
#if defined(_WIN32) && defined(RAPIRA_BINDGEN)
#define PHP_HAVE_BUILTIN_SADDL_OVERFLOW 1
#define PHP_HAVE_BUILTIN_SADDLL_OVERFLOW 1
#define PHP_HAVE_BUILTIN_SSUBL_OVERFLOW 1
#define PHP_HAVE_BUILTIN_SSUBLL_OVERFLOW 1
#define PHP_HAVE_BUILTIN_SMULL_OVERFLOW 1
#define PHP_HAVE_BUILTIN_SMULLL_OVERFLOW 1
#include <Zend/zend_portability.h>
#undef ZEND_FASTCALL
#define ZEND_FASTCALL
#endif

// clang-format off
#include <TSRM/TSRM.h>
#include <Zend/zend.h>
#include <Zend/zend_API.h>
#include <Zend/zend_compile.h>
#include <Zend/zend_globals.h>
#include <Zend/zend_exceptions.h>
#include <main/php.h>
#include <ext/standard/basic_functions.h>
#include <main/SAPI.h>
#include <main/php_main.h>
#include <main/php_output.h>
#include <main/php_variables.h>
// clang-format on
#ifdef HAVE_PHP_SESSION
#include <ext/session/php_session.h>
#endif
#include <Zend/zend_observer.h>
#include <ext/standard/head.h>
#include <main/php_memory_streams.h>
#include <main/php_streams.h>

#if defined(PHP_WIN32) && defined(ZTS)
ZEND_TSRMLS_CACHE_EXTERN()
#endif

sapi_globals_struct *rapira_sg(void);
php_core_globals *rapira_pg(void);
void rapira_init_call_stack(void);
void rapira_tsrmls_cache_update(void);
void rapira_process_init(void);
void rapira_release_temporary_streams(void);
int rapira_request_activate(void);
int rapira_request_shutdown(void);
size_t rapira_ub_write(const char *str, size_t len);
#endif