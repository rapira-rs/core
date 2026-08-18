<?php
// never pulls a job: must classify as a boot failure, not a servable worker.
\Rapira\log('booted');
