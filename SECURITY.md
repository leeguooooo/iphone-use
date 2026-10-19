# Security policy

Security fixes target the latest released version. Upgrade before checking whether a report still reproduces.

## Reporting a vulnerability

Please email leeguoooo@gmail.com with the affected version, platform, reproduction steps and expected impact. Do not put credentials, pairing records, private phone data or exploit details in a public issue. Share a minimal reproduction using synthetic data where possible.

We will assess reports and coordinate a fix and disclosure with the reporter. There is no guaranteed response time or bounty program.

## Security boundaries

The daemon listens on loopback by default and requires authentication. Runner requests use per-launch signing credentials. Remote access should use an authenticated HTTPS proxy or VPN; do not expose device control ports directly to the internet. The default IPU_RUNNER_TRANSPORT=auto off-cable path uses the encrypted CoreDevice Wi-Fi tunnel. Explicitly setting IPU_RUNNER_ALLOW_LAN=1 additionally permits an unencrypted plain LAN relay, including the legacy iOS 15/16 fallback; use that only on a trusted, isolated network.

Control of an unlocked phone can expose personal data and perform actions in installed apps. Keep pairing records, signing keys and daemon credentials private. See [the security guide](docs/guide.md#security) and [privacy policy](docs/privacy.md).
