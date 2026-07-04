<?php
$handler = static function (): void {
	var_export($_COOKIE);
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
