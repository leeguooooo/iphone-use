//! Simulated GPS position through `com.apple.dt.simulatelocation`, the
//! developer service Xcode's "Simulate Location" uses (it needs the Developer
//! Disk Image mounted, like every `com.apple.dt.*` service).
//!
//! The service reads one message per connection:
//! - set: u32 BE `0`, then the latitude and the longitude, each as a u32 BE
//!   byte length followed by the UTF-8 decimal string;
//! - clear (back to the real position): u32 BE `1`.

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub const SERVICE: &str = "com.apple.dt.simulatelocation";

const SET: u32 = 0;
const CLEAR: u32 = 1;

/// The `set` message for `lat`/`lon`, refusing out-of-range coordinates.
pub fn encode_set(lat: f64, lon: f64) -> Result<Vec<u8>> {
    validate(lat, lon)?;
    // f64's Display never uses an exponent and round-trips exactly.
    let (lat, lon) = (lat.to_string(), lon.to_string());
    let mut out = Vec::with_capacity(12 + lat.len() + lon.len());
    out.extend_from_slice(&SET.to_be_bytes());
    for text in [lat, lon] {
        out.extend_from_slice(&(text.len() as u32).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    Ok(out)
}

/// The `clear` message.
pub fn encode_clear() -> [u8; 4] {
    CLEAR.to_be_bytes()
}

pub fn validate(lat: f64, lon: f64) -> Result<()> {
    if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
        bail!("latitude {lat} is outside -90..90");
    }
    if !lon.is_finite() || !(-180.0..=180.0).contains(&lon) {
        bail!("longitude {lon} is outside -180..180");
    }
    Ok(())
}

/// Write one message and close the write side, so the service sees it whole.
async fn send<S: AsyncWrite + Unpin>(stream: &mut S, message: &[u8]) -> Result<()> {
    stream
        .write_all(message)
        .await
        .context("write to the location service")?;
    stream.flush().await?;
    // Some transports report a reset on shutdown after the peer has read and
    // closed; the message is already out by then.
    let _ = stream.shutdown().await;
    Ok(())
}

/// Move the phone's reported position to `lat`, `lon` (decimal degrees).
pub async fn set(udid: &str, lat: f64, lon: f64) -> Result<()> {
    let message = encode_set(lat, lon)?;
    let mut stream = crate::lockdown::open_service(udid, SERVICE).await?;
    send(&mut stream, &message).await
}

/// Stop simulating; the phone goes back to its real position.
pub async fn clear(udid: &str) -> Result<()> {
    let mut stream = crate::lockdown::open_service(udid, SERVICE).await?;
    send(&mut stream, &encode_clear()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// `#[tokio::test]` expands to `::core::…`, which this crate's `core`
    /// dependency shadows; tests build their own runtime.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn set_is_a_zero_then_two_length_prefixed_decimal_strings() {
        let bytes = encode_set(35.6812, 139.7671).unwrap();
        let mut want = vec![0, 0, 0, 0, 0, 0, 0, 7];
        want.extend_from_slice(b"35.6812");
        want.extend_from_slice(&[0, 0, 0, 8]);
        want.extend_from_slice(b"139.7671");
        assert_eq!(bytes, want);
        let negative = encode_set(-33.8688, -0.0000001).unwrap();
        assert_eq!(&negative[8..16], b"-33.8688");
        assert_eq!(&negative[16..20], &10u32.to_be_bytes());
        assert_eq!(&negative[20..], b"-0.0000001", "no exponent notation");
    }

    #[test]
    fn clear_is_a_single_one() {
        assert_eq!(encode_clear(), [0, 0, 0, 1]);
    }

    #[test]
    fn out_of_range_or_non_finite_coordinates_are_refused() {
        assert!(encode_set(90.0, 180.0).is_ok());
        assert!(encode_set(-90.0, -180.0).is_ok());
        assert!(encode_set(90.01, 0.0).is_err());
        assert!(encode_set(0.0, -180.5).is_err());
        assert!(encode_set(f64::NAN, 0.0).is_err());
        assert!(encode_set(0.0, f64::INFINITY).is_err());
    }

    #[test]
    fn the_message_reaches_the_service_whole() {
        block_on(the_message_reaches_the_service_whole_body());
    }

    async fn the_message_reaches_the_service_whole_body() {
        let (mut host, mut device) = tokio::io::duplex(1024);
        let message = encode_set(1.5, 2.25).unwrap();
        send(&mut host, &message).await.unwrap();
        let mut received = Vec::new();
        device.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, message);

        let (mut host, mut device) = tokio::io::duplex(1024);
        send(&mut host, &encode_clear()).await.unwrap();
        let mut received = Vec::new();
        device.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, [0, 0, 0, 1]);
    }
}
