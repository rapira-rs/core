<?php
echo "REQUEST_COUNT:" . count($_REQUEST);
echo "\nVAL_CHECK:" . ((($_GET['val'] ?? '') === ($_REQUEST['val'] ?? 'MISSING')) ? 'MATCH' : 'MISMATCH');
