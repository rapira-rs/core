<?php
$handler = static function (): void {
	header('Content-Type: text/plain');
	echo 'ok';
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
trigger_error('worker exiting', E_USER_WARNING); // lands in PG(last_error_message) after the loop