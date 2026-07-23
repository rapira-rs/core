<?php
use Rapira\Plugin\Http\HttpHandlerConfig;
use function Rapira\create_plugin_handler;

$http = create_plugin_handler(new HttpHandlerConfig());

$handler = static function () use ($http): void {
	header('Content-Type: text/plain');
	if (($_GET['boom'] ?? '') === '1') {
		throw new \RuntimeException('boom');
	}
	$info = $http->getInfo();
	echo "plugin={$http->config->info->name}",
		" state={$info->state}",
		" handled={$info->handled}\n";
};

while ($http->handleRequest($handler)) {
}
