<?php
// MINIT copies default_socket_timeout into c-client once (PECL imap php_imap.c,
// SET_*TIMEOUT) and nothing reads the ini again, so a per-request ini_set does not
// reach c-client. Under the server, MINIT runs in the master before the fork.
// imap_timeout($type) with one argument reads the c-client value
// (https://www.php.net/manual/en/function.imap-timeout.php).
$handler = static function (): void {
	if (!extension_loaded('imap')) {
		echo 'skip';
		return;
	}
	$before = imap_timeout(IMAP_OPENTIMEOUT);
	ini_set('default_socket_timeout', '5');
	$after = imap_timeout(IMAP_OPENTIMEOUT);
	echo "imap:open={$before}:after_ini_set={$after}:read=" . imap_timeout(IMAP_READTIMEOUT)
		. ':ini=' . ini_get('default_socket_timeout');
};
while (\Rapira\handle_request($handler)) {
}
