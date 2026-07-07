<?php
$handler = static function (): void {
	header('Content-Type: text/plain');
	echo 'ok';
};
while (\rapira_handle_request($handler)) {
}
trigger_error('worker exiting', E_USER_WARNING); // lands in PG(last_error_message) after the loop