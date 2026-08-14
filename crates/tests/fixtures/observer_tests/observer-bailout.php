<?php
function inner(): void
{
	if (($_GET['mode'] ?? '') === 'fatal') {
		trigger_error('boom', E_USER_ERROR);
	}
}
function outer(): void
{
	inner();
}
$handler = static function (): void {
	if (!extension_loaded('zend_test')) {
		echo 'skip';
		return;
	}
	outer();
	echo 'ok';
};
while (\Rapira\handle_request($handler)) {
}
