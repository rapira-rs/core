<?php
$handler = static function (): void {
    if (($_GET['probe'] ?? '') === '1') {
        echo 'ok';
        return;
    }
    \Rapira\log('held');
    usleep(300000);
    echo 'payload';
};
while (\Rapira\handle_request($handler)) {
}
