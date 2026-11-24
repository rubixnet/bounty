# Design Tokens & UI Seam Contract

> **STATUS: LOCKED UNTIL STEP 6.**
> Do NOT install frontend packages, scaffold `apps/web/`, or build UI components while working on Steps 1–5.
> Full page layouts, UX flows, and component specs will be added to `docs/UI.md` once Steps 1–5 pass all Exit Gates.

## Non-Negotiable Step 6 Constraints (From `criteria.md`)
1. **Single Swappable Seam:** Every import of `@hackclub/tokens`, `@hackclub/theme` (`<hc-button arrow>` and web components), and `@hackclub/icons` MUST live exclusively inside `packages/ui/src/design-seam/`.
2. **Zero Hardcoded Styles:** No raw hex colors, custom spacing scales, or third-party icon libraries (Lucide, Heroicons, etc.) anywhere outside `packages/ui/src/design-seam/`.
3. **CI Enforcement:** Step 6 must add an automated lint rule that fails CI on any seam violation.
4. **Target Stack:** Next.js 16+ with React 19 (native custom-element support for `@hackclub/theme` web components).