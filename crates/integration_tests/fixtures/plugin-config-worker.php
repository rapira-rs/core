<?php
use Rapira\Plugin\Http\HttpHandlerConfig;
use function Rapira\create_plugin_handler;

$http = create_plugin_handler(new HttpHandlerConfig(pathPrefix: '/api'));

$handler = static function () use ($http): void {
	header('Content-Type: text/plain');
	echo 'served:', $_SERVER['REQUEST_URI'], ' prefix=', $http->config->pathPrefix, "\n";
};

while ($http->handleRequest($handler)) {
}
