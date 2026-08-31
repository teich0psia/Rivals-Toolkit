# Session Mod Launch (fork design)

## Goal

Add an opt-in launch mode that keeps the Marvel Rivals install vanilla while the game is not running, then deploys the selected mod state only for launches initiated by Rivals Toolkit and restores the vanilla-at-rest state after the game exits.

Default behavior must remain identical to upstream.

## Upstream-friendly design

Fork-only lifecycle behavior lives in a dedicated `session_launch` backend module and a small dedicated frontend control. Existing upstream modules should only contain narrow integration points that delegate to the fork module when the option is enabled.

The fork should avoid changing core pak parsing, asset tooling, or game detection. Bypass helpers may expose narrow session-specific ownership operations so the session lifecycle never has to delete an unknown third-party loader.

## Mode semantics

### Disabled (default)

Rivals Toolkit behaves exactly like upstream:

- mod enable/disable operations rename files in `~mods` directly;
- signature bypass install/remove is persistent;
- Launch Game uses the normal detected launcher URL.

### Enabled

At rest:

- selected mods are represented logically by fork state;
- their physical files remain disabled (`*.disabled`);
- the Toolkit-managed signature bypass remains absent from the game directory;
- an unrecognized third-party `dsound.dll` is left untouched;
- launching Marvel Rivals directly through Steam/Epic/Loading Bay therefore does not inherit the Toolkit-managed mod state.

Toolkit launch:

1. Verify the shipping process is not already running.
2. Deploy the logically selected mods.
3. Deploy the logically enabled signature bypass without overwriting an unrecognized loader.
4. Persist a deployment record containing only files owned by this session.
5. Launch through the normal upstream launcher URL.
6. Run a detached watchdog process.
7. After `Marvel-Win64-Shipping.exe` exits, restore the at-rest state and clear the deployment record.

If Rivals Toolkit itself closes while the game is running, the watchdog remains responsible for cleanup. If the machine or process terminates unexpectedly, the next Toolkit start performs stale-deployment recovery.

## State ownership

Session-launch configuration is stored separately from upstream `settings.json` so the feature remains self-contained and upstream changes to `Settings` produce fewer merge conflicts.

`session-launch.json` contains only long-lived logical state:

- option enabled/disabled;
- logical selected mod display names;
- logical signature-bypass enabled state.

`session-deployment.json` contains the active physical deployment record. It is intentionally separate because the detached watchdog is a different process from the Toolkit GUI. Either process reads the deployment record from disk instead of assuming the other's in-memory state has changed.

Older fork builds that stored `deployment` inside `session-launch.json` are migrated on startup.

## Integration boundaries

Expected upstream-touch files:

- `src-tauri/src/lib.rs`: register/manage the fork module and commands;
- `src-tauri/src/main.rs`: watchdog entrypoint;
- `src-tauri/src/detect.rs`: delegate Launch Game when session mode is enabled;
- `src-tauri/src/mods.rs` and `src-tauri/src/mods/bypass.rs`: narrow session-safe bypass ownership helpers;
- `src-tauri/src/mods/commands.rs`: logical selection/bypass wrappers;
- `src-tauri/src/mods/profiles.rs`: profile operations against logical selection;
- `src/App.tsx`: mount the dedicated option control.

Everything else should remain fork-local.

## Safety invariants

- Never enable session mode while the shipping process is running.
- Never switch back to persistent mode while the shipping process is running.
- Session cleanup only removes loader/payload files whose contents still match files deployed by Toolkit.
- Never delete or overwrite an unrecognized third-party `dsound.dll` for session deployment.
- Refuse session mode when a custom payload or legacy bypass layout cannot be managed without destructive replacement.
- A launch failure triggers immediate cleanup.
- A stale deployment is recovered on Toolkit startup when the game is not running.
- The option is off by default.

## Known boundary

This feature manages Rivals Toolkit's normal pak/IoStore selection and its bundled signature bypass. Arbitrary root/plugin payload deployment (for example third-party ASI plugins) should be added as a separate session-payload module instead of being hard-coded into the session lifecycle.

## Review checklist

- Upstream behavior is unchanged when the option is disabled.
- Direct launcher use while session mode is idle sees no Toolkit-enabled pak files or Toolkit-managed bypass.
- Toolkit launch deploys only the logical selection.
- Cleanup occurs after a normal exit, Toolkit GUI exit, and stale-session recovery.
- A second Toolkit launch works without restarting the GUI after watchdog cleanup.
- Third-party loaders survive session-mode enable/deploy/cleanup transitions.
- Mod profiles read/write the logical selection in session mode.
- Unit tests, lint, format, type checks, and build checks pass.
