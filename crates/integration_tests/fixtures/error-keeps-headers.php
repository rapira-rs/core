<?php
ini_set('display_errors', '0');
session_start();            // queues Set-Cookie: PHPSESSID=...
http_response_code(404);
throw new \RuntimeException('boom');
