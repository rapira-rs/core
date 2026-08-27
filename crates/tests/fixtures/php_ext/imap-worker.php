<?php
$handler = static function (): void {
	if (!extension_loaded('imap')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		imap_timeout('boom');
		return;
	}
	// c-client rejects the empty host in mail_valid_net_parse before any DNS or
	// socket work, so imap_open cannot stall on default_socket_timeout.
	// imap_errors() drains the stack; RSHUTDOWN reports an undrained entry as an E_NOTICE.
	$open = @imap_open('{}INBOX', 'u', 'p');
	$errs = imap_errors();
	echo 'imap:' . ($open === false && is_array($errs) && count($errs) === 1 ? 'ok' : 'bad');
};
while (\Rapira\handle_request($handler)) {
}
