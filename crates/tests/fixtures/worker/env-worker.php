<?php
$_ENV['boot_mark'] = 'set-at-boot';
$handler = static function (): void {
    // a newly compiled file mentioning $_ENV is the wipe trigger
    require __DIR__ . '/late-env.php';
    echo $_ENV['boot_mark'] ?? 'lost';
};
while (\Rapira\handle_request($handler)) {
}
