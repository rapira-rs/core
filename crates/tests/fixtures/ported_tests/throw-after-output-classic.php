<?php
echo 'partial page';
flush();
throw new \Exception('boom');
