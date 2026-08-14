<?php
$handler = static function (): void {
	ini_set('display_errors', '0'); // no error text in the body: nothing precedes the 500
	throw new \RuntimeException('quiet boom');
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
