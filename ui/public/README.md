# `ui/public/` — static assets

Put **all binary / non-JS assets** here: SVG, PNG, JPG, WOFF2, …

Vite serves the contents of this folder **as-is** at the webview root,
with the correct Content-Type header (e.g. `image/svg+xml`). Reference
an asset with a plain string path, e.g.

```js
const logoUrl = "/logo.svg";
```

```html
<img :src="logoUrl" alt="mesh-chat" />
```

## Why not `src/assets/` + `import`?

Vite's "import asset" mode (`import logo from "./assets/logo.svg"`, with
or without the `?url` suffix) serves the asset through the ES-module
loader. The **WebKit webview** that Tauri uses on Linux (`webkit2gtk`)
enforces strict MIME checking for module imports and rejects anything
that is not `application/javascript` or `text/javascript`. The typical
failure looks like:

    TypeError: 'image/svg+xml' is not a valid JavaScript MIME type.

…and the whole Vue app fails to mount, producing a blank window.

Using `public/` avoids the problem entirely because the file is fetched
as a regular resource (via `<img src="...">` or `fetch()`), not through
the module loader.

**If you must import an asset as a module** (e.g. embedded bytes via
`?raw`), add an explicit test that it still loads under WebKit before
merging.
