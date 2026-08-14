<?php
set_exception_handler(static function (\Throwable $e): void {
	trigger_error('handler bomb', E_USER_ERROR);
});
$handler = static function (): void {
	throw new \RuntimeException('boom');
};
while (\Rapira\handle_request($handler)) {
}
