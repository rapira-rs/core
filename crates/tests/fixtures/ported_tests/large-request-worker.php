<?php
$handler = static function (): void {
	echo 'Request body size: ' . strlen(file_get_contents('php://input'));
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
