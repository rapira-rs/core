<?php
// Classic mode: the factory must refuse rather than hand back a handler whose
// loop can only ever report shutdown.
Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
