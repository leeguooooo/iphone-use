# Centralized scan review (2026-10-17)

The catalog scan of source commit `9d4b9000969a8b2956de082985deeeb6246e745d` used plugin-scanner 3.0.123 and scored 74. Local reproduction with the same version also scored 74. The catalog requires a completed centralized score of at least 80; a local result does not replace that gate.

## Changes

- Pin every external GitHub Action to an immutable commit; use a master-history Rust action commit with an explicit stable toolchain input, as required by upstream. Disable checkout credential persistence in release jobs.
- Add weekly Dependabot updates for Actions, the root Cargo workspace and the independent launcher workspace.
- Document vulnerability reporting and supported versions in SECURITY.md.
- Exclude build products and local credential files from the Codex plugin package.
- Run the catalog's SHA-pinned scanner action on pull requests and main pushes, with read-only permissions, no persisted checkout credentials, offline scanning and the same score threshold. No repository-level scanner suppressions or baseline are used.

## Credential findings

All 24 high findings are the following 12 locations counted once for each detected ecosystem (Claude and Codex). None contains a usable embedded credential. Locations refer to the original scanned commit.

| Location | Review |
| --- | --- |
| `install.sh:2983` | Reads the caller's IPHONE_USE_PASSWORD environment variable, defaulting to empty; no literal password. |
| `apps/ios/IPhoneUseRemote/Sources/BrowserDeviceAcceptance.swift:45` | Acceptance fixture for example.com; its explicitly synthetic password decrypts synthetic fixture data. |
| `apps/ios/IPhoneUseRemote/Sources/DeviceSession.swift:79` | Writes a runtime scanned pairing code and timestamp to Keychain; interpolation is not a hardcoded credential. |
| `apps/ios/IPhoneUseRemoteTests/BrowserLibraryTests.swift:21` | Synthetic work/home cookie values in a unit test. |
| `crates/server/src/lockdown.rs:754` | Invalid RSA PEM fixture with AAE= body, used to exercise error handling. |
| `crates/server/src/wda.rs:4757` | Sentinel string in a regression test ensuring response bodies do not leak into error messages. |
| `crates/server/src/setup/asc.rs:39` | Documentation of PEM boundary syntax, not key material. |
| `docs/direct-device-architecture.html:1133` | Shell command reads a token from the user's local LaunchAgent; no literal token. |
| `runner/README.md:428` | Passes a freshly generated runtime token to XCTest via an environment variable. |
| `runner/unit-check/main.swift:726` | Fixed HMAC test vector, never installed as a live runner token. |
| `scripts/remote-connection-check.swift:121` | Explicitly synthetic device token in a stub-backed acceptance check. |
| `scripts/test-flow-reverify-record.py:182` | Test-only token with stubbed flow verification. |

These fixtures and runtime operations remain unchanged to preserve coverage and behavior. No credential rotation is indicated by these findings.

## Scanner metadata findings

Version 3.0.123 requests `strict` at the Claude marketplace root. Claude marketplace strictness belongs on each plugin entry, so we do not add an invalid root field to satisfy this check. The current upstream scanner source has a per-entry check; the catalog still uses the pinned version above.

The two informational Codex asset findings concern omitted optional `termsOfServiceURL` and `screenshots` fields. No broken URL or screenshot path is present. We do not invent terms or assets to remove optional-field warnings.

## Reproduction

In an isolated Python environment, install `plugin-scanner==3.0.123`, then run:

```sh
plugin-scanner scan . --format json --output /tmp/iphone-use-scan.json --min-score 80 --fail-on-severity none
```

After the changes above, the local score is 88 (24 high, 1 medium, 2 informational findings, as reviewed above). The source CI and catalog centralized scan must independently verify the committed revision before acceptance.
