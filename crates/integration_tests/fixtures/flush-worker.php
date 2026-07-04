<?php
$handler = static function (): void {
	echo 'He';
	flush();
	echo 'llo ' . ($_GET['i'] ?? '');
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
