<?php

/** @generate-class-entries */

namespace {
    /**
     * Flush the response to the client early; the handler may keep working
     * after it. Same contract as fastcgi_finish_request().
     */
    function rapira_finish_request(): bool {}
}

namespace Rapira {
    /**
     * Identity of the plugin a handler config targets; the concrete config fills it in.
     */
    final readonly class PluginInfo
    {
        public string $name;
        public string $description;
    }

    /**
     * Base for every plugin handler config. The concrete subclass names the
     * plugin it targets.
     */
    abstract readonly class PluginHandlerConfig
    {
        public PluginInfo $info;
    }

    /**
     * Base for every plugin handler. Only $config is guaranteed; the per-plugin
     * API lives on the concrete handler. Obtain one from create_plugin_handler().
     */
    abstract class PluginHandler
    {
        public readonly PluginHandlerConfig $config;
    }

    class RapiraException extends \Exception {}

    /**
     * Create the handler for the plugin named by $config.
     *
     * @throws RapiraException outside worker mode, or when no plugin matches.
     */
    function create_plugin_handler(PluginHandlerConfig $config): PluginHandler {}
}

namespace Rapira\Plugin\Http {
    /** Declares that this worker serves HTTP. Pass it to create_plugin_handler(). */
    final readonly class HttpHandlerConfig extends \Rapira\PluginHandlerConfig
    {
        public function __construct() {}
    }

    /**
     * Live worker counters. Obtain one from HttpHandler::getInfo().
     */
    final readonly class RuntimeInfo
    {
        /** One of: starting, idle, active, draining, free. */
        public string $state;
        public int $pid;
        /** Depth of this worker's job intake right now, not a running total. */
        public int $queued;
        public int $handled;
        public int $errors;
        public int $recycles;
        public int $restarts;
    }

    final class HttpHandler extends \Rapira\PluginHandler
    {
        /**
         * Block until a request arrives, run $handler for it, and return true.
         * False means the server is shutting down: leave the loop.
         */
        public function handleRequest(callable $handler): bool {}

        /** This worker's live counters, read at call time. */
        public function getInfo(): RuntimeInfo {}
    }
}
