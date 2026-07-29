<?php echo ($_COOKIE['a'] ?? '-'), ',', ($_COOKIE['b'] ?? '-'), ',', ($_SERVER['HTTP_COOKIE'] ?? '-');
