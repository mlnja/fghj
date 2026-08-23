# fghj Design System

A from-scratch visual system for **fghj**, a local development orchestration tool. No existing brand, logo, or codebase was provided — this system was built fresh for this project.

**Concept: "Container Yard."** fghj moves services around like cargo — containers, manifests, dependency graphs. The visual language leans into that literally: industrial/utilitarian, dark canvas, stenciled crate typography, safety-orange signal color, dashed connector lines, corner brackets like technical schematics, hazard-stripe bars for dirty/warning states.

## Sources
None. Built from scratch based on the product's technical, developer-tool, platform-engineering positioning (see the attached technical specification for fghj).

## Content fundamentals
- Direct, technical, no marketing fluff. Written the way a CLI tool's own docs would read.
- Domains, branches, paths, and code use monospace (JetBrains Mono) — body copy uses Space Grotesk, headers use the stencil display face.
- No emoji. Status is communicated with a colored dot + short uppercase mono label (RUNNING / STOPPED / DIRTY / CLEAN), or a crate-style code tag (`CTR-01`).
- Lowercase, hyphenated identifiers (`checkout-flow`, `cart.fghj.internal`) are kept verbatim, never title-cased.

## Visual foundations
- **Color:** near-black canvas (`--bg`/`--panel`/`--panel-2`) with a warm off-white ink. Safety-orange (`--accent`, hue 55) is the single brand signal color — used sparingly for actions, active states, and eyebrows. Status uses three semantic colors (success green, warning amber, danger red).
- **Type:** Big Shoulders Stencil for display/headers (industrial crate-stencil look), Space Grotesk for body, JetBrains Mono for anything technical. All substituted from Google Fonts — no original font files exist for this brand yet.
- **Spacing:** 4px base scale (`--space-1` = 4px … `--space-8` = 64px). Corner radii: 6 / 10 / 14px (sm/md/lg).
- **Signature details:** `.corner-brackets` (schematic-style orange corner marks on key cards), `.crate-tag` (dashed-border mono code chips, e.g. `FLOW-02`), `.hazard-bar` (diagonal warning stripe), `.grid-bg` (faint blueprint grid on dark canvas).
- **Surfaces:** flat, no drop shadows — depth comes from panel layering (`bg` → `panel` → `panel-2`) and hairline borders, not shadows.
- **Iconography:** none defined. The UI uses colored dots, crate-tag codes, and mono labels instead of icons. Flag: introduce a real icon set once the product's visual identity firms up.

## Files
- `styles.css` — root import list (link this one file)
- `tokens/typography.css`, `tokens/colors.css`, `tokens/spacing.css` — CSS custom properties
- `tokens/utilities.css` — base reset + `.eyebrow` / `.lede` / `.body-sm` / `.body-xs` type utility classes
- `guidelines/` — foundation specimen cards (colors, status, type)

## Caveats
This is a fast, from-scratch v0: no logo, no dedicated UI component library, no icon set. It covers the tokens and type/color system needed to style the fghj dashboard consistently. Happy to iterate — tell me what to push on first (a real logo mark, an icon set, more component variety).
