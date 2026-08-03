<?php
$f = $_FILES['file'] ?? null;
if ($f === null) {
	echo 'NO FILE';
	return;
}
echo $f['name'], '|', $f['error'], '|', file_get_contents($f['tmp_name']), '|', $f['tmp_name'];
