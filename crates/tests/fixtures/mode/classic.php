<?php

$mode = \Rapira\get_mode();
echo $mode->name, ':', $mode === \Rapira\Mode::Classic ? 'case' : 'copy', ':done';
