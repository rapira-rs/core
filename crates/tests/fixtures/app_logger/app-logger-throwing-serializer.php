<?php

// An exception thrown inside a context value's jsonSerialize() must not escape log() or kill the script.
final class Bomb implements \JsonSerializable
{
	public function jsonSerialize(): mixed
	{
		throw new \RuntimeException('serializer bomb');
	}
}

try {
	\Rapira\log('bombed', \Rapira\LogLevel::Error, ['keep' => 'visible', 'bomb' => new Bomb()]);
	echo 'logged';
} catch (\Throwable $e) {
	echo 'escaped:', $e->getMessage();
}
