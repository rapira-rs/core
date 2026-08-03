<?php
$path = tempnam(sys_get_temp_dir(), 'rapira-fs');
file_put_contents($path, 'word1word2word3');
$fileStream = fopen($path, 'r');
$input = fopen('php://input', 'r');
$handler = static function () use ($fileStream, $input): void {
	echo fread($fileStream, 5);
	stream_is_local($input); // a dangling pre-loop handle would warn into the body
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
fclose($fileStream);
unlink($path);
