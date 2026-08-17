# Architecture map

`architecture-map.json` is the reviewed source of truth for the interactive
architecture map. `architecture-map.template.html` provides its self-contained
HTML, CSS, and browser interaction code. The generator combines both files into
one `index.html` file for GitHub Pages.

## Update the map

When a change affects a node's responsibility, source-file reference, route,
visible label, authority or capability boundary, boot lifecycle, or epoch
lifecycle, update `architecture-map.json` in the same change. An ordinary source
refactor does not require metadata changes when it preserves those facts. Do not
infer semantic architecture claims from Rust source or file names alone. Review
the referenced code before you update the metadata.

Use schema version `1`. Each node needs a unique `key`, finite `x`, `y`, `w`,
`h`, and `z` geometry values, non-empty text fields, and a `files` array. Every
`files` value must name an existing regular file inside this repository. Do not
use absolute paths, `..` traversal, directories, or symbolic links. Each route
must use existing `from` and `to` node keys and a `primary` or `secondary`
style.

The template must contain exactly one `__ARCHITECTURE_MAP_DATA__` marker inside
its script and exactly one `__ARCHITECTURE_MAP_REVISION__` marker in the page.
The template must render metadata with DOM APIs and `textContent`. Do not add
metadata values through `innerHTML`.

## Generate and validate

Run the generator against an empty, existing directory. The generator does not
create or clear the directory, and it rejects a directory that already contains
`index.html`.

```sh
map_output_dir="$(mktemp -d)"
node scripts/generate-architecture-map.mjs \
  --output "$map_output_dir" \
  --revision "$(git rev-parse HEAD)"
node --test scripts/generate-architecture-map.test.mjs
```

The CI workflow creates the fresh `site/` directory and publishes only its
generated `index.html` as the GitHub Pages artifact. Do not commit generated
HTML, Mermaid, SVG, PNG, or Excalidraw output for this deployment.

## Browser smoke test

Before merging a map change, open generated `index.html` and verify all of the
following:

- Pointer selection and `Enter` or `Space` keyboard selection update the detail
  panel.
- The detail panel displays metadata as text.
- Canvas drag pans the map.
- The mouse wheel and the `+` and `−` controls zoom the map.
- The reset control restores the default view.
- Sidebar buttons select their matching nodes.

After an administrator enables GitHub Pages, the deployed map is available at
[`https://wetware.github.io/ww/`](https://wetware.github.io/ww/).
