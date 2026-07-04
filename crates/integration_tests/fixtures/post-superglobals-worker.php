<?php
$handler = static function (): void {
	var_export($_GET);
	var_export($_POST);
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
