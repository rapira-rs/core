<?php
function fib(int $n): int
{
    return $n < 2 ? $n : fib($n - 1) + fib($n - 2);
}

// 300 independent fibers, each suspending twice: every start/resume crosses the fiber/worker stack boundary, where PHP swaps EG(stack_base)/EG(stack_limit) in and back out
$sum = 0;
for ($i = 0; $i < 300; $i++) {
    $f = new Fiber(function (): int {
        $a = fib(14);                 // 377 - recursion on the fiber's own stack
        $b = Fiber::suspend($a);      // -> worker stack; resumes with 377
        $c = Fiber::suspend($b + 1);  // -> worker stack; resumes with 378
        return $a + $c;               // 755
    });
    $r1 = $f->start();                // 377  (first suspend value)
    $r2 = $f->resume($r1);            // 378  (second suspend value)
    $f->resume($r2);
    $sum += $f->getReturn();          // 755 each
}
// sum = 300 * 755 = 226500

// 25 nested fibers alive at once, so base/limit is saved 25 times on the way in and restored 25 times on the way out
function nest(int $depth): int
{
    if ($depth === 0) {
        return fib(12);               // 144
    }
    $inner = new Fiber(fn(): int => nest($depth - 1));
    $inner->start();                  // inner runs to completion (never suspends)
    return $inner->getReturn();
}
$sum += nest(25);                     // +144

// a stale stack base from any fiber boundary faults on this final compile/echo
echo "fibers ok sum=$sum\n";          // 226644