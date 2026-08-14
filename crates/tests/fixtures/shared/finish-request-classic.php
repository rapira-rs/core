<?php
echo 'BEFORE';
rapira_finish_request();
echo 'AFTER';
\Rapira\log('post-finish-ran');
