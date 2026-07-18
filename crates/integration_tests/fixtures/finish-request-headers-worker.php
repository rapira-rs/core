<?php
$handler = static function (): void {
	header('Location: /done');
	http_response_code(302);
	\rapira_finish_request();
};
while (\rapira_handle_request($handler)) {
}
