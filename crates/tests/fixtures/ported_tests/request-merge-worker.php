<?php
$handler = static function (): void {
	echo "REQUEST:";
	var_export($_REQUEST);
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
