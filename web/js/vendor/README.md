# Browser dependencies

These files are embedded in the Rope binary. No CDN or npm installation is
required to build or run Rope.

- markdown-it 15.0.3: official browser ESM bundle, MIT license.
- DOMPurify 3.4.17: official ESM distribution, Apache-2.0 OR MPL-2.0.

Upstream source: https://github.com/markdown-it/markdown-it and
https://github.com/cure53/DOMPurify. License texts are included alongside the
modules. Only trailing source-map references are removed; source maps are not
shipped.

To update, change the exact versions in `web/package.json`, regenerate the npm
lockfile, then run from the repository root:

```sh
npm ci --prefix web --ignore-scripts
npm run --prefix web vendor
npm exec --prefix web -- playwright install chromium
npm test --prefix web
```

Commit the lockfile, generated modules and licenses together. Run `vendor`
again and check `git diff` to verify reproducibility.
