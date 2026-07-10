<?php
// Classic-mode front controller for the extension exec tests: each exec runs this
// script fresh, with the request URI in $_GET, and echoes "ok:<from>".
echo 'ok:' . ($_GET['from'] ?? '?');
