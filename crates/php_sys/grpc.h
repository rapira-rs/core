#ifndef RAPIRA_GRPC_H
#define RAPIRA_GRPC_H

#include "wrapper.h"

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_handle_grpc_request, 0, 0,
                                        _IS_BOOL, 0)
ZEND_END_ARG_INFO()

ZEND_FUNCTION(rapira_handle_grpc_request);

#endif // RAPIRA_GRPC_H
