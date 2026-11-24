---
name: review-model-catalog
description: "Check models.dev for new or missing models on Cupel's providers, review which routes belong in the built-in catalog, and refresh the catalog when asked to apply changes."
---

# Review Cupel's model catalog

Treat "check", "review", and "look for new models" as read-only requests. Change the catalog only when the user asks you to apply or implement changes. Follow any provider, family, or date limits in the request.

## 1. Compare with the live inventory

1. Read `crates/cupel-coding-agent/src/bin/generate_catalog/curation.rs`. Use `PROVIDERS` to map Cupel providers to models.dev provider keys. Read the current rows and any scope rules in `crates/cupel-core/src/catalog.rs` and recent catalog history.
2. Fetch `https://models.dev/api.json` to a temporary location and validate the requested provider blocks before comparing. Use the full response. Both the checked-in `models_dev.json` and `generate-catalog --fetch` keep only curated rows and cannot discover additions. If the feed or a provider block is unavailable, report that part of the review as incomplete. Do not treat it as an empty inventory.
3. Compare exact provider keys and model IDs. Find uncurated routes, curated routes missing upstream, and changes to fields Cupel consumes. Use `models_dev.rs` to compare normalized metadata with the checked-in snapshot rather than diffing raw JSON. Call a route *uncurated* unless its release date or a previous full inventory establishes that it is *new*.
4. Review `OPENAI_CODEX_MODELS` separately against Codex's own model list or documentation. Do not infer subscription availability from OpenAI API listings. Treat `ULTRAFAST_BASES` as generated variants, not separate upstream models.

## 2. Decide what belongs

- **Add:** Confirm the exact route in the serving provider's documentation or model list. Explain what it adds to the current selection. Distinguish dated snapshots, `latest` aliases, regional routes, previews, and free or speed tiers. Do not assume an aggregator route has the same features, limits, or pricing as the direct API.
- **Remove:** Require evidence that the route retired, was replaced, or falls outside an established Cupel scope rule. An upstream omission, a newer sibling, or an older version number is not enough. Check every affected route independently. A direct API retirement does not imply an OpenRouter retirement.
- **Update metadata:** Check meaningful changes against provider sources. For additions and updates, verify the API and endpoint, request model ID, tool support, reasoning controls, supported inputs, context and output limits, and pricing, including cache and context tiers. Reuse a curation helper only if its behavior matches the route. Preserve intentional overrides unless evidence supports changing them.
- **Defer:** Keep the current catalog when sources disagree or scope is unclear. State what evidence or user decision would resolve the case.

Before recommending a removal or rename, search the repo for the exact IDs and any generated twin IDs. Check provider defaults, request mappings, fixtures, tests, and docs. The first row for a provider is its `/provider` default, and the first catalog row has a workspace fixture contract. Flag any intended default change. Name a verified replacement if users with saved model selections or explicit model IDs need to switch.

## 3. Apply approved changes

1. Edit `curation.rs`, not the generated JSON. Change pinned Codex rows or `ULTRAFAST_BASES` only when needed. Preserve existing row order unless the change calls for a different default. Keep catalog IDs unique across providers.
2. Regenerate `crates/cupel-coding-agent/src/bin/generate_catalog/models_dev.json` and `crates/cupel-core/src/catalog.json` together:

   ```sh
   cargo run -p cupel-coding-agent --bin generate-catalog -- --fetch
   ```

   If the generator fails on a curated route that is still in scope, report the blocker. Do not drop the route or patch either JSON file to make the run pass.
3. Review both generated diffs. Account for every added or removed row and every material metadata change. The fetch can refresh unrelated rows. Flag changes outside the request rather than treating them as approved.
4. Run the generator and core catalog tests on the final files. Run the affected provider tests if routing or request behavior changed.

   ```sh
   cargo test -p cupel-coding-agent --bin generate-catalog
   cargo test -p cupel-core catalog::tests
   ```

## 4. Report the result

Group results by provider. For each recommendation, give the exact ID, action, reason, and source link. For removals, include a verified replacement or say none was found. List unresolved decisions and providers you could not verify. Say whether you changed files and which checks passed, failed, or did not run. If nothing merits a change, say so without listing every uncurated route.
