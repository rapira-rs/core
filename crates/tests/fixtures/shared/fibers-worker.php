<?php

function fib(int $n): int
{
	return $n < 2 ? $n : fib($n - 1) + fib($n - 2);
}

function nest(int $depth): int
{
	if ($depth === 0) {
		return fib(12);
	}
	$inner = new Fiber(fn(): int => nest($depth - 1));
	$inner->start();                      // inner runs to completion, never suspends
	return $inner->getReturn();
}

$handler = static function (): void {
	$sum = 0;

	// 300 independent fibers, each suspending twice: every start/resume crosses the fiber<->worker stack boundary.
	for ($i = 0; $i < 300; $i++) {
		$f = new Fiber(function (): int {
			$a = fib(14);                 // recursion runs on the fiber's own stack
			$b = Fiber::suspend($a);
			$c = Fiber::suspend($b + 1);
			return $a + $c;
		});
		$r1 = $f->start();
		$r2 = $f->resume($r1);
		$f->resume($r2);
		$sum += $f->getReturn();
	}
	// sum = 300 * 755 = 226500

	// 25 nested fibers keep 25 fiber stacks live at once; adds 144.
	$sum += nest(25);

	header('Content-Type: text/plain');
	echo "fibers ok sum=$sum\n";
};

while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
