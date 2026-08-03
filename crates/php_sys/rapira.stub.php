<?php

/** @generate-class-entries */

namespace {
    /**
     * Classic mode only. Flush the response to the client early; the script may keep
     * working after it. Same contract as fastcgi_finish_request().
     */
    function rapira_finish_request(): bool {}
}

namespace Rapira {
    enum LogLevel
    {
        case Error;
        case Warning;
        case Info;
        case Debug;
        case Trace;
    }

    /**
     * A unit of work from a dispatcher. Host-created; the finalizing verbs live on the
     * concrete type.
     */
    interface Work
    {
        public function isFinalized(): bool;

        public function isCancelled(): bool;
    }

    /** Immutable counter snapshot. Observability only. */
    interface DispatcherInfo
    {
        public function pendingCount(): int;

        public function activeCount(): int;
    }

    /** The plugin surface this worker's pool serves. Plugins narrow every method. */
    interface Dispatcher
    {
        public function name(): string;

        /**
         * Never blocks. Null means nothing available right now.
         *
         * @throws Exception\ClosedException
         */
        public function tryReceive(): ?Work;

        /**
         * @param int $timeout Microseconds; -1 waits indefinitely, 0 does not wait at all.
         * @throws Exception\TimeoutException
         * @throws Exception\ClosedException
         */
        public function receive(int $timeout = -1): Work;

        public function getInfo(): DispatcherInfo;
    }

    /**
     * The same instance for the life of the process.
     *
     * @throws Exception\NotInWorkerModeError Called outside worker mode.
     */
    function get_dispatcher(): Dispatcher {}

    function get_version(): string {}

    /**
     * Queued to the host under the `app` target. Never blocks, never throws.
     * The `exception` key of $context is special: a \Throwable, serialized structurally.
     */
    function log(string $message, LogLevel $level = LogLevel::Info, array $context = []): void {}
}
