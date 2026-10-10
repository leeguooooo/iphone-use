# Phone Use Remote — Privacy Policy

Last updated: 2026-10-07 · [中文](privacy.zh-CN.md)

This policy covers the **Phone Use Remote** iOS app (bundle `com.leeguoo.iphone-use.remote`) and the
open-source [iphone-use](https://github.com/leeguooooo/iphone-use) software it connects to.

## What the app does

Phone Use Remote is a remote control for a phone that **your own Mac** drives with the iphone-use daemon.
The app shows that phone's screen and sends your taps, swipes and text to it.

## Data we collect

**None.** The developer runs no server for this app and receives no data from it:

- no account, sign-up or login with the developer;
- no analytics, crash reporting, advertising or tracking SDKs;
- no data sold or shared with anyone.

## Where your data goes

The app talks **only to the Mac address you enter or scan**, which is software you run yourself:

- the screen picture and your input travel between this app and that Mac over your network;
- the Mac's address is stored in the app's preferences; the control password or the pairing token
  is stored in the iOS Keychain on this device;
- the camera is used only to read the pairing QR code; no image is stored or sent anywhere;
- local network access is used only to reach that Mac.

Deleting the app removes its stored address; **Forget this Mac** in the app's settings removes the
saved password and pairing token.

## The Mac daemon's usage counts

The iphone-use daemon on your Mac contains code for anonymous usage counts, and it is **currently
off for everyone**: no project token ships with it, so it sends nothing. The iOS app has no such
code at all.

If a future release turns it on, it will say so here and in [telemetry.md](telemetry.md). It would
count only which agent API endpoint was called, whether it worked, a failure class from a fixed
list, and how long it took, with the daemon's version, platform and a random install id. It would
never send text, element labels, screenshots, app bundle ids, phone identifiers or names, or file
paths. `IPHONE_USE_TELEMETRY=0` or `DO_NOT_TRACK=1` in the daemon's environment turns it off
completely.

## Demo mode

**Try the demo** shows screens recorded in advance and bundled with the app. It connects to nothing.

## Children

The app is not directed at children and collects no data from anyone.

## Contact

Questions: open an issue at <https://github.com/leeguooooo/iphone-use/issues>.
