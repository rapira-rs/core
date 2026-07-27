<?php
use Rapira\Plugin\Http\HttpHandler;
use Rapira\Plugin\Http\HttpHandlerConfig;
use Rapira\Plugin\Http\RuntimeInfo;
use Rapira\PluginHandlerConfig;
use Rapira\PluginInfo;
use function Rapira\create_plugin_handler;

$http = create_plugin_handler(new HttpHandlerConfig());

$handler = static function () use ($http): void {
	header('Content-Type: text/plain');
	$base = new \ReflectionClass(PluginHandlerConfig::class);
	$out = [
		'final=' . (int) (new \ReflectionClass(HttpHandler::class))->isFinal(),
		'abstract=' . (int) $base->isAbstract(),
		'readonly=' . (int) (new \ReflectionClass(HttpHandlerConfig::class))->isReadOnly(),
		'prop-readonly=' . (int) $base->getProperty('info')->isReadOnly(),
		'instanceof=' . (int) ($http instanceof \Rapira\PluginHandler),
	];
	foreach ([HttpHandler::class, RuntimeInfo::class, PluginInfo::class] as $blocked) {
		try {
			new $blocked();
			$out[] = 'ctor=allowed';
		} catch (\Error) {
			$out[] = 'ctor=blocked';
		}
	}
	try {
		$http->config->info->name = 'x';
		$out[] = 'write=allowed';
	} catch (\Error) {
		$out[] = 'write=blocked';
	}
	echo implode(' ', $out), "\n";
};

while ($http->handleRequest($handler)) {
}
