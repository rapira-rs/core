<?php

// exit() inside a serializer is an unwind-exit, not a serialization failure: log() must let it keep unwinding.
final class Quitter implements \JsonSerializable
{
	public function jsonSerialize(): mixed
	{
		echo 'quitting';
		exit;
	}
}

\Rapira\log('quit', \Rapira\LogLevel::Info, ['q' => new Quitter()]);
echo ' after-log';
