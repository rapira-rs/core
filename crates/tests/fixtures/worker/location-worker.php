<?php
$handler = static function (): void {
    header('Location: /elsewhere');
};
while (\Rapira\handle_request($handler)) {
}
