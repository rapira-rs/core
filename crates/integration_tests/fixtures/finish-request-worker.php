<?php
class State
{
    public static int $n = 0;
}
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo "count=" . State::$n . " BEFORE";
    \rapira_finish_request();   // php_output_end_all() + Context::finish(): flush, then close the stream
    echo " AFTER";             // stream already closed -> dropped, never reaches the client
    State::$n++;               // post-response work still executes; the next request observes it
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
    gc_collect_cycles();
}
