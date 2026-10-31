# Runner-native validation — 2026-10-29

macOS 27.0.1, Xcode 27.1, iPhone 17 Pro / iOS 27.0.1, iPhone Use 0.27.6.
Based on upstream `a1355ab`. Private identifiers and source documents are omitted.

## Current implementation

- The existing signed `.xctrunner` was backed up, staged with the two public
  file-sharing plist keys, and re-signed with the same identity/entitlements.
  `codesign --verify --deep --strict` passed; icon and nested executable preserved.
- Installed into the same runner identity and resumed through the official
  supervisor. `drivable=true`, `runner_actionable=true`, `transport=usb`.
- iOS Files displayed **iPhoneUse-Runner** with its existing icon and the inbox
  folder. The folder was added to Favorites using native UI.
- QQ Mail real PDF → Save to Files → runner inbox → Mac: 109,440 bytes, expected
  SHA-256 matched. Repeating the receive returned `already_identical`.
- Synthetic 16 MiB file seeded into the same runner container → Mac: 16,777,216
  bytes, expected SHA-256 matched. This checks the payload path; the real PDF
  separately covers native Save UI. Two copies were read for stability.
- No Copy action, pasteboard API, shared-pasteboard cache or Handoff-setting
  change was used. Hashes are computed on the Mac, not inside the runner.

`python3 -m unittest discover -s tests -v`: **38 tests passed**. Coverage includes
actual CLI errors/status, cancellation/locks/stale state, lease/UDID/wireless
checks, deadlines, path/metadata/size validation, two-read disagreement, source
changes, expected digest, idempotency, symlinks, non-overwriting publication
races, post-commit receipt errors and preservation of plist identity/icon.
Device errors are mocked for the failure-path tests.

Synthetic bytes: `0..255` repeated 65,536 times (16 MiB).
SHA-256: `341aacac661ccb210720bedaa9ead5d668fe5ea41a73532fc147c71e34040df1`.

## Scope changes and remaining checks

The standalone Swift app and its UI have been removed from the proposal.
Its earlier 23 tests and Debug/Release builds are historical evidence for that
previous prototype, not claimed as Runner-native coverage. This version does
not change Runner source or require a new Swift binary build.

Physical cable removal during a copy, transient route changes between guard
checkpoints, the 64 MiB hardware boundary, multiple devices, older iOS versions,
and persistence through an official rebuild/upgrade remain untested. Native
Save must finish before reception; two stable snapshots alone cannot establish
completion of an unrelated asynchronous writer. This remains a draft experiment
pending setup/signing/cache integration review.

## Owner-loss regression (review follow-up)

The reviewer reproduced an owner change during the blocking CoreDevice
transport probe. The new tests failed against the prior code (including actual
`commit_file` calls with the real guard), then passed with the fix. Lease checks
now run after transport discovery and immediately before each device copy.
Cases include owner replacement/release, target UDID change, loss of drivable
state, fresh destination publication, and an existing identical destination.
Aborted publication leaves no new target or staging file and releases its lock.
These are offline regressions; no additional phone access was needed.

This closes the reproduced blocking-probe window, not every possible race:
a status observation is not an atomic lease held across a device subprocess or
filesystem publication. A daemon-native transaction is still required for that
stronger guarantee.

## Recoverable publication follow-up

Ten additional offline tests cover cancellation immediately after a successful
link, interrupted success-receipt delivery through the CLI, journal-completion
failure, pre-publication abort, uncertain commit attempts, replacement/in-place
modification of the target, ID reuse/traversal, and recovery from a separate
process that exits immediately after linking (no Python cleanup). The new process
can query the persisted intent and verify the destination identity and hash.

These tests do not strengthen the documented owner/transport checkpoints into
an atomic lease. Same-name lease reincarnation, source freshness and hard live
I/O quotas still require integration with the daemon/transfer protocol.


## Descriptor-bound reconciliation follow-up

Recovery now hashes the same opened regular-file descriptor whose device/inode
matches the publication intent. It checks file metadata and the destination path
binding before and after the read. Replacement or in-place changes during the
inspection remain `unknown`.

Three offline regressions cover replacement between identity inspection and
hashing, replacement during hashing, and in-place modification during hashing.
All three fail against the prior recovery code; the full 38-test suite passes
with the descriptor-bound check. No additional phone access was used. The result
is a read-time observation, not a promise that another process cannot change the
file after the check completes.
