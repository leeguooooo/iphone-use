//! Why no iPhone is usable when usbmuxd lists none (or lists one this Mac
//! cannot use), read from the USB layer itself.
//!
//! An iPhone on a newer iOS than this Mac's device-support components shows
//! up in the IOUSB plane as `iPhone@… <class IOUSBHostDevice, …, !registered,
//! !matched, …>`: the cable and port work, but no driver claimed it, so
//! usbmuxd and devicectl list nothing and setup used to say only "no device".
//! Only asked on the no-device path: `ioreg` is cheap but not free.

use std::time::Duration;

use super::sys;

/// What the USB layer says about the phone(s) usbmuxd cannot use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnosis {
    /// No iPhone/iPad on the USB bus at all.
    NoneOnUsb,
    /// An iPhone is on the bus but no driver claimed it, or usbmuxd does not
    /// list it: this Mac's device support is too old or not initialized.
    NotClaimed { name: String, matched: bool },
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
}

/// iPhone/iPad entries in `ioreg -p IOUSB -w0` text. A line looks like
/// `  | +-o iPhone@01100000  <class IOUSBHostDevice, id 0x…, !registered, !matched, active, …>`.
pub fn ios_entries(ioreg: &str) -> Vec<UsbEntry> {
    ioreg
        .lines()
        .filter_map(|line| {
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
            })
        })
        .collect()
}

/// Decide from the USB plane, usbmuxd's USB serials, and which of those
/// serials lack a pairing record.
pub fn diagnose(entries: &[UsbEntry], usb: &[String], untrusted: &[String]) -> Option<Diagnosis> {
    if let Some(serial) = untrusted.first() {
        return Some(Diagnosis::NotTrusted {
            serial: serial.clone(),
        });
    }
    if !usb.is_empty() {
        return None;
    }
    match entries.iter().find(|e| !e.matched).or(entries.first()) {
        Some(entry) => Some(Diagnosis::NotClaimed {
            name: entry.name.clone(),
            matched: entry.matched,
        }),
        None => Some(Diagnosis::NoneOnUsb),
    }
}

/// `ioreg -p IOUSB -w0`, by absolute path with a fixed argv (no shell).
fn read_ioreg() -> String {
    sys::stdout_of("/usr/sbin/ioreg", &["-p", "IOUSB", "-w0"])
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

/// The diagnosis for this Mac now, given usbmuxd's USB serials. `None` when
/// the USB layer has nothing to add (a trusted phone is listed).
pub fn probe(usb: &[String]) -> Option<Diagnosis> {
    let untrusted = untrusted(usb);
    if !untrusted.is_empty() || !usb.is_empty() {
        return diagnose(&[], usb, &untrusted);
    }
    diagnose(&ios_entries(&read_ioreg()), usb, &[])
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
                matched: false
            }]
        );
        let diagnosis = diagnose(&ios_entries(UNMATCHED), &[], &[]).unwrap();
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
                matched: true
            }]
        );
    }

    #[test]
    fn a_working_listed_phone_has_nothing_to_add() {
        let usb = vec!["00008150-000A1C3E0C42401C".to_string()];
        assert_eq!(diagnose(&ios_entries(MATCHED), &usb, &[]), None);
    }

    #[test]
    fn a_matched_phone_usbmuxd_does_not_list_still_points_at_device_support() {
        let diagnosis = diagnose(&ios_entries(MATCHED), &[], &[]).unwrap();
        assert_eq!(diagnosis.blocker(), "xcode_too_old");
        assert!(diagnosis.message().contains("not listed by usbmuxd"));
    }

    #[test]
    fn no_iphone_on_the_bus_is_a_cable_or_port_problem() {
        assert!(ios_entries(NO_PHONE).is_empty());
        assert!(ios_entries("").is_empty());
        let diagnosis = diagnose(&ios_entries(NO_PHONE), &[], &[]).unwrap();
        assert_eq!(diagnosis, Diagnosis::NoneOnUsb);
        assert_eq!(diagnosis.blocker(), "usb");
        assert!(diagnosis.message().contains("charge-only"));
    }

    #[test]
    fn a_listed_but_unpaired_phone_needs_trust() {
        let usb = vec!["00008110-001234".to_string()];
        let diagnosis = diagnose(&[], &usb, &usb).unwrap();
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
                matched: true
            }]
        );
    }
}
