<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo "Hello from worker, " . ($_GET['name'] ?? 'anonymous') . "!\n";
};
while (\Rapira\handle_request($handler)) {
}
