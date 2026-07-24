/* This is a generated file, edit the .stub.php file instead.
 * Stub hash: 5f326c8163f7cd7846497fb1e2f3dd7f19254b7a */

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_finish_request, 0, 0, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_Rapira_create_plugin_handler, 0, 1, Rapira\\PluginHandler, 0)
	ZEND_ARG_OBJ_INFO(0, config, Rapira\\PluginHandlerConfig, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_PluginInfo___construct, 0, 0, 2)
	ZEND_ARG_TYPE_INFO(0, name, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, description, IS_STRING, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_PluginHandlerConfig___construct, 0, 0, 1)
	ZEND_ARG_OBJ_INFO(0, info, Rapira\\PluginInfo, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_INFO_EX(arginfo_class_Rapira_Plugin_Http_HttpHandlerConfig___construct, 0, 0, 0)
	ZEND_ARG_TYPE_INFO_WITH_DEFAULT_VALUE(0, pathPrefix, IS_STRING, 0, "\'\'")
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_class_Rapira_Plugin_Http_HttpHandler_handleRequest, 0, 1, _IS_BOOL, 0)
	ZEND_ARG_TYPE_INFO(0, handler, IS_CALLABLE, 0)
ZEND_END_ARG_INFO()

ZEND_BEGIN_ARG_WITH_RETURN_OBJ_INFO_EX(arginfo_class_Rapira_Plugin_Http_HttpHandler_getInfo, 0, 0, Rapira\\Plugin\\Http\\RuntimeInfo, 0)
ZEND_END_ARG_INFO()

ZEND_FUNCTION(rapira_finish_request);
ZEND_FUNCTION(Rapira_create_plugin_handler);
ZEND_METHOD(Rapira_PluginInfo, __construct);
ZEND_METHOD(Rapira_PluginHandlerConfig, __construct);
ZEND_METHOD(Rapira_Plugin_Http_HttpHandlerConfig, __construct);
ZEND_METHOD(Rapira_Plugin_Http_HttpHandler, handleRequest);
ZEND_METHOD(Rapira_Plugin_Http_HttpHandler, getInfo);

static const zend_function_entry ext_functions[] = {
	ZEND_FE(rapira_finish_request, arginfo_rapira_finish_request)
	ZEND_RAW_FENTRY(ZEND_NS_NAME("Rapira", "create_plugin_handler"), zif_Rapira_create_plugin_handler, arginfo_Rapira_create_plugin_handler, 0, NULL, NULL)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_PluginInfo_methods[] = {
	ZEND_ME(Rapira_PluginInfo, __construct, arginfo_class_Rapira_PluginInfo___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_PluginHandlerConfig_methods[] = {
	ZEND_ME(Rapira_PluginHandlerConfig, __construct, arginfo_class_Rapira_PluginHandlerConfig___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Plugin_Http_HttpHandlerConfig_methods[] = {
	ZEND_ME(Rapira_Plugin_Http_HttpHandlerConfig, __construct, arginfo_class_Rapira_Plugin_Http_HttpHandlerConfig___construct, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static const zend_function_entry class_Rapira_Plugin_Http_HttpHandler_methods[] = {
	ZEND_ME(Rapira_Plugin_Http_HttpHandler, handleRequest, arginfo_class_Rapira_Plugin_Http_HttpHandler_handleRequest, ZEND_ACC_PUBLIC)
	ZEND_ME(Rapira_Plugin_Http_HttpHandler, getInfo, arginfo_class_Rapira_Plugin_Http_HttpHandler_getInfo, ZEND_ACC_PUBLIC)
	ZEND_FE_END
};

static zend_class_entry *register_class_Rapira_PluginInfo(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "PluginInfo", class_Rapira_PluginInfo_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_READONLY_CLASS);

	zval property_name_default_value;
	ZVAL_UNDEF(&property_name_default_value);
	zend_declare_typed_property(class_entry, ZSTR_KNOWN(ZEND_STR_NAME), &property_name_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));

	zval property_description_default_value;
	ZVAL_UNDEF(&property_description_default_value);
	zend_string *property_description_name = zend_string_init("description", sizeof("description") - 1, 1);
	zend_declare_typed_property(class_entry, property_description_name, &property_description_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_description_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_PluginHandlerConfig(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "PluginHandlerConfig", class_Rapira_PluginHandlerConfig_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_ABSTRACT|ZEND_ACC_READONLY_CLASS);

	zval property_info_default_value;
	ZVAL_UNDEF(&property_info_default_value);
	zend_string *property_info_name = zend_string_init("info", sizeof("info") - 1, 1);
	zend_string *property_info_class_Rapira_PluginInfo = zend_string_init("Rapira\\PluginInfo", sizeof("Rapira\\PluginInfo")-1, 1);
	zend_declare_typed_property(class_entry, property_info_name, &property_info_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_info_class_Rapira_PluginInfo, 0, 0));
	zend_string_release(property_info_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_PluginHandler(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "PluginHandler", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_ABSTRACT);

	zval property_config_default_value;
	ZVAL_UNDEF(&property_config_default_value);
	zend_string *property_config_name = zend_string_init("config", sizeof("config") - 1, 1);
	zend_string *property_config_class_Rapira_PluginHandlerConfig = zend_string_init("Rapira\\PluginHandlerConfig", sizeof("Rapira\\PluginHandlerConfig")-1, 1);
	zend_declare_typed_property(class_entry, property_config_name, &property_config_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_CLASS(property_config_class_Rapira_PluginHandlerConfig, 0, 0));
	zend_string_release(property_config_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_RapiraException(zend_class_entry *class_entry_Exception)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira", "RapiraException", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Exception, 0);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Plugin_Http_HttpHandlerConfig(zend_class_entry *class_entry_Rapira_PluginHandlerConfig)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Plugin\\Http", "HttpHandlerConfig", class_Rapira_Plugin_Http_HttpHandlerConfig_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Rapira_PluginHandlerConfig, ZEND_ACC_FINAL|ZEND_ACC_READONLY_CLASS);

	zval property_pathPrefix_default_value;
	ZVAL_UNDEF(&property_pathPrefix_default_value);
	zend_string *property_pathPrefix_name = zend_string_init("pathPrefix", sizeof("pathPrefix") - 1, 1);
	zend_declare_typed_property(class_entry, property_pathPrefix_name, &property_pathPrefix_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_pathPrefix_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Plugin_Http_RuntimeInfo(void)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Plugin\\Http", "RuntimeInfo", NULL);
	class_entry = zend_register_internal_class_with_flags(&ce, NULL, ZEND_ACC_FINAL|ZEND_ACC_READONLY_CLASS);

	zval property_state_default_value;
	ZVAL_UNDEF(&property_state_default_value);
	zend_string *property_state_name = zend_string_init("state", sizeof("state") - 1, 1);
	zend_declare_typed_property(class_entry, property_state_name, &property_state_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_STRING));
	zend_string_release(property_state_name);

	zval property_pid_default_value;
	ZVAL_UNDEF(&property_pid_default_value);
	zend_string *property_pid_name = zend_string_init("pid", sizeof("pid") - 1, 1);
	zend_declare_typed_property(class_entry, property_pid_name, &property_pid_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_LONG));
	zend_string_release(property_pid_name);

	zval property_queued_default_value;
	ZVAL_UNDEF(&property_queued_default_value);
	zend_string *property_queued_name = zend_string_init("queued", sizeof("queued") - 1, 1);
	zend_declare_typed_property(class_entry, property_queued_name, &property_queued_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_LONG));
	zend_string_release(property_queued_name);

	zval property_handled_default_value;
	ZVAL_UNDEF(&property_handled_default_value);
	zend_string *property_handled_name = zend_string_init("handled", sizeof("handled") - 1, 1);
	zend_declare_typed_property(class_entry, property_handled_name, &property_handled_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_LONG));
	zend_string_release(property_handled_name);

	zval property_errors_default_value;
	ZVAL_UNDEF(&property_errors_default_value);
	zend_string *property_errors_name = zend_string_init("errors", sizeof("errors") - 1, 1);
	zend_declare_typed_property(class_entry, property_errors_name, &property_errors_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_LONG));
	zend_string_release(property_errors_name);

	zval property_recycles_default_value;
	ZVAL_UNDEF(&property_recycles_default_value);
	zend_string *property_recycles_name = zend_string_init("recycles", sizeof("recycles") - 1, 1);
	zend_declare_typed_property(class_entry, property_recycles_name, &property_recycles_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_LONG));
	zend_string_release(property_recycles_name);

	zval property_restarts_default_value;
	ZVAL_UNDEF(&property_restarts_default_value);
	zend_string *property_restarts_name = zend_string_init("restarts", sizeof("restarts") - 1, 1);
	zend_declare_typed_property(class_entry, property_restarts_name, &property_restarts_default_value, ZEND_ACC_PUBLIC|ZEND_ACC_READONLY, NULL, (zend_type) ZEND_TYPE_INIT_MASK(MAY_BE_LONG));
	zend_string_release(property_restarts_name);

	return class_entry;
}

static zend_class_entry *register_class_Rapira_Plugin_Http_HttpHandler(zend_class_entry *class_entry_Rapira_PluginHandler)
{
	zend_class_entry ce, *class_entry;

	INIT_NS_CLASS_ENTRY(ce, "Rapira\\Plugin\\Http", "HttpHandler", class_Rapira_Plugin_Http_HttpHandler_methods);
	class_entry = zend_register_internal_class_with_flags(&ce, class_entry_Rapira_PluginHandler, ZEND_ACC_FINAL);

	return class_entry;
}
