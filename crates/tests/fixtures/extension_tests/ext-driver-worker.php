<?php
// Resident worker script: handles each request an extension drives via `exec`.
$handler = static function (): void {
	header('Content-Type: text/plain');
	echo 'ok:' . ($_GET['from'] ?? '?');
};
while (\Rapira\handle_request($handler)) {
}
