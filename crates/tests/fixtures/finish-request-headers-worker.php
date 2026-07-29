<?php
$handler = static function (): void {
	header('Location: /done');
	http_response_code(302);
	\rapira_finish_request();
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
