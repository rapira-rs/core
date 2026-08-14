<?php
$handler = static function (): void {
	if (($_GET['use_request'] ?? '') === '1') {
		include __DIR__ . '/jit-request-include.php';
	} else {
		echo "SKIPPED";
	}
	echo "\nGET:";
	var_export($_GET);
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
