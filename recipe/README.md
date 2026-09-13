# Symfony Flex recipe for `build-gaia/php-collector`

`manifest.json` in this directory is a Symfony Flex **contrib-recipe manifest**
— the same `{"bundles": {...}}` shape a recipe in
[`symfony/recipes-contrib`](https://github.com/symfony/recipes-contrib) uses to
register a bundle for every environment with no manual step. It registers
exactly one thing:

```json
{
    "bundles": {
        "Chronos\\Collector\\Framework\\Symfony\\ChronosBundle": ["all"]
    }
}
```

That is deliberately the whole recipe. `ChronosBundle::build()` already no-ops
completely when the native extension is not loaded (see its own docblock), so
there is nothing else for a recipe to configure — no `config/packages/chronos.yaml`
to copy, no environment variables to seed. Registering the bundle is the one
manual step this package cannot remove on its own, and this file is that step,
shaped so it is ready the moment the recipe reaches wherever your organisation's
Symfony Flex is configured to look.

## Why this alone does not make Flex "just work"

Flex resolves a recipe by asking an **endpoint** (by default
`https://symfony.com`, backed by the public `symfony/recipes` /
`symfony/recipes-contrib` index) whether it knows a recipe for the package
Composer just installed. `build-gaia/php-collector` is a private package: it is
not submitted to that public index, so Flex asks the public endpoint about a
package the public endpoint has never heard of, and gets back "no recipe" —
regardless of this file existing in the package itself. Flex does not scan an
installed package's own tree for a `recipe/` directory; a recipe only reaches
an application through an endpoint Flex is configured to query.

Two ways that changes, neither of them "invent an auto-registration hack":

- **A private Flex endpoint.** Flex supports additional endpoints via
  `extra.symfony.endpoint` in the *application's* `composer.json` (an array of
  URLs, checked in order alongside the default). If this organisation stands up
  a private recipe index (or a flat directory Flex can query in that shape) and
  publishes this manifest under `build-gaia/php-collector/<version>/manifest.json`
  there, every Flex-managed application pointed at that endpoint registers the
  bundle automatically the moment it runs `composer require`. This directory is
  the source for that submission, not a substitute for it.
- **A recipe pinned to this VCS repository.** Flex can also be pointed at a
  single package's recipe living in its own repository via
  `extra.symfony.endpoint` entries of the form
  `"github.com/build-gaia/php-collector:recipes"` (a branch/ref Flex reads
  `manifest.json` from directly, no index server required). That still requires
  the *consuming application* to add the endpoint entry once — it is a smaller
  step than hand-registering the bundle, but it is still a step, and is only
  worth it for an application already committed to Flex-managed config.

Until one of those is set up, this manifest documents the target shape and
costs nothing sitting here.

## The one-line fallback (works today, no Flex, no endpoint)

Add `ChronosBundle` to `config/bundles.php` yourself — this is the exact line a
Flex recipe would have added, so an application on Flex with the endpoint above
configured and an application doing it by hand end up with an identical
container:

```php
Chronos\Collector\Framework\Symfony\ChronosBundle::class => ['all' => true],
```

That is the entire manual step. `ChronosBundle::build()` is a no-op without the
native extension loaded (see its own docblock in
`api/src/Framework/Symfony/ChronosBundle.php`), so adding the line to an
application that has not yet dropped in `chronos.so` changes nothing — it only
starts wiring bridges once the extension shows up, matching the rest of this
package's zero-cost-until-configured contract.
