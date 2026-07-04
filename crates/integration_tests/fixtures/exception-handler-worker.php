<?php
set_exception_handler(static function (\Throwable $e): void {
	header('Content-Type: text/plain');
	echo "handled:", $e->getMessage();
});
$handler = static function (): void {
	throw new \RuntimeException('boom');
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
