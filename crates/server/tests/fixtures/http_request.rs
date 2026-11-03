// Read a complete HTTP fixture request before closing the connection.
// A header-only read can reset an in-flight JSON body and change delivery evidence.
fn read_mock_request(reader: &mut impl Read) -> std::io::Result<Option<String>> {
    read_mock_request_until(reader, || false)
}

fn read_mock_request_until(
    reader: &mut impl Read,
    stopped: impl Fn() -> bool,
) -> std::io::Result<Option<String>> {
    use std::io::{Error, ErrorKind};
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut bytes = Vec::new();
    let mut expected = None;
    loop {
        if stopped() {
            return Err(ErrorKind::Interrupted.into());
        }
        if std::time::Instant::now() >= deadline {
            return Err(ErrorKind::TimedOut.into());
        }
        let mut chunk = [0; 4096];
        let count = match reader.read(&mut chunk) {
            Ok(0) if bytes.is_empty() => return Ok(None),
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > 1024 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "mock request exceeds 1 MiB",
            ));
        }
        if expected.is_none() {
            if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                let header = std::str::from_utf8(&bytes[..end])
                    .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
                let mut length = 0usize;
                for line in header.lines() {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value
                                .trim()
                                .parse()
                                .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
                        }
                    }
                }
                let total = (end + 4)
                    .checked_add(length)
                    .filter(|size| *size <= 1024 * 1024)
                    .ok_or_else(|| {
                        Error::new(ErrorKind::InvalidData, "invalid mock content length")
                    })?;
                expected = Some(total);
            }
        }
        if let Some(total) = expected.filter(|total| bytes.len() >= *total) {
            bytes.truncate(total);
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|error| Error::new(ErrorKind::InvalidData, error));
        }
    }
}
