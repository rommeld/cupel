# Palette themes

These JSON files replace the former Rust palette constants. `cupel` is the
only built-in theme and remains the default. The app reads palette files
from cupel home, not from this repository directory.

Install from the repository root without overwriting existing files:

```bash
mkdir -p "${CUPEL_HOME:-$HOME/.cupel}/themes"
cp -n themes/*.json "${CUPEL_HOME:-$HOME/.cupel}/themes/"
```

Run `/hot-reload`, then `/themes <id>`. See the
[custom theme documentation](../README.md#defining-a-custom-theme) for the format.

## Sources

- Ayu: `zed-industries/zed`, `assets/themes/ayu/ayu.json`.
- Nightfox family: `cange/nightfox.zed`, `themes/nvim-nightfox.json`.

Body text uses Zed's `text`; headings use `syntax.title`; bold and italic use
`syntax.emphasis`; links use `syntax.link_text`. Other accents use terminal
ANSI colors, except Ayu's answer color (`syntax.constant`): its terminal
magenta is blue, so the answer uses purple to stay distinct from tools.
