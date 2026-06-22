<?php
header('Content-Type: text/plain');
header('X-Powered-By: Rapira');
echo "Hello, " . ($_GET['name'] ?? 'anonymous') . "!\n";
echo "Method: {$_SERVER['REQUEST_METHOD']}\n";