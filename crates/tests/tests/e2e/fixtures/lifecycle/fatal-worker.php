<?php
// Bootstrap fatal: an uncaught exception before the request loop. PHP never
// reaches the request loop, so the worker fails to boot every time and the
// gen-0 pool dies unhealthy (master failboot path).
throw new RuntimeException('fatal-worker: intentional bootstrap failure');
