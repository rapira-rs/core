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
	// c-client rejects the empty host in mail_valid_net_parse (uw-imap mail.c) before
	// any DNS or socket work. The 1-second bound keeps a wrong assumption from
	// stalling the suite: imap_timeout() writes c-client directly, unlike ini_set.
	imap_timeout(IMAP_OPENTIMEOUT, 1);
	$open = @imap_open('{}INBOX', 'u', 'p');
	// imap_errors() drains the stack; RSHUTDOWN reports an undrained entry as an E_NOTICE.
	$errs = imap_errors();
	$rejected = is_array($errs) && str_contains(implode('|', $errs), 'invalid remote specification');
	echo 'imap:' . ($open === false && $rejected ? 'ok' : 'bad');
};
while (\Rapira\handle_request($handler)) {
}
