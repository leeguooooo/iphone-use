# USB file handoff through the existing Runner

Reuse **iPhoneUse-Runner**, its existing icon, and iOS's native **Save to Files**
UI. No additional iPhone app, custom transfer screen, clipboard access, or
Handoff toggle is required. The Mac side receives an explicitly selected saved
file from the runner's own Documents container through CoreDevice.

This is an opt-in experiment, not a released MCP endpoint or automatic change
to runner setup. The earlier standalone-app prototype has been removed from
this proposal. The XCTest test-host delegate and control loop are unchanged.

## Enable the native Files entry (development setup)

Xcode generates the `.xctrunner` application. Setting plist keys on the nested
UI-test bundle does not enable file sharing on that outer app. This helper
stages a copy of an already signed outer runner, adds `UIFileSharingEnabled`
and `LSSupportsOpeningDocumentsInPlace`, and re-signs its outer seal using the
same identity and entitlements. It verifies signatures before and after,
preserves the original icon/nested code, and never modifies its source.

```sh
python3 mac/enable_runner_files.py \
  --source '/absolute/path/to/iPhoneUse-Runner.app' \
  --output '/absolute/new/path/iPhoneUse-Runner.app'
```

Use the existing Xcode development identity. While the managed runner is
paused, back up its cached product, make the verified staged app the product
used for `test-without-building`, install it with `devicectl`, and resume using
the normal iPhone Use supervisor. Keep a backup for rollback. This deployment
is deliberately explicit: the experiment does not rewrite the supervisor,
install another app, or change signing accounts. A subsequent rebuild/upgrade
can clear these flags; integration into the normal setup/signing/cache pipeline
is still needed before this becomes a persistent product feature.

The keys expose the runner's **whole Documents directory**, not only the inbox,
to the user's native Files/Finder file-sharing UI. Do not store tokens or other
internal secrets in Documents. No new network file server is started.

Create an inbox folder (for example `Documents/Send to Mac`) in the runner's
container with CoreDevice. Its exact bundle ID is the installed team-derived
`.xctrunner` ID, not the nested test bundle ID. The native path is:

**Files → On My iPhone → iPhoneUse-Runner → Send to Mac**

Optionally favorite the folder. The local Chinese-language validation used
“发送到 Mac”; a favorite is also available from the native save picker.

## Receive a file after saving

1. Obtain your normal iPhone Use control lease. Confirm its exact owner, UDID,
   instance and wired connection. Do not borrow another caller's owner.
2. In the source app, use the system share menu → **Save to Files**, select the
   runner inbox, and finish saving. Confirm the dialog completed and note the
   exact filename. This is not an automatic “latest file” selector: old files
   remain until the user removes them.
3. Run the receiver using your actual device/bundle/owner values:

   ```sh
   python3 mac/runner_files.py receive \
     --device '<exact-udid>' --bundle '<installed-runner-id>.xctrunner' \
     --instance default --owner '<your-session-owner>' \
     --folder 'Documents/Send to Mac' --filename '<exact-saved-name.pdf>' \
     --destination '/absolute/output/directory'
   ```

`IPHONE_USE_OWNER` can supply `--owner`. `--expect-sha256` optionally checks a
known sample. Maximum/default accepted file size is 64 MiB. This is validated after CoreDevice
reads as well as against its initial metadata; it is not a hard download/disk quota. `--state-dir` relocates
runtime state from `~/.iphone-use/usb-file-bridge`.

The receiver lists the exact saved file, verifies size/type, reads it twice with
stable metadata checks, compares both SHA-256 values, and atomically publishes
without overwriting a different destination. **Hashing is on the Mac**; this
version does not add an on-phone hashing service. Two stable reads cannot prove
an arbitrary writer has finished; invoke only after the native save completed.
There is no separate phone-side receipt UI: the native file and the Mac's
`committed=true` result are the visible outcomes.

```sh
python3 mac/runner_files.py status --device '<udid>' --bundle '<runner>.xctrunner'
python3 mac/runner_files.py stop   --device '<udid>' --bundle '<runner>.xctrunner'
```

Use the same instance/state-dir for each command. `stop` uses a session token,
not a stale PID. An OS lock scopes exclusivity by UDID/bundle/instance. Agents
should select one route per task and avoid clipboard-export flows while a USB
receiver is active. Apple clipboard settings remain unchanged throughout;
there is no global switch to restore after cancellation or disconnection.

## Recover an interrupted publication

Each receive emits a `started` event with a transfer ID (or accepts a caller's
fresh UUID via `--transfer-id`). Before publishing a destination, the receiver
persists an intent containing its expected size/hash, path and inode identity.
After an interrupted command, query without touching the phone:

```sh
python3 mac/runner_files.py result \
  --device '<udid>' --bundle '<runner>.xctrunner' --transfer-id '<uuid>'
```

Use the original instance/state-dir. The result distinguishes `committed=true`,
`not_published`, and `unknown` (`committed=null`). A successful link followed by
cancellation is reconciled instead of being reported as a failed copy. A hard
process exit before the normal receipt can also be queried from the persisted
intent. Missing/replaced/modified targets may yield unknown; do not automatically
retry an unknown outcome. Existing transfer IDs cannot silently overwrite their
records. A missing record is not a substitute for inspecting an interrupted task.

The ID identifies the receive/publication attempt, not the originating app's Save
transaction. This does not prove source freshness, add an atomic phone lease,
or guarantee storage survives every hardware/power failure. Result queries are
read-only and do not remove orphan staging paths left by a hard process kill.

## Boundaries

- Owner and target UDID are checked before and after the blocking CoreDevice
  `wired` probe, immediately before each device copy, and before commit. CoreDevice controls the underlying route: checkpoints are not
  a transport lock and do not prove a cable cannot change between checks.
- Commands are bounded by the remaining deadline and a 15-second command cap.
  Owner loss, cancellation and connection errors abort before publication.
- Paths, names, file type, size, stable metadata and read digests are checked.
  Publication uses a flushed private (0600) staging file and a non-overwriting
  atomic hard link. An identical existing file is idempotent; symlinks and
  different content are refused. Receipts stay on the Mac.
- Exact-file selection avoids silently substituting another file. Queues,
  automatic cleanup, wireless transport and production setup integration are
  not implemented. Files remain in the user's runner inbox for inspection.

## Tests

```sh
python3 -m unittest discover -s tests -v
```

See [VALIDATION.md](VALIDATION.md) for the current Runner-native hardware checks.
No invoices, account/device identifiers, signing material or machine-specific
paths are included in this contribution.
