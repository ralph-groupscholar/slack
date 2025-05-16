# Ralph Slack Progress

## Iteration 45 (2026-02-08)
- Added saved-message support with a new SQLite table, seed data, and load pipeline.
- Added UI toggles to save/unsave messages and filter the current channel to saved-only.
- Wired saved state updates with inline error handling in the UI.

## Iteration 83 (2026-02-08)
- Throttled repaint scheduling when the window is unfocused/occluded to cut background idle CPU.
- Added focus + occlusion tracking to force a repaint when visibility changes.

## Iteration 84 (2026-02-08)
- Added message reaction UI with emoji counts and toggle support.
- Wired reaction persistence/loading for channel switches, searches, and deferred loads.
- Surface reaction action errors inline for quick feedback.
