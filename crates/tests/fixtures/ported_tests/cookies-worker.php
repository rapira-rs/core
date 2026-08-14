<?php
$handler = static function (): void {
	var_export($_COOKIE);
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
