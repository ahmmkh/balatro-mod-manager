# Per-release manual test checklist

CI covers unit, install-flow, contract and startup tests on Windows, macOS and Linux. It cannot
launch the real (paid) game with Lovely injected, and it cannot see how the packaged UI looks.
Run this once per release on each real platform and paste the filled table into the release notes.

Mark each cell **Pass**, **Fail** (with a note), or **Not covered** (could not be checked on that
platform). Never write Pass for something you did not do.

| # | Check | Expected | Win 11 | macOS (Apple Silicon) | Linux native | Steam Deck / Proton |
|---|-------|----------|--------|-----------------------|--------------|---------------------|
| 1 | Install the packaged app (installer / dmg / deb / AppImage / Flatpak) | App starts; no crash dialog | | | | |
| 2 | Game path auto-detected; then set it manually to another folder | Detected path shown; manual choice persists after restart | | | | |
| 3 | Install Steamodded and Lovely from the app | Both listed as installed; no error toast | | | | |
| 4 | Install, update and uninstall one normal mod | Folder appears in the Mods dir, changes on update, is gone after uninstall | | | | |
| 5 | Install a mod with a Unicode name (e.g. one with `é`/`ü`) | Installs and appears in the installed list | | | | |
| 6 | Launch Balatro from the app with Lovely | Game starts and the installed mod is visible in the game's mod menu | | | | |
| 7 | Open the report form | Preview shows `~` / `<user>` instead of your home path and account name; Submit stays disabled until the box is ticked | | | | |
| 8 | Submit a report, then read it with the admin token (`GET /admin/reports/{id}/log`) | The stored log contains no account name, e-mail address or IP | | | | |
| 9 | Disable the network and restart the app | Last cached catalog is still listed (no empty error screen) | | | | |
| 10 | Run an older build against this release | The update prompt appears and links to this fork's releases | | | | |
| 11 | Mod list shows thumbnails and download counts | Thumbnails load; counts are non-negative numbers | | | | |

## Not covered this release

List every OS/row above that was not checked and why (no hardware, no game licence, ...).

- _(fill in per release)_

## Notes

- Row 6 is the only check that needs the real game; it is the main reason this list exists.
- Intel Macs are covered only by the universal build; there is no CI runner for them.
