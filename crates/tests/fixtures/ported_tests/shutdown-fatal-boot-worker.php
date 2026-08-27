<?php
// A fatal inside a boot shutdown function must leave the worker's exit path clean.
register_shutdown_function(static function (): void {
    trigger_error('boot shutdown bomb', E_USER_ERROR); // absorbed by php_call_shutdown_functions' zend_try
});
$handler = static function (): void {
    echo 'ok';
};
while (\Rapira\handle_request($handler)) {
}
