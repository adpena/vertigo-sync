# Vertigo Sync Icon Direction

## Current shipping direction

The current selected mark is the yellow-on-black left-leaning `V` bolt in:

- `branding/marketplace/vertigo-sync-icon.svg`
- `branding/marketplace/vertigo-sync-icon-512.png`
- `branding/marketplace/vertigo-sync-icon-1024.png`

This is now the source of truth for toolbar and marketplace packaging work.

## Visual rules

- silhouette must stay legible at `16x16`
- avoid tiny detached details
- no gradients in the shipping marketplace icon
- use a black field with a single high-contrast yellow mark
- the `V` should feel fast, severe, and centered enough to read instantly in toolbar scale

## Palette

- Accent: `#FFD21F`
- Deep: `#060606`

## Notes

- the icon should not reuse Rojo visual language
- the icon should not imply cloud sync; this is a local-first dev tool
- the icon should still work as monochrome toolbar art
- local OSS installs should not depend on a published Roblox asset ID
