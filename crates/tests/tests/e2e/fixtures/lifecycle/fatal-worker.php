<?php
// Uncaught exception before the request loop: the worker fails to boot every time and the gen-0 pool dies unhealthy.
throw new RuntimeException('fatal-worker: intentional bootstrap failure');
