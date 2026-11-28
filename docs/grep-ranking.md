# Check grep files-mode ranking

Run the ranker against a checkout of the pinned Cupel revision. The first line of
[`grep-ranking.tsv`](../crates/cupel-coding-agent/examples/grep-ranking.tsv) records the revision.
Each remaining line lists a literal query and its expected first result.

```sh
	git worktree add --detach ../cupel-ranking-fixture e4a38be3f7fd8fbbb07828ad2efa127c96adb8df
	cargo run --locked -p cupel-coding-agent --example grep-ranking -- ../cupel-ranking-fixture
	git worktree remove ../cupel-ranking-fixture
```

The program prints any mismatches and exits unsuccessfully if fewer than 24 of the 25 queries
return the expected file first.
