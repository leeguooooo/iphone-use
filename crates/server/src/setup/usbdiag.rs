//! Why no iPhone is usable when usbmuxd lists none (or lists one this Mac
//! cannot use), read from the USB layer itself.
//!
//! An iPhone on a newer iOS than this Mac's device-support components shows
//! up in the IOUSB plane as `iPhone@… <class IOUSBHostDevice, …, !registered,
//! !matched, …>`: the cable and port work, but no driver claimed it, so
//! usbmuxd and devicectl list nothing and setup used to say only "no device".
//! Only asked on the no-device path: `ioreg` is cheap but not free.
//!
//! A phone this Mac already paired with that restarted and was not unlocked
//! since (Before First Unlock) is on the bus and claimed by its driver too,
//! but usbmuxd does not list it until someone enters the passcode once. That
//! is `locked_after_restart`, not an old Xcode: `xcode_too_old` needs positive
//! evidence, an iPhone no driver claimed.

use std::time::Duration;

use super::sys;

/// What the USB layer says about the phone(s) usbmuxd cannot use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnosis {
    /// No iPhone/iPad on the USB bus at all.
    NoneOnUsb,
    /// An iPhone is on the bus but no driver claimed it: this Mac's device
    /// support is too old for its iOS or not initialized.
    NotClaimed { name: String, matched: bool },
    /// A phone this Mac knows (the configured target, or one it holds a
    /// pairing record for) is on the bus and claimed, but usbmuxd does not
    /// list it: it restarted and nobody has unlocked it since.
    LockedAfterRestart { name: String, serial: String },
    /// An unknown phone is on the bus and claimed, but usbmuxd does not list
    /// it. No evidence points at device support; unlock and replug first.
    NotListed { name: String },
    /// usbmuxd lists the phone but has no pairing record for it.
    NotTrusted { serial: String },
}

impl Diagnosis {
    /// The setup blocker it is published as (all existing daemon blockers,
    /// so the daemon's hints already cover them).
    pub fn blocker(&self) -> &'static str {
        match self {
            Diagnosis::NoneOnUsb => "usb",
            Diagnosis::NotClaimed { .. } => "xcode_too_old",
            Diagnosis::LockedAfterRestart { .. } => "locked_after_restart",
            Diagnosis::NotListed { .. } => "usb",
            Diagnosis::NotTrusted { .. } => "trust",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Diagnosis::NoneOnUsb => "no iPhone on USB: check that the cable carries data (charge-only cables will not work), try another port, and unlock the phone".into(),
            Diagnosis::NotClaimed { name, matched } => format!(
                "an {name} is on USB{}, but this Mac's device-support components are too old for its iOS (or were never initialized): install or update Xcode, then run `xcodebuild -runFirstLaunch` (or open Xcode once)",
                if *matched { " and not listed by usbmuxd" } else { " but no driver claimed it" }
            ),
            Diagnosis::LockedAfterRestart { name, serial } => format!(
                "the {name} {serial} is on USB but usbmuxd does not list it: it restarted and has not been unlocked since (iOS keeps a phone in that state off USB until its passcode is entered once) — have a person enter the passcode on the phone; it connects on its own after that"
            ),
            Diagnosis::NotListed { name } => format!(
                "an {name} is on USB and claimed by its driver, but usbmuxd does not list it: unlock it (a phone that restarted is listed only after its first unlock) and replug the cable; if it stays unlisted, update Xcode and run `xcodebuild -runFirstLaunch`"
            ),
            Diagnosis::NotTrusted { serial } => format!(
                "iPhone {serial} is on USB but has not trusted this Mac: unlock the iPhone and tap Trust This Computer"
            ),
        }
    }
}

/// One iOS device entry in `ioreg -p IOUSB -w0` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbEntry {
    pub name: String,
    pub matched: bool,
    /// `USB Serial Number` (from `ioreg -l`), the phone's UDID without the
    /// dash. `None` from plain `ioreg` output.
    pub serial: Option<String>,
}

/// iPhone/iPad entries in `ioreg -p IOUSB -w0` text. A line looks like
/// `  | +-o iPhone@01100000  <class IOUSBHostDevice, id 0x…, !registered, !matched, active, …>`.
/// With `-l`, each entry's properties follow its line until the next
/// `+-o `; `"USB Serial Number" = "…"` there is its serial.
pub fn ios_entries(ioreg: &str) -> Vec<UsbEntry> {
    let mut entries: Vec<UsbEntry> = Vec::new();
    // Whether the properties being read belong to the last iOS entry.
    let mut in_entry = false;
    for line in ioreg.lines() {
        if line.contains("+-o ") {
            match entry_line(line) {
                Some(entry) => {
                    entries.push(entry);
                    in_entry = true;
                }
                None => in_entry = false,
            }
        } else if in_entry {
            if let Some(serial) = serial_property(line) {
                if let Some(last) = entries.last_mut() {
                    last.serial.get_or_insert(serial);
                }
            }
        }
    }
    entries
}

/// `"USB Serial Number" = "000081100002346211A0401E"` (or the
/// `kUSBSerialNumberString` spelling).
fn serial_property(line: &str) -> Option<String> {
    let (key, value) = line.trim().trim_start_matches('|').trim().split_once(" = ")?;
    if !matches!(key, "\"USB Serial Number\"" | "\"kUSBSerialNumberString\"") {
        return None;
    }
    let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
    (!value.is_empty()).then(|| value.to_string())
}

fn entry_line(line: &str) -> Option<UsbEntry> {
    let rest = &line[line.find("+-o ")? + 4..];
    let (name, attrs) = rest.split_once("  <")?;
    let name = name.split('@').next()?.trim();
    if !(name.starts_with("iPhone") || name.starts_with("iPad")) {
        return None;
    }
    let attrs = attrs.strip_prefix("class ")?;
    let mut fields = attrs.trim_end_matches('>').split(", ");
    if !fields.next()?.starts_with("IOUSBHostDevice") {
        return None;
    }
    let matched = fields.any(|field| field == "matched");
    Some(UsbEntry {
        name: name.to_string(),
        matched,
        serial: None,
    })
}

/// The same phone: usbmuxd and ioreg spell the serial with or without the
/// dash a newer UDID has.
fn same_serial(a: &str, b: &str) -> bool {
    let a = crate::usbmux::normalize_udid(a);
    !a.is_empty() && a == crate::usbmux::normalize_udid(b)
}

/// Decide from the USB plane, usbmuxd's USB serials, which of those serials
/// lack a pairing record, and the serials this Mac knows (the configured
/// target and the phones it holds a pairing record for).
pub fn diagnose(
    entries: &[UsbEntry],
    usb: &[String],
    untrusted: &[String],
    known: &[String],
) -> Option<Diagnosis> {
    if let Some(serial) = untrusted.first() {
        return Some(Diagnosis::NotTrusted {
            serial: serial.clone(),
        });
    }
    // Phones on the bus usbmuxd does not list. Without a serial (plain
    // ioreg) an entry counts as unlisted only when usbmuxd lists nothing.
    let unlisted: Vec<&UsbEntry> = entries
        .iter()
        .filter(|entry| match &entry.serial {
            Some(serial) => !usb.iter().any(|listed| same_serial(listed, serial)),
            None => usb.is_empty(),
        })
        .collect();
    let is_known = |entry: &&&UsbEntry| {
        entry
            .serial
            .as_deref()
            .is_some_and(|serial| known.iter().any(|k| same_serial(k, serial)))
    };
    // Positive evidence first: no driver claimed it (device support).
    if let Some(entry) = unlisted.iter().find(|entry| !entry.matched) {
        return Some(Diagnosis::NotClaimed {
            name: entry.name.clone(),
            matched: false,
        });
    }
    if let Some(entry) = unlisted.iter().find(is_known) {
        return Some(Diagnosis::LockedAfterRestart {
            name: entry.name.clone(),
            serial: entry.serial.clone().unwrap_or_default(),
        });
    }
    if let Some(entry) = unlisted.first() {
        return Some(Diagnosis::NotListed {
            name: entry.name.clone(),
        });
    }
    (usb.is_empty() && entries.is_empty()).then_some(Diagnosis::NoneOnUsb)
}

/// `ioreg -p IOUSB -l -w0` (with properties, for the serials), by absolute
/// path with a fixed argv (no shell).
fn read_ioreg() -> String {
    sys::stdout_of("/usr/sbin/ioreg", &["-p", "IOUSB", "-l", "-w0"])
}

/// usbmuxd holds a pairing record for this serial (tried as ioreg spells it
/// and with the dash a newer UDID has after its 8th character).
fn paired(serial: &str) -> bool {
    let mut ids = vec![serial.to_string()];
    if serial.len() == 24 && !serial.contains('-') {
        ids.push(format!("{}-{}", &serial[..8], &serial[8..]));
    }
    ids.iter().any(|id| {
        sys::block_on(async {
            tokio::time::timeout(Duration::from_secs(3), crate::usbmux::read_pair_record(id)).await
        })
        .is_ok_and(|read| read.is_ok())
    })
}

/// USB serials usbmuxd lists but holds no pairing record for.
pub fn untrusted(usb: &[String]) -> Vec<String> {
    usb.iter()
        .filter(|serial| {
            sys::block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(3),
                    crate::usbmux::read_pair_record(serial),
                )
                .await
            })
            .is_ok_and(|read| read.is_err())
        })
        .cloned()
        .collect()
}

/// The diagnosis for this Mac now, given usbmuxd's USB serials and the
/// configured target (empty when none). `None` when the USB layer has nothing
/// to add (every phone on the bus is listed and trusted).
pub fn probe(usb: &[String], target: &str) -> Option<Diagnosis> {
    let untrusted = untrusted(usb);
    if !untrusted.is_empty() {
        return diagnose(&[], usb, &untrusted, &[]);
    }
    let entries = ios_entries(&read_ioreg());
    let mut known: Vec<String> = Vec::new();
    if !target.is_empty() {
        known.push(target.to_string());
    }
    for serial in entries.iter().filter_map(|entry| entry.serial.as_deref()) {
        let listed = usb.iter().any(|listed| same_serial(listed, serial));
        if !listed && !known.iter().any(|k| same_serial(k, serial)) && paired(serial) {
            known.push(serial.to_string());
        }
    }
    diagnose(&entries, usb, &[], &known)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured 2026-10-08: iPhone 13 on iOS 27.0, Mac with only Xcode 26.6.
    const UNMATCHED: &str = "+-o Root  <class IORegistryEntry, id 0x100000100, retain 37>
  +-o AppleT8132USBXHCI@01000000  <class AppleT8132USBXHCI, id 0x100000508, registered, matched, active, busy 0 (59 ms), retain 35>
  | +-o iPhone@01100000  <class IOUSBHostDevice, id 0x1031d9618, !registered, !matched, active, busy 0, retain 13>
  +-o AppleT8132USBXHCI@00000000  <class AppleT8132USBXHCI, id 0x10000055e, registered, matched, active, busy 0 (235004 ms), retain 59>
  +-o AppleT8132USBXHCI@03000000  <class AppleT8132USBXHCI, id 0x10000053e, registered, matched, active, busy 0 (14 ms), retain 25>
";
    // Captured 2026-10-08: a working iPhone 17 Pro Max.
    const MATCHED: &str = "+-o Root  <class IORegistryEntry, id 0x100000100, retain 32>
  +-o AppleT8122USBXHCI@00000000  <class AppleT8122USBXHCI, id 0x10000039d, registered, matched, active, busy 0 (49846 ms), retain 124>
  | +-o iPhone@00100000  <class IOUSBHostDevice, id 0x1001832a4, registered, matched, active, busy 0 (237 ms), retain 67>
  +-o AppleT8122USBXHCI@01000000  <class AppleT8122USBXHCI, id 0x100000467, registered, matched, active, busy 0 (156686 ms), retain 110>
";
    const NO_PHONE: &str = "+-o Root  <class IORegistryEntry, id 0x100000100, retain 32>
  +-o AppleT8122USBXHCI@00000000  <class AppleT8122USBXHCI, id 0x10000039d, registered, matched, active, busy 0 (49846 ms), retain 124>
  | +-o USB2.0 Hub@00100000  <class IOUSBHostDevice, id 0x1001832a5, registered, matched, active, busy 0 (5 ms), retain 20>
  | | +-o Magic Keyboard@00110000  <class IOUSBHostDevice, id 0x1001832a6, registered, matched, active, busy 0 (5 ms), retain 20>
";

    #[test]
    fn an_unclaimed_iphone_is_found_as_not_matched() {
        assert_eq!(
            ios_entries(UNMATCHED),
            vec![UsbEntry {
                name: "iPhone".into(),
                matched: false,
                serial: None
            }]
        );
        let diagnosis = diagnose(&ios_entries(UNMATCHED), &[], &[], &[]).unwrap();
        assert_eq!(diagnosis.blocker(), "xcode_too_old");
        let message = diagnosis.message();
        assert!(message.contains("no driver claimed it"), "{message}");
        assert!(message.contains("xcodebuild -runFirstLaunch"), "{message}");
    }

    #[test]
    fn a_claimed_iphone_parses_as_matched() {
        assert_eq!(
            ios_entries(MATCHED),
            vec![UsbEntry {
                name: "iPhone".into(),
                matched: true,
                serial: None
            }]
        );
    }

    #[test]
    fn a_working_listed_phone_has_nothing_to_add() {
        let usb = vec!["00008150-000A1C3E0C42401C".to_string()];
        assert_eq!(diagnose(&ios_entries(MATCHED), &usb, &[], &[]), None);
    }

    #[test]
    fn an_unknown_claimed_phone_usbmuxd_does_not_list_is_not_blamed_on_xcode() {
        // No serial, no pairing record: nothing says device support is old.
        let diagnosis = diagnose(&ios_entries(MATCHED), &[], &[], &[]).unwrap();
        assert_eq!(diagnosis.blocker(), "usb");
        assert!(diagnosis.message().contains("does not list it"));
        assert!(diagnosis.message().contains("unlock"));
    }

    // Modeled on the 2026-10-10 report: an iPhone X (iOS 16.5, passcode set)
    // restarted remotely and sits in Before First Unlock. On the bus,
    // registered and matched (format of `ioreg -p IOUSB -l -w0` on the build
    // Mac), but usbmuxd lists only the other phone, an iPhone 13.
    const BFU_X: &str = "+-o Root  <class IORegistryEntry, id 0x100000100, retain 32>
  +-o AppleT8112USBXHCI@03000000  <class AppleT8112USBXHCI, id 0x10000052c, registered, matched, active, busy 0 (12 ms), retain 40>
    +-o iPhone@03100000  <class IOUSBHostDevice, id 0x1031eea06, registered, matched, active, busy 0 (190 ms), retain 189>
        {
          \"kUSBSerialNumberString\" = \"000081100002346211A0401E\"
          \"USB Serial Number\" = \"000081100002346211A0401E\"
          \"USB Product Name\" = \"iPhone\"
        }
    +-o iPhone@03200000  <class IOUSBHostDevice, id 0x1031eeb17, registered, matched, active, busy 0 (75 ms), retain 41>
        {
          \"kUSBSerialNumberString\" = \"4b1e3c2a9d8f7e6d5c4b3a291807f6e5d4c3b2a1\"
          \"USB Serial Number\" = \"4b1e3c2a9d8f7e6d5c4b3a291807f6e5d4c3b2a1\"
          \"USB Product Name\" = \"iPhone\"
        }
";
    const X_UDID: &str = "4b1e3c2a9d8f7e6d5c4b3a291807f6e5d4c3b2a1";
    const IPHONE13: &str = "00008110-0002346211A0401E";

    #[test]
    fn ioreg_properties_give_each_phone_its_serial() {
        let entries = ios_entries(BFU_X);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].serial.as_deref(), Some("000081100002346211A0401E"));
        assert_eq!(entries[1].serial.as_deref(), Some(X_UDID));
        assert!(entries.iter().all(|entry| entry.matched));
    }

    #[test]
    fn a_known_phone_off_usbmuxd_after_a_restart_is_locked_not_xcode_too_old() {
        let usb = vec![IPHONE13.to_string()];
        // The configured target (or a pairing record) makes it known.
        let diagnosis =
            diagnose(&ios_entries(BFU_X), &usb, &[], &[X_UDID.to_string()]).unwrap();
        assert_eq!(
            diagnosis,
            Diagnosis::LockedAfterRestart {
                name: "iPhone".into(),
                serial: X_UDID.into()
            }
        );
        assert_eq!(diagnosis.blocker(), "locked_after_restart");
        assert!(diagnosis.message().contains("passcode"));
        assert!(!diagnosis.message().contains("Xcode"));
        // The listed iPhone 13 (usbmuxd spells it with a dash) is never
        // the one diagnosed.
        let only_13 = &ios_entries(BFU_X)[..1];
        assert_eq!(diagnose(only_13, &usb, &[], &[]), None);
    }

    #[test]
    fn xcode_too_old_needs_an_unclaimed_phone() {
        // Even a known phone: no driver claimed it, so device support it is.
        let diagnosis = diagnose(&ios_entries(UNMATCHED), &[], &[], &["X".into()]).unwrap();
        assert_eq!(diagnosis.blocker(), "xcode_too_old");
        let usb = vec![IPHONE13.to_string()];
        let unknown = diagnose(&ios_entries(BFU_X), &usb, &[], &[]).unwrap();
        assert_eq!(unknown.blocker(), "usb", "unknown + claimed is not xcode_too_old");
    }

    #[test]
    fn no_iphone_on_the_bus_is_a_cable_or_port_problem() {
        assert!(ios_entries(NO_PHONE).is_empty());
        assert!(ios_entries("").is_empty());
        let diagnosis = diagnose(&ios_entries(NO_PHONE), &[], &[], &[]).unwrap();
        assert_eq!(diagnosis, Diagnosis::NoneOnUsb);
        assert_eq!(diagnosis.blocker(), "usb");
        assert!(diagnosis.message().contains("charge-only"));
    }

    #[test]
    fn a_listed_but_unpaired_phone_needs_trust() {
        let usb = vec!["00008110-001234".to_string()];
        let diagnosis = diagnose(&[], &usb, &usb, &[]).unwrap();
        assert_eq!(diagnosis.blocker(), "trust");
        assert!(diagnosis.message().contains("Trust This Computer"));
    }

    #[test]
    fn odd_lines_and_other_classes_are_ignored() {
        let text = "iPhone@1 <class IOUSBHostDevice, !matched>\n\
                    +-o iPhone@2\n\
                    +-o iPhone Hub@3  <class IOUSBHostInterface, id 0x1, !matched>\n\
                    +-o iPad@4  <class IOUSBHostDevice, id 0x2, registered, matched, active>\n";
        assert_eq!(
            ios_entries(text),
            vec![UsbEntry {
                name: "iPad".into(),
                matched: true,
                serial: None
            }]
        );
    }
}
