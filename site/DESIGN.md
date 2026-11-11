# Design: Koidrive

The design rules for this site. Every page follows them. Change a rule here before a page breaks it.

The system is coreav-dev/site's (its DESIGN.md, src/styles/tokens.css, src/pages/index.astro and its Nav, Footer and Base layout): Hallmark's Ultraviolet theme for type, pills and hairlines, on the colours of Hallmark's Rim theme at night. The nav, the hero and the footer take its markup and its values. Koidrive keeps its own copy and logo, and every place it differs is in the table under "Departures from coreav-dev/site". The page has had a Hallmark v2.0.0 polish against this file; the run record is the stamp at the top of `site.css`.

## System
- Genre: modern-minimal. Dark only. Every head, button and label keeps the copy's own case.
- Shape: the edge-aligned nav (the lockup and the links at opposite edges, no rule, 72px tall; under 40rem the lockup alone), the hero and a colophon footer with the trademark notice. Nothing on the page is decoration only. Add a section only when the project has true content for it.
- Hero: CoreAV's Ledge in its bare form. The hero is at least the screen's height less the nav (`calc(100svh - 72px)`), and the head sits in its middle. The hero's bottom padding adds the nav's 72px to CoreAV's 40px, so the head centres on the screen rather than on the space under the nav, a touch above the screen's middle. The head is two equal columns (6fr and 6fr), centred on each other: the claim (the headline, then See GitHub 40px under it) on the left, the install block on the right. Under 60rem the columns stack, the hero drops its minimum height and takes 64px above and 96px below. The bare form has no field row there either, so nothing adds space under the head.
- Footer: 120px under the hero (80px under 60rem, 56px under 40rem), a hairline, then the notice. The body is a column at least the screen tall, so on a short page the footer still sits at the foot of the screen.
- Axes: dark/geometric-sans/warm, posture: committed.

## Tokens
`tokens.css` is the source of truth. It is coreav-dev/site's tokens.css with one token added, `--color-surface`; every other value is unchanged. The roles:
```css
:root {
  --color-paper: oklch(9% 0.008 75);  --color-paper-2: oklch(12.5% 0.009 75);  --color-paper-3: oklch(6.5% 0.007 75);
  --color-surface: oklch(19% 0.009 75);
  --color-ink: oklch(94% 0.010 80);  --color-ink-2: oklch(84% 0.011 80);  --color-muted: oklch(68% 0.012 80);  --color-muted-2: oklch(50% 0.010 80);
  --color-rule: oklch(25% 0.010 78);  --color-rule-2: oklch(16% 0.009 78);
  --color-accent: oklch(72% 0.17 45);  --color-accent-deep: oklch(64% 0.17 44);  --color-accent-soft: oklch(32% 0.06 45);
  --color-accent-ink: oklch(10% 0.02 45);  --color-focus: oklch(78% 0.15 45);
  --font-display: "Outfit";  --font-body: "Onest";  --font-outlier: "JetBrains Mono";
  --radius-card: 16px;  --radius-pill: 999px;  --rule-hair: 1px;  --shadow-card: none;
}
```
The spacing scale, the type scale, the easings and the durations live in `tokens.css` too. `base.css` is coreav-dev/site's base.css without its image-slot rules, which nothing here uses.

## Meanings
- One orange in two strengths. The full one is for the one filled control, focus and a link's hover. The deep one is the filled control's hover. The soft one is unused today.
- `--color-surface` is the release line's fill, a grey step above the paper that reads as a panel. On it the ink is 15.6:1.
- `--color-rule` is the section line (over the footer). `--color-rule-2` is the fainter row line and a nav link's hover. Pill outlines take `--color-muted-2`.
- Body text is ink-2; heads, labels and commands are ink; the notice and the prompt are muted.

## Type
- The fonts are self-hosted in `fonts/` from @fontsource (Outfit 5.3.0, Onest 5.3.1, JetBrains Mono 5.3.0), Latin subset, only the weights in use (Outfit 500, Onest 400, JetBrains Mono 400), each with its SIL Open Font License beside it. `fonts.css` declares them. The page loads nothing from another host.
- Heads: Outfit 500, tracking -0.03em, line height 1. The headline is `clamp(2.5rem, 4.6vw, 4.5rem)` with balanced lines. Labels and buttons: Outfit 500. Body: Onest 400.
- JetBrains Mono 400 is for commands only, at 14px on a 24px line, so they are never smaller than the 14px labels around them.

## Logo
- The wordmark is "Koidrive" in Outfit 600, outlined so it needs no font load, with the font's own kerning and -0.01em of tracking, in the ink (`logo/wordmark.svg`). Only the chosen wordmark outlines ship; no comparison weights or Poppins are included. The existing Outfit 500 file remains for the page's headings and controls, not the wordmark.
- The mark is the owner's supplied `logo/koidrive-bold-icon.svg`, orange koi on a transparent ground. `logo/koidrive-bold-appicon.svg` is the supplied white koi on an orange rounded tile. Both source SVGs are kept unchanged. Their orange is `#FF6A13`; it belongs to the supplied artwork, while the page's accent token stays unchanged.
- The nav uses `logo/nav-icon.svg`, a viewBox-only crop of the supplied transparent icon to `74 39 357 425`. Its paths are unchanged. The original SVG and app tile remain unchanged for other uses. The cropped icon is 7/6 of the wordmark's height with its width set by its aspect ratio: 28px tall beside the 24px wordmark, or 23.33px beside 20px on a phone. This small optical overshoot balances the thinner koi silhouette against the wordmark's stems. Both are centred on the same line, with the existing 10px desktop or 8.33px phone gap.
- `favicon.svg` is the supplied app-icon SVG; `favicon.ico` holds 16 and 32px renders of it. `apple-touch-icon.png` and the manifest's 192 and 512px icons also render that SVG, without redrawing the fish.
- `og.png` is a 1200 by 630 image with the transparent icon and outlined wordmark on the dark paper. The page names it in the Open Graph and Twitter card metadata. Its URL is relative in this undeployed preview; use an absolute deployment URL when a hostname is chosen.
- `logo/draw.py /path/to/outfit-latin-600-normal.woff2` draws the wordmark from an external Outfit 600 Latin WOFF2 file from @fontsource/outfit 5.3.0 and generates the cropped nav SVG. The source font stays outside the repository. The script needs fonttools and brotli. `logo/icons.mjs` renders the supplied app icons and OG image using sharp. Install these tools outside the repository.
- The GitHub links still name flarebox-dev/flarebox. This page changes the landing-page identity, not the repository name.
- Don't reuse CoreAV's wordmark setting, its AV monogram or its icons.

## Controls
- Primary: the orange pill, `--color-accent-ink` text, `--color-accent-deep` on hover, 48px, Outfit 500. Once per page: "See GitHub", 40px under the headline (24px under 60rem).
- Secondary: a pill with a 1.5px inset outline. Nothing on the page uses it today.
- Disabled: an `aria-disabled` pill drops to 55% opacity with a not-allowed cursor and does not move.
- Nav links: Outfit 500 at the label size in a 40px pill that steps to rule-2 on hover.
- No install commands ship until there is a release. In their place the act column holds one line, "The first release is coming soon.", in the ink on the surface fill with 16px of padding all round.
- Focus: a 2px ring in `--color-focus`, 3px out on pills and 2px on links.

## Motion
- Character: quick and dry. Colours step over `--dur-micro` (120ms) and a pressed pill moves 1px. Nothing is revealed on scroll or load. Under reduced motion every transition ends at once.

## Copy
- Every word passes the project's unslop skill. The page carries one status line, the release line, and no roadmap lines.
- The ® sits 0.2 of its own size (0.064em of the headline) after "Cloudflare", with its top on the cap line.
- The trademark notice follows Cloudflare's trademark guidelines (https://www.cloudflare.com/trademark/): the first "Cloudflare" in visible text carries ®, set small at the cap line (`.reg`), and the footer names the marks the page uses and their owners: Cloudflare, Inc. for Cloudflare and Dropbox, Inc. for Dropbox. It ends "Koidrive is not affiliated with or endorsed by either company." Keep it, and name a mark there only when the page uses it.
- Never put "Cloudflare" in the product name.

## Departures from coreav-dev/site
| Where | coreav-dev/site | Koidrive | Reason |
| --- | --- | --- | --- |
| Hero | Ledge with the speed chart as its field when there is data, the bare form without it | The bare form only | Our copy: there is no chart to draw |
| Headline | A forced break (`<br>`) before the orange word's line | No forced break; balanced lines break it as "Your own Dropbox / on Cloudflare" | Our copy |
| Headline | No mark | ® after "Cloudflare" | Trademark rules |
| Under the headline | The lead sentence | See GitHub | Gunwoo's cuts (no lead) and his See GitHub move |
| Install block | One command in a `<pre>`, then a note, then the GitHub pill | One release line on the surface fill, no commands | The maintainer's call: no release or Homebrew tap exists yet |
| Columns of the head | Feet on one line (`align-items: end`) | Centred on each other (`align-items: center`) | Gunwoo's request |
| Hero bottom padding, 60rem and wider | 40px | 40px plus the nav's 72px | Design review: centres the head on the screen, not under the nav |
| Release line fill | `--color-paper-2` | `--color-surface` | Gunwoo's request: the block was close to invisible on the paper |
| Gap above the primary pill | 16px (`.cta`), under the install note | 40px under the headline, 24px under 60rem (`.claim .cta`) | Gunwoo's request: 16px was too tight under the headline |
| Primary pill | "GitHub", in the right column under the note | "See GitHub", in the left column under the headline | Our copy (the label) and Gunwoo's See GitHub move (the place) |
| Nav | The CoreAV wordmark; Docs, Blog and GitHub | The Koidrive lockup (the supplied koi icon and the wordmark); GitHub, hidden under 40rem | Brand rules (the wordmark) and our copy (no docs or blog); hiding the link on a phone is Gunwoo's request |
| Footer | The licence and the Blog and GitHub links | The trademark notice | Trademark rules (the notice) and our copy (no blog) |

## Files
- `index.html`: the page. It has no script.
- `site.css` imports `tokens.css` (which imports `fonts.css`) and `base.css`, then styles the page. The nav, hero and footer rules are coreav-dev/site's scoped Astro styles, made global under their class names.
- `fonts/`: the three WOFF2 files and their licences. `logo/`: the logo and the scripts that draw it.
- `favicon.*`, `apple-touch-icon.png`, `icon-*.png` and `site.webmanifest`: the icons.

The site is static, with no build step. To preview it, run `python3 -m http.server` in this folder and open the printed address.
