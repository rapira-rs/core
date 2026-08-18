<?php
// Classic-mode front controller for the extension exec tests: each exec runs this script fresh.
echo 'ok:' . ($_GET['from'] ?? '?');
