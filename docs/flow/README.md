# Flow diagrams

Each diagram is written in Mermaid (`*.mmd`), turned into an Excalidraw scene
(`*.excalidraw`, editable at excalidraw.com) and rendered to SVG for light and
dark themes with the [Excalidraw CLI](https://github.com/excalidraw/cli)
(Node.js 22 or newer; it renders locally with an installed browser):

```powershell
excalidraw mermaid media.mmd --out media.excalidraw
excalidraw render media.excalidraw --out media-light.svg --theme light --transparent
excalidraw render media.excalidraw --out media-dark.svg --theme dark --transparent
```

The `-pt` files are the same diagrams in Portuguese, for `README-pt-BR.md`
(`media-pt.mmd` → `media-pt.excalidraw` → `media-pt-light.svg` and
`media-pt-dark.svg`, and so on).
