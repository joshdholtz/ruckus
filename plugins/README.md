# ruckus plugins (first-party)

Each folder is a ruckus plugin (a `ruckus-plugin.toml` adding `[[bind]]`
shortcuts and `[[link]]` handlers). They're managed in this repo.

**Use them (dev, edits are live):**

```sh
ruckus plugin link ./plugins   # links every plugin here
ruckus reload
```

**Or install one from the repo (monorepo subpath):**

```sh
ruckus plugin install joshdholtz/ruckus/plugins/github-links
```

| plugin | needs | does |
|---|---|---|
| github-links | gh | ctrl-click `#123` / a commit SHA → open on GitHub |
| pr-review | gh (or your reviewer) | ctrl-click a PR URL → review it in a split |
| gh-dash | `gh extension install dlvhdr/gh-dash` | `alt-g` → gh-dash in a split |
| scratch | — | ``alt-` `` → throwaway shell popup |
