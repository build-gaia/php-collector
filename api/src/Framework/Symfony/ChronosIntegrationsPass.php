<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony;

use Symfony\Component\DependencyInjection\Compiler\CompilerPassInterface;
use Symfony\Component\DependencyInjection\ContainerBuilder;
use Symfony\Component\DependencyInjection\Reference;

/**
 * Auto-registers the opt-in bridges a Symfony application would otherwise wire
 * by hand: the Messenger telemetry middleware on every message bus, the
 * HttpClient decorator on `http_client`, the Doctrine DBAL middleware via the
 * `doctrine.middleware` tag, and the Monolog handler.
 *
 * Every block is guarded on the third-party interface actually existing —
 * these packages are NOT dependencies (composer.json's zero-runtime-deps
 * constraint is absolute, they only appear under `suggest`), so an application
 * without, say, symfony/messenger must compile and boot with the corresponding
 * block silently skipped.
 *
 * ORDERING IS LOAD-BEARING: ChronosBundle registers this pass at
 * TYPE_BEFORE_OPTIMIZATION priority 1 (default passes run at 0) because two of
 * the blocks write inputs that other bundles' own priority-0 passes consume in
 * the same phase:
 *   - FrameworkBundle's MessengerPass reads each `messenger.bus.<name>.middleware`
 *     parameter exactly once to build the bus — a middleware prepended after
 *     that read is never seen;
 *   - DoctrineBundle's middleware pass collects `doctrine.middleware` tags — a
 *     service tagged after the collection is never attached to a connection.
 * Running one priority notch earlier keeps this pass ahead of both without
 * depending on bundle registration order in the kernel. The extension-config
 * merge pass (where framework/doctrine/monolog config becomes definitions and
 * parameters) always runs before ALL before-optimization passes, so everything
 * probed for below already exists by then when it is configured at all.
 */
final class ChronosIntegrationsPass implements CompilerPassInterface
{
    public function process(ContainerBuilder $container): void
    {
        $this->wireMessenger($container);
        $this->wireHttpClient($container);
        $this->wireDoctrine($container);
        $this->wireMonolog($container);
    }

    /**
     * Prepend the Chronos middleware onto every configured message bus, so it
     * wraps the whole chain: on dispatch the traceparent stamp must be on the
     * envelope before the send_message middleware (always last) serializes it,
     * and on consume the job-scoped request must open before any handler work.
     */
    private function wireMessenger(ContainerBuilder $container): void
    {
        if (!interface_exists(\Symfony\Component\Messenger\Middleware\MiddlewareInterface::class)) {
            return;
        }
        $middlewareId = \Chronos\Collector\Framework\Messenger\ChronosMiddleware::class;
        $registered = false;
        foreach ($container->findTaggedServiceIds('messenger.bus') as $busId => $tags) {
            // FrameworkExtension stores each bus's middleware chain as a plain
            // parameter that MessengerPass later consumes; editing the parameter
            // (rather than the built bus definition) is the one seam that works
            // for every Symfony version this SDK supports.
            $parameter = $busId.'.middleware';
            if (!$container->hasParameter($parameter)) {
                continue;
            }
            $middleware = $container->getParameter($parameter);
            if (!is_array($middleware)) {
                continue;
            }
            foreach ($middleware as $entry) {
                if (is_array($entry) && ($entry['id'] ?? null) === $middlewareId) {
                    // The application wired it explicitly in framework.yaml —
                    // never double-register, a second copy would double spans.
                    continue 2;
                }
            }
            if (!$registered) {
                $container->register($middlewareId, $middlewareId);
                $registered = true;
            }
            array_unshift($middleware, ['id' => $middlewareId]);
            $container->setParameter($parameter, $middleware);
        }
    }

    /**
     * Decorate `http_client` the same way ChronosBundle::build decorates
     * `http_kernel` — but from a pass, not build(): http_client only exists
     * after FrameworkExtension ran (and only when framework.http_client is
     * enabled at all), and decorating a service that never gets defined is a
     * compile error rather than a no-op.
     */
    private function wireHttpClient(ContainerBuilder $container): void
    {
        if (!interface_exists(\Symfony\Contracts\HttpClient\HttpClientInterface::class)) {
            return;
        }
        if (!$container->hasDefinition('http_client') && !$container->hasAlias('http_client')) {
            return;
        }
        $decoratorId = \Chronos\Collector\Framework\HttpClient\ChronosHttpClient::class;
        if ($container->hasDefinition($decoratorId)) {
            return; // application already wired it by hand
        }
        $container
            ->register($decoratorId, $decoratorId)
            ->setDecoratedService('http_client')
            ->setArguments([new Reference($decoratorId.'.inner')]);
    }

    /**
     * doctrine-bundle attaches any service tagged `doctrine.middleware` to every
     * configured DBAL connection, so the tag is the whole registration. Without
     * doctrine-bundle the tagged service is simply never referenced and the
     * container's remove-unused pass drops it — safe either way.
     */
    private function wireDoctrine(ContainerBuilder $container): void
    {
        if (!interface_exists(\Doctrine\DBAL\Driver\Middleware::class)) {
            return;
        }
        $middlewareId = \Chronos\Collector\Framework\Doctrine\ChronosMiddleware::class;
        if ($container->hasDefinition($middlewareId)) {
            return; // application already wired (and possibly configured) it
        }
        $container
            ->register($middlewareId, $middlewareId)
            ->addTag('doctrine.middleware');
    }

    /**
     * Push the Chronos handler onto the default Monolog channel. This pass runs
     * BEFORE MonologBundle's LoggerChannelPass (same priority-1 reasoning as
     * above), which clones `monolog.logger`'s method calls onto every extra
     * channel it creates — so one push here reaches all channels. The handler
     * is also registered under a stable id (`chronos.monolog_handler`) so an
     * application that wants per-channel control can reference it from
     * monolog.yaml as a `type: service` handler instead.
     */
    private function wireMonolog(ContainerBuilder $container): void
    {
        if (!class_exists(\Monolog\Logger::class)
            || !class_exists(\Monolog\Handler\AbstractProcessingHandler::class)) {
            return;
        }
        if (!$container->hasDefinition('monolog.logger')) {
            return;
        }
        if ($container->hasDefinition('chronos.monolog_handler')) {
            return; // application already wired it by hand
        }
        $container->register(
            'chronos.monolog_handler',
            \Chronos\Collector\Framework\Monolog\ChronosHandler::class,
        );
        $container
            ->getDefinition('monolog.logger')
            ->addMethodCall('pushHandler', [new Reference('chronos.monolog_handler')]);
    }
}
