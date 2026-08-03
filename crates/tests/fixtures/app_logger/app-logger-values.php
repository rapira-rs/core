<?php

enum Suit: string
{
    case Hearts = 'H';
}

final class Money implements \JsonSerializable
{
    public function __construct(private int $cents) {}

    public function jsonSerialize(): array
    {
        return ['cents' => $this->cents];
    }
}

final class Account
{
    public string $id = 'acc_1';
    public ?string $note = null;
    protected string $hidden = 'protected';
    private string $secret = 'private';
}

\Rapira\log('values', \Rapira\LogLevel::Info, [
    'obj' => new Account(),
    'money' => new Money(1250),
    'suit' => Suit::Hearts,
    'nothing' => null,
    'list' => [1, 2, 3],
    'deep' => ['a' => ['b' => ['c' => 'bottom']]],
    'unicode' => 'héllo — ok',
    'zero' => 0,
]);

echo 'logged';
