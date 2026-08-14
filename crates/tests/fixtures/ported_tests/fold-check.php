<?php echo ($_COOKIE['a'] ?? '-'), ',', ($_COOKIE['b'] ?? '-'), ',', ($_SERVER['HTTP_COOKIE'] ?? '-'), ',', ($_SERVER['HTTP_X_FORWARDED_FOR'] ?? '-'), ',', ($_SERVER['HTTP_AUTHORIZATION'] ?? '-');
