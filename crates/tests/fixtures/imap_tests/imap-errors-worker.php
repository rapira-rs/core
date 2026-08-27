<?php
// RINIT resets imap_errorstack and RSHUTDOWN frees it (PECL imap php_imap.c); rapira reloads the module per job, so an error must not reach the next request.
$handler = static function (): void {
	if (!extension_loaded('imap')) {
		echo 'skip';
		return;
	}
	if (($_GET['step'] ?? '') === 'leak') {
		// the 1-second bound keeps a wrong lexical-rejection assumption from stalling the suite
		imap_timeout(IMAP_OPENTIMEOUT, 1);
		@imap_open('{}INBOX', 'u', 'p');
		// imap_last_error() does not clear the stack.
		echo 'imap:leaked:' . (is_string(imap_last_error()) ? '1' : '0');
		return;
	}
	$errs = imap_errors();
	echo 'imap:errors:' . ($errs === false ? 'empty' : implode('|', $errs));
};
while (\Rapira\handle_request($handler)) {
}
