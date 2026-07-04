<?php
$handler = static function (): void {
	header('Foo: bar');
	header('Foo2: bar2');
	header('Foo3:bar3');
	header('Invalid');
	header('I: ' . ($_GET['i'] ?? 'i not set'));
	http_response_code(201);
	echo 'Hello';
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
