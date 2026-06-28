<?php
$handler = static function (): void {
	if (($_GET['boom'] ?? '') === '1') {
		throw new \RuntimeException("scoreboard error");
	}
	echo "ok";
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
