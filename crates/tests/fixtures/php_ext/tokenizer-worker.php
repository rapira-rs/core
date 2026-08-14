<?php
$handler = static function (): void {
	if (!extension_loaded('tokenizer')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		token_get_all('<?php $x = ;', TOKEN_PARSE);
		return;
	}
	echo 'tok:' . count(token_get_all('<?php echo 1;'));
};
while (\Rapira\handle_request($handler)) {
}
