# Design: Flarebox

The design rules for this site. Every page follows them. Change a rule here before a page breaks it.

## System
- Genre: modern-minimal
- Shape: one screen. A Stage hero, a floating bar nav with the wordmark and one key, and a footer with the trademark notice. Add a section only when the project needs it.
- Theme: Cobalt
- Axes: light/grotesk-sans/cool, posture: restrained
- Signature: the two install commands, each with a copy key, and `flarebox setup` running under them in a terminal panel. Each progress line is a step in the CLI setup design (docs/design/cli-setup.md). The workers.dev host is an example.

## Tokens
`tokens.css` is the source of truth. The roles:
```css
:root {
  --color-paper: oklch(98.6% 0.005 250);  --color-paper-2: oklch(96.2% 0.008 252);  --color-paper-3: oklch(93% 0.011 252);
  --color-ink: oklch(21% 0.028 262);  --color-ink-2: oklch(36% 0.022 260);  --color-muted: oklch(50% 0.018 258);  --color-rule: oklch(90% 0.009 252);
  --color-graphite: oklch(21% 0.024 262);  --color-graphite-2: oklch(26.5% 0.026 262);  --color-graphite-ink: oklch(93% 0.010 250);
  --color-accent: oklch(52% 0.19 258);  --color-accent-2: oklch(76% 0.13 254);  --color-accent-wash: oklch(95% 0.028 256);
  --color-accent-ink: oklch(98.6% 0.005 250);  --color-focus: oklch(52% 0.19 258);
  --font-display: "Familjen Grotesk", system-ui, sans-serif;  --font-body: "IBM Plex Sans", system-ui, sans-serif;  --font-outlier: "JetBrains Mono", ui-monospace, monospace;
  --radius-card: 8px;  --radius-input: 6px;  --rule-hair: 1px;  --shadow-card: 0 1px 2px oklch(21% 0.028 262 / 0.06);
  --cell-w: 8px;  --cell-h: 16px;  --cut: 8px;  --cut-l: 16px;
}
```
The spacing scale, the type scale, the easings and the durations are the scaffold's and live in `tokens.css`.

## Type
- The fonts are self-hosted in `fonts/` as variable WOFF2 files (Latin subset from Google Fonts), each with its SIL Open Font License beside it. The page loads nothing from another host.
- Heads: Familjen Grotesk 600, item titles 500, tracking -0.028em, sentence case. Body: IBM Plex Sans 400 and 500.
- JetBrains Mono only for code, object keys, hosts and machine values. Never for labels, step numerals or heads.

## Surfaces
- Hairlines (1px rule) split rows; panels and chips are cut on two opposite corners (`.cut`, plain radius as fallback).
- Graphite is for the terminal panel only. The commands sit on a graphite-2 strip above the setup log.
- Cobalt stays under 5% of a screen outside the hero.

## Controls
- Primary: solid cobalt keycap (`.key--primary`), 6px radius, once per page. Today that key is "See GitHub" in the nav, at the 44px small size.
- Secondary: key-face keycap (`.key`), same radius and height.
- Each install command has a Copy button that copies the text of its `[data-cmd]` span exactly, and its accessible name names that command. If the copy fails, the button reads "Copy failed" and the command is selected for a keyboard copy. To change a command, edit that span in index.html.

## Motion
- Character: still. The hero setup log prints its lines once, in order, when it comes into view, and shows every line at once under reduced motion. No other reveals.

## Copy
- Every word passes the project's unslop skill. The page reads as it will on launch day, with no status caveats and no roadmap lines. The shipped files carry no drafting notes.
- Keep the notice that Flarebox is not affiliated with or endorsed by Cloudflare, Inc. Never put "Cloudflare" in the product name.

## Files
- `index.html`: the page, with one inline script for the Copy buttons and the setup log.
- `site.css` imports `tokens.css` and `base.css`.
- `fonts/`: the three WOFF2 files and their licenses.

The site is static, with no build step. To preview it, run `python3 -m http.server` in this folder and open the printed address.
