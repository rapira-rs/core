<?php

// One record per LogLevel case, plus one that omits the argument entirely: the
// stub's `= LogLevel::Info` default is reflection metadata, so C has to apply it.
\Rapira\log('lvl-error', \Rapira\LogLevel::Error);
\Rapira\log('lvl-warning', \Rapira\LogLevel::Warning);
\Rapira\log('lvl-info', \Rapira\LogLevel::Info);
\Rapira\log('lvl-debug', \Rapira\LogLevel::Debug);
\Rapira\log('lvl-trace', \Rapira\LogLevel::Trace);
\Rapira\log('lvl-omitted');

echo 'logged';
