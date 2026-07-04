<?php
header('Foo: bar');
header('Foo2: bar2');
header('Invalid');
header('I: ' . ($_GET['i'] ?? ''));
http_response_code(200 + (int)($_GET['i'] ?? 0));
echo implode("\n", headers_list());
