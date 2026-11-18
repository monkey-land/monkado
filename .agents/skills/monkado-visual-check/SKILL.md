---
name: monkado-visual-check
description: Launch the native Monkado app with synthetic --demo data, capture its window to PNG in the git-ignored .evidence/ folder and send the images to the owner. Use for every visible UI change (dialogs, call stage, tiles, badges, pickers, players) before calling it done, and whenever the owner asks to see how something looks.
---

# Monkado visual check

Every visible change gets looked at in the running app, not only compiled. The owner wants to see
the screenshots: send them in the conversation and keep them in `.evidence/`, which git ignores.

## Where images go

- `.evidence/<task-slug>/<state>.png`, for example `.evidence/share-picker/after-windows-tab.png`.
- `.evidence/` is in `.gitignore`. Never commit these images and never put them under `docs/`.
- Send each inspected image to the owner (`SendUserFile` when available; otherwise give the
  absolute path). In the PR, describe what was checked; do not link local paths.

## Capture

1. Build once with the demo feature: `cargo build -p monkado --locked --features demo` (on
   Windows set `CMAKE` to the Visual Studio bundled `cmake.exe` if the system CMake lacks the
   installed Visual Studio generator). Without the feature `--demo` exits with code 2.
2. Launch with synthetic data only and keep its PID, for example in PowerShell:
   `$demo = Start-Process target/debug/monkado.exe -ArgumentList --demo -PassThru`.
   Never launch a saved authenticated session for evidence.
3. Drive the app to the state to check. Use computer-use tools when the session has them
   (clicks, typing). Without them, capture what the start state shows and say which states
   could not be reached.
4. Capture only the app window:
   `pwsh -File .agents/skills/monkado-visual-check/scripts/capture-window.ps1 -Out .evidence/<slug>/<state>.png -Id $demo.Id`
   It renders that process's window with `PrintWindow` and saves a PNG; no other windows or
   notifications end up in the image. Always pass the PID: the owner often has their real,
   signed-in Monkado open, and capturing it would leak their account. The script refuses a
   process that was not started with `--demo`.
5. Read the PNG yourself before sending it. Look for clipped text, overlapping badges, blank
   areas, unreadable contrast and wrong language. Fix and recapture before reporting.
6. Stop only the process you started: `Stop-Process -Id $demo.Id`. Never stop by name; that
   would close the owner's own Monkado.

## What to capture

- Before and after when the change alters an existing screen: capture `before` on the starting
  commit's build before editing, then `after`.
- The states the change touches: empty, loading, many items, long names, narrow window.
- Demo data is synthetic. Say so when sending; never present it as a real Discord account.

Some states need a live call or real sources (for example share previews of real windows) that
`--demo` cannot produce. Report that limit instead of claiming the state was checked.
