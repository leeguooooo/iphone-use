//! Apple File Conduit: the file protocol behind `com.apple.afc` and the
//! services built on it (`com.apple.crashreportcopymobile` serves the crash
//! log folder this way). Only the read side is here: list a directory, stat
//! a path, open a file read-only, read it, close it.
//!
//! Every packet is a 40-byte header of five little-endian u64s (the magic
//! `CFA6LPAA`, the whole packet's length, the header-plus-arguments length,
//! a packet number, the operation), then the operation's arguments, then the
//! data. The device answers each request with one packet: `Status` (code 0
//! is success, anything else an [`AfcError`]), `Data`, or `FileOpenResult`.
//! Lists (directory entries, `key\0value\0` pairs) are NUL-separated strings.
//!
//! Layout as in libimobiledevice's afc.c and the idevice crate; the IO is
//! generic over the stream so tests drive it with `tokio::io::duplex`.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAGIC: &[u8; 8] = b"CFA6LPAA";
pub const HEADER_LEN: usize = 40;
/// Largest reply accepted; a read asks for at most [`READ_CHUNK`].
const MAX_PACKET: u64 = 8 << 20;
/// Bytes asked for per `FileRefRead`.
pub const READ_CHUNK: usize = 64 * 1024;
/// One request/reply exchange.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

pub const OP_STATUS: u64 = 0x01;
pub const OP_DATA: u64 = 0x02;
pub const OP_READ_DIR: u64 = 0x03;
pub const OP_GET_FILE_INFO: u64 = 0x0a;
pub const OP_FILE_OPEN: u64 = 0x0d;
pub const OP_FILE_OPEN_RESULT: u64 = 0x0e;
pub const OP_FILE_READ: u64 = 0x0f;
pub const OP_FILE_CLOSE: u64 = 0x14;

/// `FileRefOpen` mode `r` (O_RDONLY).
const MODE_READ_ONLY: u64 = 1;

fn path_argument(path: &str) -> Result<Vec<u8>> {
    if path.as_bytes().contains(&0) {
        bail!("AFC path contains an embedded NUL");
    }
    let mut argument = path.as_bytes().to_vec();
    argument.push(0);
    Ok(argument)
}

/// A non-zero AFC status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AfcError(pub u64);

impl AfcError {
    pub const NOT_FOUND: AfcError = AfcError(8);
    pub const IS_DIRECTORY: AfcError = AfcError(9);
    pub const PERMISSION_DENIED: AfcError = AfcError(10);

    pub fn name(self) -> &'static str {
        match self.0 {
            1 => "unknown error",
            2 => "operation header invalid",
            3 => "no resources",
            4 => "read error",
            5 => "write error",
            6 => "unknown packet type",
            7 => "invalid argument",
            8 => "object not found",
            9 => "object is a directory",
            10 => "permission denied",
            11 => "service not connected",
            12 => "operation timed out",
            13 => "too much data",
            14 => "end of data",
            15 => "operation not supported",
            16 => "object exists",
            17 => "object busy",
            18 => "no space left",
            19 => "operation would block",
            20 => "I/O error",
            21 => "operation interrupted",
            22 => "operation in progress",
            23 => "internal error",
            30 => "mux error",
            31 => "out of memory",
            32 => "not enough data",
            33 => "directory not empty",
            _ => "unrecognized status",
        }
    }
}

impl std::fmt::Display for AfcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AFC error {} ({})", self.0, self.name())
    }
}

impl std::error::Error for AfcError {}

/// Whether `error` is (or wraps) the AFC status `code`.
pub fn is_status(error: &anyhow::Error, code: AfcError) -> bool {
    error.downcast_ref::<AfcError>() == Some(&code)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub number: u64,
    pub operation: u64,
    pub arguments: Vec<u8>,
    pub data: Vec<u8>,
}

/// The wire bytes of one packet.
pub fn encode(number: u64, operation: u64, arguments: &[u8], data: &[u8]) -> Vec<u8> {
    let header_and_arguments = (HEADER_LEN + arguments.len()) as u64;
    let total = header_and_arguments + data.len() as u64;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&header_and_arguments.to_le_bytes());
    out.extend_from_slice(&number.to_le_bytes());
    out.extend_from_slice(&operation.to_le_bytes());
    out.extend_from_slice(arguments);
    out.extend_from_slice(data);
    out
}

/// `(total length, header-plus-arguments length, number, operation)`.
fn decode_header(header: &[u8; HEADER_LEN]) -> Result<(u64, u64, u64, u64)> {
    if &header[..8] != MAGIC {
        bail!("AFC packet has a bad magic {:02x?}", &header[..8]);
    }
    let word = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
    let (total, this, number, operation) = (word(8), word(16), word(24), word(32));
    if this < HEADER_LEN as u64 || total < this {
        bail!("AFC packet lengths are inconsistent (total {total}, header {this})");
    }
    if total > MAX_PACKET {
        bail!("AFC packet of {total} bytes is over the limit");
    }
    Ok((total, this, number, operation))
}

/// Read one packet off `stream`.
pub async fn read_packet<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Packet> {
    read_packet_limited(stream, None).await
}

async fn read_packet_limited<S: AsyncRead + Unpin>(
    stream: &mut S,
    read_limit: Option<u64>,
) -> Result<Packet> {
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).await?;
    let (total, this, number, operation) = decode_header(&header)?;
    if let Some(limit) = read_limit {
        let valid = match operation {
            OP_DATA => this == HEADER_LEN as u64 && total - this <= limit,
            OP_STATUS => this == HEADER_LEN as u64 + 8 && total == this,
            _ => false,
        };
        if !valid {
            bail!("AFC read reply exceeds requested bytes or has an invalid layout");
        }
    }
    let mut arguments = vec![0u8; (this as usize) - HEADER_LEN];
    stream.read_exact(&mut arguments).await?;
    let mut data = vec![0u8; (total - this) as usize];
    stream.read_exact(&mut data).await?;
    Ok(Packet {
        number,
        operation,
        arguments,
        data,
    })
}

/// A `Status` packet's code as a result: 0 is `Ok`.
fn status(packet: &Packet) -> Result<()> {
    if packet.operation != OP_STATUS {
        return Ok(());
    }
    let code = packet
        .arguments
        .get(..8)
        .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
        .context("AFC status packet without a code")?;
    if code == 0 {
        Ok(())
    } else {
        Err(AfcError(code).into())
    }
}

/// NUL-separated strings, empty ones dropped.
pub fn split_strings(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// `key\0value\0…` pairs, as `GetFileInfo` answers.
pub fn parse_pairs(bytes: &[u8]) -> HashMap<String, String> {
    let strings = split_strings(bytes);
    strings
        .chunks_exact(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub size: u64,
    /// `st_ifmt`: `S_IFREG`, `S_IFDIR`, `S_IFLNK`, …
    pub kind: String,
    /// `st_mtime` in nanoseconds since the Unix epoch.
    pub modified_ns: Option<u64>,
    /// `st_birthtime` in nanoseconds since the Unix epoch.
    pub created_ns: Option<u64>,
}

impl FileInfo {
    pub fn from_pairs(pairs: &HashMap<String, String>) -> Result<Self> {
        let number = |key: &str| pairs.get(key).and_then(|v| v.parse::<u64>().ok());
        Ok(Self {
            size: number("st_size").context("AFC file info has no st_size")?,
            kind: pairs.get("st_ifmt").cloned().unwrap_or_default(),
            modified_ns: number("st_mtime"),
            created_ns: number("st_birthtime"),
        })
    }

    pub fn is_dir(&self) -> bool {
        self.kind == "S_IFDIR"
    }

    pub fn is_file(&self) -> bool {
        self.kind == "S_IFREG"
    }

    /// `st_mtime` in whole seconds.
    pub fn modified_secs(&self) -> Option<u64> {
        self.modified_ns.map(|ns| ns / 1_000_000_000)
    }
}

/// An AFC session over a service stream.
pub struct AfcClient<S> {
    stream: S,
    next_number: u64,
    failed: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AfcClient<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            next_number: 0,
            failed: false,
        }
    }

    /// Send one request and read its reply; a non-zero status is an error.
    async fn request(&mut self, operation: u64, arguments: &[u8]) -> Result<Packet> {
        if self.failed {
            bail!("AFC stream is unusable after a failed exchange");
        }
        let number = self.next_number;
        self.next_number += 1;
        let packet = encode(number, operation, arguments, &[]);
        let read_limit = if operation == OP_FILE_READ {
            Some(u64::from_le_bytes(
                arguments
                    .get(8..16)
                    .context("AFC read request without byte count")?
                    .try_into()
                    .unwrap(),
            ))
        } else {
            None
        };
        let reply = tokio::time::timeout(EXCHANGE_TIMEOUT, async {
            self.stream.write_all(&packet).await?;
            self.stream.flush().await?;
            read_packet_limited(&mut self.stream, read_limit).await
        })
        .await
        .context("the AFC service did not answer in time")
        .and_then(|reply| reply);
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        status(&reply)?;
        Ok(reply)
    }

    /// Entry names in `path`, without `.` and `..`.
    pub async fn read_dir(&mut self, path: &str) -> Result<Vec<String>> {
        let reply = self.request(OP_READ_DIR, &path_argument(path)?).await?;
        Ok(split_strings(&reply.data)
            .into_iter()
            .filter(|name| name != "." && name != "..")
            .collect())
    }

    pub async fn file_info(&mut self, path: &str) -> Result<FileInfo> {
        let reply = self
            .request(OP_GET_FILE_INFO, &path_argument(path)?)
            .await?;
        FileInfo::from_pairs(&parse_pairs(&reply.data))
    }

    /// Open `path` read-only; the handle goes to [`Self::read`] and [`Self::close`].
    pub async fn open_read(&mut self, path: &str) -> Result<u64> {
        let mut arguments = MODE_READ_ONLY.to_le_bytes().to_vec();
        arguments.extend_from_slice(&path_argument(path)?);
        let reply = self.request(OP_FILE_OPEN, &arguments).await?;
        if reply.operation != OP_FILE_OPEN_RESULT {
            bail!("AFC FileOpen answered operation {:#x}", reply.operation);
        }
        reply
            .arguments
            .get(..8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
            .context("AFC FileOpen reply without a handle")
    }

    /// Up to `length` bytes from the handle; empty at the end of the file.
    pub async fn read(&mut self, handle: u64, length: usize) -> Result<Vec<u8>> {
        let mut arguments = handle.to_le_bytes().to_vec();
        arguments.extend_from_slice(&(length as u64).to_le_bytes());
        let reply = self.request(OP_FILE_READ, &arguments).await?;
        Ok(reply.data)
    }

    pub async fn close(&mut self, handle: u64) -> Result<()> {
        self.request(OP_FILE_CLOSE, &handle.to_le_bytes()).await?;
        Ok(())
    }

    /// The first `cap` bytes of `path`, and whether the file holds more. Close
    /// is attempted after a read error; a broken exchange retires the stream
    /// instead of sending another command across an unknown frame boundary.
    pub async fn read_file(&mut self, path: &str, cap: usize) -> Result<(Vec<u8>, bool)> {
        let handle = self.open_read(path).await?;
        let read = self.read_to_cap(handle, cap).await;
        let closed = self.close(handle).await;
        let (bytes, truncated) = read?;
        closed?;
        Ok((bytes, truncated))
    }

    /// Stream a file into caller-owned staging, writing at most `cap` bytes.
    /// One extra byte detects growth; it is never written. An error may leave
    /// an incomplete prefix, which the caller must discard rather than publish.
    /// Scope checks, source freshness, flushing and publication belong to the caller.
    pub async fn copy_file<W: AsyncWrite + Unpin>(
        &mut self,
        path: &str,
        cap: usize,
        output: &mut W,
    ) -> Result<usize> {
        let handle = self.open_read(path).await?;
        let copied = async {
            let mut written = 0;
            loop {
                let remaining = cap - written;
                let want = remaining.saturating_add(1).min(READ_CHUNK);
                let chunk = self.read(handle, want).await?;
                if chunk.is_empty() {
                    return Ok(written);
                }
                if chunk.len() > remaining {
                    bail!("AFC file exceeds copy byte limit");
                }
                output.write_all(&chunk).await?;
                written += chunk.len();
            }
        }
        .await;
        let closed = self.close(handle).await;
        let written = copied?;
        closed?;
        Ok(written)
    }

    /// Copy one regular file whose size and metadata remain unchanged across
    /// the read. Growth is bounded by the original declared size, not only the
    /// caller's maximum. This is a checkpoint, not proof a writer has finished.
    pub async fn copy_regular_file<W: AsyncWrite + Unpin>(
        &mut self,
        path: &str,
        maximum: usize,
        output: &mut W,
    ) -> Result<FileInfo> {
        let before = self.file_info(path).await?;
        let size = usize::try_from(before.size).context("AFC file size exceeds host range")?;
        if !before.is_file() || before.modified_ns.is_none() || size > maximum {
            bail!("AFC source must be a regular file with known modification time within the size limit");
        }
        let written = self.copy_file(path, size, output).await?;
        let after = self.file_info(path).await?;
        if written != size || before != after {
            bail!("AFC source changed during copy; discard staging");
        }
        Ok(after)
    }

    async fn read_to_cap(&mut self, handle: u64, cap: usize) -> Result<(Vec<u8>, bool)> {
        let mut out = Vec::new();
        // One byte past the cap tells a file of exactly `cap` from a longer one.
        while out.len() <= cap {
            let want = (cap - out.len()).saturating_add(1).min(READ_CHUNK);
            let chunk = self.read(handle, want).await?;
            if chunk.is_empty() {
                break;
            }
            out.extend_from_slice(&chunk);
        }
        let truncated = out.len() > cap;
        out.truncate(cap);
        Ok((out, truncated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    #[test]
    fn read_rejects_excess_headers_before_body_and_invalidates_stream() {
        block_on(async {
            for (operation, arguments, data) in [
                (OP_DATA, vec![], vec![0; 6]),
                (OP_DATA, vec![0; 8], vec![]),
                (OP_STATUS, vec![0; 8], vec![0]),
                (OP_FILE_OPEN_RESULT, vec![0; 8], vec![]),
            ] {
                let (host, mut peer) = tokio::io::duplex(1024);
                let device = tokio::spawn(async move {
                    let request = read_packet(&mut peer).await.unwrap();
                    assert_eq!(request.operation, OP_FILE_READ);
                    let reply = encode(request.number, operation, &arguments, &data);
                    // Retain the stream without sending any reply body. Rejection
                    // must follow the header, not EOF or a completed allocation.
                    peer.write_all(&reply[..HEADER_LEN]).await.unwrap();
                    peer
                });
                let mut client = AfcClient::new(host);
                let error = tokio::time::timeout(Duration::from_secs(1), client.read(1, 5))
                    .await
                    .expect("read waited for an excessive reply body")
                    .unwrap_err();
                assert!(error.to_string().contains("requested bytes"), "{error:#}");
                let error = client.read(1, 5).await.unwrap_err();
                assert!(error.to_string().contains("unusable"), "{error:#}");
                drop(device.await.unwrap());
            }
        });
    }

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
    fn packets_round_trip_through_the_wire_layout() {
        block_on(packets_round_trip_through_the_wire_layout_body());
    }

    async fn packets_round_trip_through_the_wire_layout_body() {
        let bytes = encode(7, OP_FILE_READ, &[1, 2, 3], b"data");
        assert_eq!(&bytes[..8], b"CFA6LPAA");
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 47);
        assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 43);
        assert_eq!(u64::from_le_bytes(bytes[24..32].try_into().unwrap()), 7);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 0x0f);
        // The magic is the u64 0x4141504c36414643 in little-endian.
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            0x4141_504c_3641_4643
        );
        let packet = read_packet(&mut &bytes[..]).await.unwrap();
        assert_eq!(
            packet,
            Packet {
                number: 7,
                operation: OP_FILE_READ,
                arguments: vec![1, 2, 3],
                data: b"data".to_vec(),
            }
        );
    }

    #[test]
    fn bad_magic_and_inconsistent_lengths_are_refused() {
        block_on(bad_magic_and_inconsistent_lengths_are_refused_body());
    }

    async fn bad_magic_and_inconsistent_lengths_are_refused_body() {
        let mut bytes = encode(0, OP_STATUS, &0u64.to_le_bytes(), &[]);
        bytes[0] = b'X';
        assert!(read_packet(&mut &bytes[..]).await.is_err());
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(MAGIC);
        header[8..16].copy_from_slice(&10u64.to_le_bytes()); // total < header
        header[16..24].copy_from_slice(&40u64.to_le_bytes());
        assert!(decode_header(&header).is_err());
        header[8..16].copy_from_slice(&(MAX_PACKET + 1).to_le_bytes());
        assert!(decode_header(&header).is_err());
    }

    #[test]
    fn status_packets_map_to_results() {
        let ok = Packet {
            number: 0,
            operation: OP_STATUS,
            arguments: 0u64.to_le_bytes().to_vec(),
            data: vec![],
        };
        assert!(status(&ok).is_ok());
        let missing = Packet {
            arguments: 8u64.to_le_bytes().to_vec(),
            ..ok.clone()
        };
        let error = status(&missing).unwrap_err();
        assert!(is_status(&error, AfcError::NOT_FOUND));
        assert!(error.to_string().contains("object not found"), "{error}");
        let short = Packet {
            arguments: vec![1],
            ..ok.clone()
        };
        assert!(status(&short).is_err());
        let data = Packet {
            operation: OP_DATA,
            arguments: vec![],
            ..ok
        };
        assert!(status(&data).is_ok(), "only Status packets carry a code");
    }

    #[test]
    fn file_info_pairs_parse() {
        let raw = b"st_size\x001234\x00st_blocks\x008\x00st_nlink\x001\x00st_ifmt\x00S_IFREG\x00\
                    st_mtime\x001760166062000000000\x00st_birthtime\x001760166000000000000\x00";
        let info = FileInfo::from_pairs(&parse_pairs(raw)).unwrap();
        assert_eq!(info.size, 1234);
        assert!(info.is_file() && !info.is_dir());
        assert_eq!(info.modified_secs(), Some(1_760_166_062));
        assert!(FileInfo::from_pairs(&parse_pairs(b"st_ifmt\x00S_IFDIR\x00")).is_err());
    }

    /// Answers each request the fake device reads with the next scripted reply.
    async fn fake_device(
        mut device: DuplexStream,
        replies: Vec<(u64, Vec<u8>, Vec<u8>)>,
    ) -> Vec<Packet> {
        let mut seen = Vec::new();
        for (operation, arguments, data) in replies {
            let request = read_packet(&mut device).await.unwrap();
            let number = request.number;
            seen.push(request);
            device
                .write_all(&encode(number, operation, &arguments, &data))
                .await
                .unwrap();
        }
        seen
    }

    #[test]
    fn read_dir_drops_dot_entries_and_numbers_requests() {
        block_on(read_dir_drops_dot_entries_and_numbers_requests_body());
    }

    async fn read_dir_drops_dot_entries_and_numbers_requests_body() {
        let (host, device) = tokio::io::duplex(1 << 16);
        let script = vec![
            (OP_DATA, vec![], b".\0..\0a.ips\0Retired\0".to_vec()),
            (OP_STATUS, 8u64.to_le_bytes().to_vec(), vec![]),
        ];
        let device = tokio::spawn(fake_device(device, script));
        let mut client = AfcClient::new(host);
        assert_eq!(client.read_dir("/").await.unwrap(), ["a.ips", "Retired"]);
        let error = client.read_dir("/nope").await.unwrap_err();
        assert!(is_status(&error, AfcError::NOT_FOUND), "{error:#}");
        let seen = device.await.unwrap();
        assert_eq!(seen[0].operation, OP_READ_DIR);
        assert_eq!(seen[0].arguments, b"/\0");
        assert_eq!((seen[0].number, seen[1].number), (0, 1));
    }

    #[test]
    fn read_file_opens_reads_in_chunks_and_closes() {
        block_on(read_file_opens_reads_in_chunks_and_closes_body());
    }

    async fn read_file_opens_reads_in_chunks_and_closes_body() {
        let (host, device) = tokio::io::duplex(1 << 20);
        let script = vec![
            (OP_FILE_OPEN_RESULT, 5u64.to_le_bytes().to_vec(), vec![]),
            (OP_DATA, vec![], b"hello ".to_vec()),
            (OP_DATA, vec![], b"world".to_vec()),
            (OP_DATA, vec![], vec![]),
            (OP_STATUS, 0u64.to_le_bytes().to_vec(), vec![]),
        ];
        let device = tokio::spawn(fake_device(device, script));
        let mut client = AfcClient::new(host);
        let (bytes, truncated) = client.read_file("/a.ips", 100).await.unwrap();
        assert_eq!(bytes, b"hello world");
        assert!(!truncated);
        let seen = device.await.unwrap();
        assert_eq!(seen[0].operation, OP_FILE_OPEN);
        assert_eq!(
            &seen[0].arguments[..8],
            &1u64.to_le_bytes(),
            "read-only mode"
        );
        assert_eq!(&seen[0].arguments[8..], b"/a.ips\0");
        assert_eq!(seen[1].operation, OP_FILE_READ);
        assert_eq!(&seen[1].arguments[..8], &5u64.to_le_bytes());
        assert_eq!(&seen[1].arguments[8..], &101u64.to_le_bytes());
        assert_eq!(seen[4].operation, OP_FILE_CLOSE);
        assert_eq!(seen[4].arguments, 5u64.to_le_bytes());
    }

    #[test]
    fn read_file_stops_at_the_cap_and_still_closes() {
        block_on(read_file_stops_at_the_cap_and_still_closes_body());
    }

    #[test]
    fn streaming_copy_bounds_writes_and_closes_at_each_boundary() {
        block_on(async {
            for (cap, chunks, expected, accepted) in [
                (0, vec![vec![]], vec![], true),
                (0, vec![b"x".to_vec()], vec![], false),
                (5, vec![b"12345".to_vec(), vec![]], b"12345".to_vec(), true),
                (
                    5,
                    vec![b"123".to_vec(), b"456".to_vec()],
                    b"123".to_vec(),
                    false,
                ),
                (usize::MAX, vec![b"x".to_vec(), vec![]], b"x".to_vec(), true),
            ] {
                let (host, peer) = tokio::io::duplex(1 << 20);
                let mut script = vec![(OP_FILE_OPEN_RESULT, 7u64.to_le_bytes().to_vec(), vec![])];
                for chunk in chunks {
                    script.push((OP_DATA, vec![], chunk));
                }
                script.push((OP_STATUS, 0u64.to_le_bytes().to_vec(), vec![]));
                let device = tokio::spawn(fake_device(peer, script));
                let mut client = AfcClient::new(host);
                let mut output = Vec::new();
                let result = client.copy_file("/saved.pdf", cap, &mut output).await;
                assert_eq!(result.is_ok(), accepted, "{result:?}");
                assert_eq!(output, expected);
                assert!(output.len() <= cap);
                if accepted {
                    assert_eq!(result.unwrap(), expected.len());
                }
                let requests = device.await.unwrap();
                assert_eq!(requests.last().unwrap().operation, OP_FILE_CLOSE);
                assert!(requests
                    .iter()
                    .filter(|request| request.operation == OP_FILE_READ)
                    .all(|request| u64::from_le_bytes(
                        request.arguments[8..16].try_into().unwrap()
                    ) <= READ_CHUNK as u64));
            }
        });
    }

    #[test]
    fn streaming_output_error_still_closes_the_remote_handle() {
        block_on(async {
            let (host, peer) = tokio::io::duplex(1024);
            let device = tokio::spawn(fake_device(
                peer,
                vec![
                    (OP_FILE_OPEN_RESULT, 7u64.to_le_bytes().to_vec(), vec![]),
                    (OP_DATA, vec![], b"saved".to_vec()),
                    (OP_STATUS, 0u64.to_le_bytes().to_vec(), vec![]),
                ],
            ));
            let mut client = AfcClient::new(host);
            let (mut output, reader) = tokio::io::duplex(1024);
            drop(reader);
            assert!(client
                .copy_file("/saved.pdf", 10, &mut output)
                .await
                .is_err());
            assert_eq!(
                device.await.unwrap().last().unwrap().operation,
                OP_FILE_CLOSE
            );
        });
    }

    #[test]
    fn regular_stream_copy_checks_metadata_and_actual_length() {
        block_on(async {
            let metadata = |size, modified| {
                format!("st_size\0{size}\0st_ifmt\0S_IFREG\0st_mtime\0{modified}\0").into_bytes()
            };
            for (payload, after, accepted) in [
                (b"saved".to_vec(), metadata(5, 123), true),
                (b"save".to_vec(), metadata(5, 123), false),
                (b"saved".to_vec(), metadata(5, 124), false),
                (b"saved".to_vec(), metadata(6, 123), false),
            ] {
                let (host, peer) = tokio::io::duplex(4096);
                let device = tokio::spawn(fake_device(
                    peer,
                    vec![
                        (OP_DATA, vec![], metadata(5, 123)),
                        (OP_FILE_OPEN_RESULT, 7u64.to_le_bytes().to_vec(), vec![]),
                        (OP_DATA, vec![], payload.clone()),
                        (OP_DATA, vec![], vec![]),
                        (OP_STATUS, 0u64.to_le_bytes().to_vec(), vec![]),
                        (OP_DATA, vec![], after),
                    ],
                ));
                let mut client = AfcClient::new(host);
                let mut output = Vec::new();
                let result = client
                    .copy_regular_file("/saved.pdf", 100, &mut output)
                    .await;
                assert_eq!(result.is_ok(), accepted, "{result:?}");
                assert_eq!(output, payload);
                let seen = device.await.unwrap();
                assert_eq!(seen[2].arguments[8..], 6u64.to_le_bytes());
            }
        });
    }

    #[test]
    fn regular_stream_copy_refuses_invalid_metadata_before_open() {
        block_on(async {
            for metadata in [
                b"st_size\x006\0st_ifmt\0S_IFREG\0st_mtime\x00123\0".to_vec(),
                b"st_size\x005\0st_ifmt\0S_IFLNK\0st_mtime\x00123\0".to_vec(),
                b"st_size\x005\0st_ifmt\0S_IFREG\0".to_vec(),
            ] {
                let (host, peer) = tokio::io::duplex(4096);
                let device = tokio::spawn(fake_device(peer, vec![(OP_DATA, vec![], metadata)]));
                let mut client = AfcClient::new(host);
                let mut output = Vec::new();
                assert!(client
                    .copy_regular_file("/saved.pdf", 5, &mut output)
                    .await
                    .is_err());
                assert!(output.is_empty());
                assert_eq!(device.await.unwrap().len(), 1);
            }
        });
    }

    async fn read_file_stops_at_the_cap_and_still_closes_body() {
        let (host, device) = tokio::io::duplex(1 << 20);
        let script = vec![
            (OP_FILE_OPEN_RESULT, 9u64.to_le_bytes().to_vec(), vec![]),
            (OP_DATA, vec![], b"01234".to_vec()),
            (OP_STATUS, 0u64.to_le_bytes().to_vec(), vec![]),
        ];
        let device = tokio::spawn(fake_device(device, script));
        let mut client = AfcClient::new(host);
        let (bytes, truncated) = client.read_file("/big.ips", 4).await.unwrap();
        assert_eq!(bytes, b"0123");
        assert!(truncated);
        let seen = device.await.unwrap();
        assert_eq!(seen[2].operation, OP_FILE_CLOSE);
    }

    #[test]
    fn a_failed_read_still_closes_the_handle() {
        block_on(a_failed_read_still_closes_the_handle_body());
    }

    async fn a_failed_read_still_closes_the_handle_body() {
        let (host, device) = tokio::io::duplex(1 << 20);
        let script = vec![
            (OP_FILE_OPEN_RESULT, 3u64.to_le_bytes().to_vec(), vec![]),
            (OP_STATUS, 4u64.to_le_bytes().to_vec(), vec![]),
            (OP_STATUS, 0u64.to_le_bytes().to_vec(), vec![]),
        ];
        let device = tokio::spawn(fake_device(device, script));
        let mut client = AfcClient::new(host);
        let error = client.read_file("/x.ips", 10).await.unwrap_err();
        assert!(is_status(&error, AfcError(4)), "{error:#}");
        let seen = device.await.unwrap();
        assert_eq!(seen[2].operation, OP_FILE_CLOSE);
    }

    #[test]
    fn file_info_and_open_errors_surface() {
        block_on(file_info_and_open_errors_surface_body());
    }

    #[test]
    fn path_requests_include_nul_terminators_on_the_wire() {
        block_on(async {
            let (host, peer) = tokio::io::duplex(4096);
            let device = tokio::spawn(fake_device(
                peer,
                vec![
                    (OP_DATA, vec![], b"saved.pdf\0".to_vec()),
                    (
                        OP_DATA,
                        vec![],
                        b"st_size\x005\0st_ifmt\0S_IFREG\0".to_vec(),
                    ),
                    (OP_FILE_OPEN_RESULT, 7u64.to_le_bytes().to_vec(), vec![]),
                ],
            ));
            let mut client = AfcClient::new(host);
            client.read_dir("/inbox").await.unwrap();
            client.file_info("/inbox/saved.pdf").await.unwrap();
            assert_eq!(client.open_read("/inbox/saved.pdf").await.unwrap(), 7);
            let seen = device.await.unwrap();
            assert_eq!(seen[0].arguments, b"/inbox\0");
            assert_eq!(seen[1].arguments, b"/inbox/saved.pdf\0");
            assert_eq!(&seen[2].arguments[8..], b"/inbox/saved.pdf\0");
        });
    }

    #[test]
    fn embedded_nul_paths_are_refused_without_sending_requests() {
        block_on(async {
            let (host, mut peer) = tokio::io::duplex(4096);
            let mut client = AfcClient::new(host);
            for error in [
                client.read_dir("/a\0b").await.unwrap_err(),
                client.file_info("/a\0b").await.unwrap_err(),
                client.open_read("/a\0b").await.unwrap_err(),
            ] {
                assert!(error.to_string().contains("embedded NUL"));
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(10), peer.read_u8())
                    .await
                    .is_err()
            );
        });
    }

    async fn file_info_and_open_errors_surface_body() {
        let (host, device) = tokio::io::duplex(1 << 16);
        let script = vec![
            (
                OP_DATA,
                vec![],
                b"st_size\x0042\x00st_ifmt\x00S_IFDIR\x00".to_vec(),
            ),
            (OP_STATUS, 10u64.to_le_bytes().to_vec(), vec![]),
        ];
        let device = tokio::spawn(fake_device(device, script));
        let mut client = AfcClient::new(host);
        let info = client.file_info("/Retired").await.unwrap();
        assert!(info.is_dir());
        assert_eq!(info.size, 42);
        let error = client.open_read("/secret").await.unwrap_err();
        assert!(is_status(&error, AfcError::PERMISSION_DENIED));
        let seen = device.await.unwrap();
        assert_eq!(seen[0].operation, OP_GET_FILE_INFO);
    }
}
