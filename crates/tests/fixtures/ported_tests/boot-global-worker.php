<?php
// A refcount-1 object in a bare boot global stays in the symbol table across jobs. Its destructor runs once, at cycle end.
class Kernel
{
    public int $calls = 0;

    public function tick(): int
    {
        return ++$this->calls;
    }

    public function __destruct()
    {
        \Rapira\log('boot-kernel destructed');
    }
}

$kernel = new Kernel();

$handler = static function (): void {
    if (!isset($GLOBALS['kernel'])) {
        echo 'kernel=gone';
        return;
    }
    echo 'kernel=ok calls=', $GLOBALS['kernel']->tick();
};
while (\Rapira\handle_request($handler)) {
}
