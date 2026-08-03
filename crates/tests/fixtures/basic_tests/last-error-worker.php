<?php
// A non-fatal warning does not bail, so no recycle masks the reset: proves
// rapira_clear_last_error() clears PG(last_error_message) between worker jobs.
$handler = static function (): void {
    if (($_GET['step'] ?? '') === 'warn') {
        trigger_error('leaky', E_USER_WARNING);
        echo 'warned';
        return;
    }
    $e = error_get_last();
    echo $e === null ? 'clean' : 'leaked:' . $e['message'];
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
