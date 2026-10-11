//! A minimal MP4 muxer for one H.264 video track.
//!
//! Screen recordings ([`crate::recording`]) arrive as the live stream's
//! Annex-B access units ([`crate::video::H264Frame`]: start codes, SPS and PPS
//! in-band on keyframes). [`Mp4Writer`] turns them into a plain progressive
//! MP4 without a dependency:
//!
//! - `ftyp` (isom, avc1), then `mdat` written as the samples arrive (a 64-bit
//!   size patched at the end), then `moov` last. The file never needs the
//!   whole recording in memory, only one small entry per sample.
//! - Samples are AVCC: every NAL gets a 4-byte big-endian length instead of a
//!   start code. AUDs are dropped, and so are SPS/PPS identical to the ones in
//!   `avcC`. A *different* SPS (the stream switched size mid-recording) stays
//!   in-band in its sample: decoders follow it, the header just keeps the
//!   first size.
//! - Samples before the first IDR with SPS and PPS are dropped: nothing could
//!   decode them.
//! - Per-sample durations come from the timestamps (90 kHz media timescale);
//!   `stss` lists the IDR samples. All samples sit in one chunk, so
//!   `stsc`/`stco` have one entry each (`co64` if that offset ever passes
//!   4 GiB).

use std::io::{self, Seek, SeekFrom, Write};

/// Media timescale (ticks per second), the usual one for video.
pub const TIMESCALE: u32 = 90_000;
/// Movie (`mvhd`/`tkhd`) timescale.
const MOVIE_TIMESCALE: u32 = 1_000;
/// Duration of the last sample when nothing better is known (30 fps).
const DEFAULT_SAMPLE_MICROS: u64 = 33_333;

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

// ---------------------------------------------------------------------------
// Annex-B
// ---------------------------------------------------------------------------

/// NAL units (without start codes) of an Annex-B byte stream. Accepts 3- and
/// 4-byte start codes; trailing zero bytes of a unit are dropped.
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).map_or(data.len(), |next| next - 3);
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            units.push(&data[start..end]);
        }
    }
    units
}

fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |header| header & 0x1F)
}

/// Whether an Annex-B access unit holds an IDR slice.
pub fn is_idr(annexb: &[u8]) -> bool {
    split_annexb(annexb)
        .iter()
        .any(|unit| nal_type(unit) == NAL_IDR)
}

/// AVCC sample bytes (`[u32 BE length][NAL]`…) of the given NAL units.
pub fn to_length_prefixed<'a>(units: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in units {
        out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out
}

// ---------------------------------------------------------------------------
// SPS
// ---------------------------------------------------------------------------

/// What the MP4 header needs from an SPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsInfo {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    /// Display size, cropping applied.
    pub width: u32,
    pub height: u32,
}

/// Parse an SPS NAL unit (header byte included).
pub fn parse_sps(nal: &[u8]) -> Option<SpsInfo> {
    if nal_type(nal) != NAL_SPS || nal.len() < 4 {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader {
        data: &rbsp,
        bit: 0,
    };
    let profile_idc = r.bits(8)? as u8;
    let constraint_flags = r.bits(8)? as u8;
    let level_idc = r.bits(8)? as u8;
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bits(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.bits(1)? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for index in 0..lists {
                if r.bits(1)? == 1 {
                    skip_scaling_list(&mut r, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bits(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()? + 1;
    let height_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bits(1)?;
    if frame_mbs_only == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?; // direct_8x8_inference_flag
    let (mut crop_x, mut crop_y) = (0, 0);
    if r.bits(1)? == 1 {
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (unit_x, unit_y) = match chroma_format_idc {
            0 => (1, 2 - frame_mbs_only),
            1 => (2, 2 * (2 - frame_mbs_only)),
            2 => (2, 2 - frame_mbs_only),
            _ => (1, 2 - frame_mbs_only),
        };
        crop_x = (left + right) * unit_x;
        crop_y = (top + bottom) * unit_y;
    }
    let width = (width_mbs * 16).checked_sub(crop_x)?;
    let height = ((2 - frame_mbs_only) * height_map_units * 16).checked_sub(crop_y)?;
    Some(SpsInfo {
        profile_idc,
        constraint_flags,
        level_idc,
        width,
        height,
    })
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let mut last = 8i64;
    let mut next = 8i64;
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// RBSP from a NAL payload: drop each emulation-prevention byte (00 00 03).
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl BitReader<'_> {
    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value: u64 = 0;
        for _ in 0..count {
            let byte = *self.data.get(self.bit / 8)?;
            value = (value << 1) | u64::from((byte >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        u32::try_from(value).ok()
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        u32::try_from((1u64 << zeros) - 1 + u64::from(rest)).ok()
    }

    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

// ---------------------------------------------------------------------------
// Boxes
// ---------------------------------------------------------------------------

/// Append a box: `[u32 size][type][body]`.
fn mp4_box(out: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(kind);
    body(out);
    let size = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

/// Append a full box: a box whose body starts with version and flags.
fn full_box(
    out: &mut Vec<u8>,
    kind: &[u8; 4],
    version: u8,
    flags: u32,
    body: impl FnOnce(&mut Vec<u8>),
) {
    mp4_box(out, kind, |out| {
        out.push(version);
        out.extend_from_slice(&flags.to_be_bytes()[1..]);
        body(out);
    });
}

fn u16be(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn u32be(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn u64be(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// The identity transform of `mvhd`/`tkhd`.
fn unity_matrix(out: &mut Vec<u8>) {
    for value in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32be(out, value);
    }
}

/// `ftyp`: isom with avc1 among the compatible brands.
pub fn ftyp() -> Vec<u8> {
    let mut out = Vec::new();
    mp4_box(&mut out, b"ftyp", |out| {
        out.extend_from_slice(b"isom");
        u32be(out, 0x200);
        for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
            out.extend_from_slice(brand);
        }
    });
    out
}

/// `avcC` (AVCDecoderConfigurationRecord) for one SPS and one PPS, 4-byte
/// NAL lengths.
pub fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    mp4_box(&mut out, b"avcC", |out| {
        out.push(1); // configurationVersion
        out.push(sps.get(1).copied().unwrap_or(0)); // AVCProfileIndication
        out.push(sps.get(2).copied().unwrap_or(0)); // profile_compatibility
        out.push(sps.get(3).copied().unwrap_or(0)); // AVCLevelIndication
        out.push(0xFC | 3); // lengthSizeMinusOne = 3
        out.push(0xE0 | 1); // one SPS
        u16be(out, sps.len() as u16);
        out.extend_from_slice(sps);
        out.push(1); // one PPS
        u16be(out, pps.len() as u16);
        out.extend_from_slice(pps);
    });
    out
}

/// One recorded sample.
#[derive(Debug, Clone, Copy)]
struct Sample {
    size: u32,
    /// Presentation time in [`TIMESCALE`] ticks from the first sample.
    ticks: u64,
    sync: bool,
}

/// Everything `moov` describes.
struct Track<'a> {
    sps: &'a [u8],
    pps: &'a [u8],
    width: u32,
    height: u32,
    samples: &'a [Sample],
    /// Ticks of each sample (same length as `samples`).
    durations: &'a [u32],
    /// File offset of the first sample.
    chunk_offset: u64,
}

impl Track<'_> {
    fn duration_ticks(&self) -> u64 {
        self.durations.iter().map(|&d| u64::from(d)).sum()
    }
}

fn moov(track: &Track) -> Vec<u8> {
    let media_duration = track.duration_ticks();
    let movie_duration = media_duration * u64::from(MOVIE_TIMESCALE) / u64::from(TIMESCALE);
    let long = movie_duration > u64::from(u32::MAX) || media_duration > u64::from(u32::MAX);
    let mut out = Vec::new();
    mp4_box(&mut out, b"moov", |out| {
        full_box(out, b"mvhd", u8::from(long), 0, |out| {
            times(out, long, MOVIE_TIMESCALE, movie_duration);
            u32be(out, 0x0001_0000); // rate 1.0
            u16be(out, 0x0100); // volume 1.0
            out.extend_from_slice(&[0; 10]); // reserved
            unity_matrix(out);
            out.extend_from_slice(&[0; 24]); // pre_defined
            u32be(out, 2); // next_track_ID
        });
        mp4_box(out, b"trak", |out| {
            // flags: enabled | in movie | in preview
            full_box(out, b"tkhd", u8::from(long), 7, |out| {
                if long {
                    u64be(out, 0);
                    u64be(out, 0);
                    u32be(out, 1); // track_ID
                    u32be(out, 0);
                    u64be(out, movie_duration);
                } else {
                    u32be(out, 0);
                    u32be(out, 0);
                    u32be(out, 1); // track_ID
                    u32be(out, 0);
                    u32be(out, movie_duration as u32);
                }
                out.extend_from_slice(&[0; 8]); // reserved
                u16be(out, 0); // layer
                u16be(out, 0); // alternate_group
                u16be(out, 0); // volume (video)
                u16be(out, 0);
                unity_matrix(out);
                u32be(out, track.width << 16);
                u32be(out, track.height << 16);
            });
            mp4_box(out, b"mdia", |out| {
                full_box(out, b"mdhd", u8::from(long), 0, |out| {
                    times(out, long, TIMESCALE, media_duration);
                    u16be(out, 0x55C4); // language "und"
                    u16be(out, 0);
                });
                full_box(out, b"hdlr", 0, 0, |out| {
                    u32be(out, 0);
                    out.extend_from_slice(b"vide");
                    out.extend_from_slice(&[0; 12]);
                    out.extend_from_slice(b"VideoHandler\0");
                });
                mp4_box(out, b"minf", |out| {
                    full_box(out, b"vmhd", 0, 1, |out| out.extend_from_slice(&[0; 8]));
                    mp4_box(out, b"dinf", |out| {
                        full_box(out, b"dref", 0, 0, |out| {
                            u32be(out, 1);
                            // flags 1: the media is in this file.
                            full_box(out, b"url ", 0, 1, |_| {});
                        });
                    });
                    stbl(out, track);
                });
            });
        });
    });
    out
}

/// creation/modification time, timescale and duration of `mvhd`/`mdhd`.
fn times(out: &mut Vec<u8>, long: bool, timescale: u32, duration: u64) {
    if long {
        u64be(out, 0);
        u64be(out, 0);
        u32be(out, timescale);
        u64be(out, duration);
    } else {
        u32be(out, 0);
        u32be(out, 0);
        u32be(out, timescale);
        u32be(out, duration as u32);
    }
}

fn stbl(out: &mut Vec<u8>, track: &Track) {
    mp4_box(out, b"stbl", |out| {
        full_box(out, b"stsd", 0, 0, |out| {
            u32be(out, 1);
            mp4_box(out, b"avc1", |out| {
                out.extend_from_slice(&[0; 6]); // reserved
                u16be(out, 1); // data_reference_index
                out.extend_from_slice(&[0; 16]); // pre_defined, reserved
                u16be(out, track.width.min(0xFFFF) as u16);
                u16be(out, track.height.min(0xFFFF) as u16);
                u32be(out, 0x0048_0000); // 72 dpi
                u32be(out, 0x0048_0000);
                u32be(out, 0); // reserved
                u16be(out, 1); // frame_count
                out.extend_from_slice(&[0; 32]); // compressorname
                u16be(out, 0x0018); // depth
                u16be(out, 0xFFFF); // pre_defined = -1
                out.extend_from_slice(&avcc(track.sps, track.pps));
            });
        });
        // Run-length coded sample durations.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &duration in track.durations {
            match runs.last_mut() {
                Some((count, d)) if *d == duration => *count += 1,
                _ => runs.push((1, duration)),
            }
        }
        full_box(out, b"stts", 0, 0, |out| {
            u32be(out, runs.len() as u32);
            for (count, duration) in &runs {
                u32be(out, *count);
                u32be(out, *duration);
            }
        });
        let syncs: Vec<u32> = (1u32..)
            .zip(track.samples)
            .filter(|(_, s)| s.sync)
            .map(|(n, _)| n)
            .collect();
        if syncs.len() < track.samples.len() {
            // Without stss every sample is a sync sample.
            full_box(out, b"stss", 0, 0, |out| {
                u32be(out, syncs.len() as u32);
                for n in &syncs {
                    u32be(out, *n);
                }
            });
        }
        full_box(out, b"stsc", 0, 0, |out| {
            u32be(out, 1);
            u32be(out, 1); // first_chunk
            u32be(out, track.samples.len() as u32); // samples_per_chunk
            u32be(out, 1); // sample_description_index
        });
        full_box(out, b"stsz", 0, 0, |out| {
            u32be(out, 0); // sizes vary
            u32be(out, track.samples.len() as u32);
            for sample in track.samples {
                u32be(out, sample.size);
            }
        });
        if let Ok(offset) = u32::try_from(track.chunk_offset) {
            full_box(out, b"stco", 0, 0, |out| {
                u32be(out, 1);
                u32be(out, offset);
            });
        } else {
            full_box(out, b"co64", 0, 0, |out| {
                u32be(out, 1);
                u64be(out, track.chunk_offset);
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// What a finished file holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Mp4Summary {
    pub frames: u64,
    pub duration_micros: u64,
    /// Total file size.
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

/// Streams H.264 access units into an MP4 (see the module docs).
pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    /// Offset of the `mdat` header (its 64-bit size is patched at the end).
    mdat_at: u64,
    /// Bytes written so far (the file's current length).
    position: u64,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// Timestamp of the first sample, µs.
    first_pts: Option<u64>,
    samples: Vec<Sample>,
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Write `ftyp` and the `mdat` header.
    pub fn new(mut out: W) -> io::Result<Self> {
        let start = out.stream_position()?;
        let header = ftyp();
        out.write_all(&header)?;
        let mdat_at = start + header.len() as u64;
        // size = 1: the real size follows as a u64 (patched in `finish`).
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;
        Ok(Self {
            out,
            mdat_at,
            position: mdat_at + 16,
            sps: None,
            pps: None,
            first_pts: None,
            samples: Vec::new(),
        })
    }

    /// Samples written so far.
    pub fn frames(&self) -> u64 {
        self.samples.len() as u64
    }

    /// Bytes written so far (without the `moov` still to come).
    pub fn bytes(&self) -> u64 {
        self.position
    }

    /// Span from the first sample's timestamp to the newest one, µs.
    pub fn elapsed_micros(&self) -> u64 {
        self.samples
            .last()
            .map_or(0, |s| s.ticks * 1_000_000 / u64::from(TIMESCALE))
    }

    /// Append one Annex-B access unit shown at `pts_micros`. Returns whether
    /// it became a sample: units before the first IDR with parameter sets,
    /// and units with no picture data, are skipped. Timestamps should rise;
    /// one that does not is moved just after the previous sample.
    pub fn push(&mut self, annexb: &[u8], pts_micros: u64) -> io::Result<bool> {
        let units = split_annexb(annexb);
        let idr = units.iter().any(|unit| nal_type(unit) == NAL_IDR);
        if self.samples.is_empty() {
            if !idr {
                return Ok(false);
            }
            let sps = units.iter().find(|u| nal_type(u) == NAL_SPS);
            let pps = units.iter().find(|u| nal_type(u) == NAL_PPS);
            if self.sps.is_none() {
                if let (Some(sps), Some(pps)) = (sps, pps) {
                    if parse_sps(sps).is_some() {
                        self.sps = Some(sps.to_vec());
                        self.pps = Some(pps.to_vec());
                    }
                }
            }
            if self.sps.is_none() {
                return Ok(false);
            }
        }
        let (sps, pps) = (self.sps.as_deref(), self.pps.as_deref());
        let kept = units.into_iter().filter(|unit| match nal_type(unit) {
            NAL_AUD => false,
            NAL_SPS => Some(*unit) != sps,
            NAL_PPS => Some(*unit) != pps,
            _ => true,
        });
        let sample = to_length_prefixed(kept);
        if sample.is_empty() {
            return Ok(false);
        }
        let first = *self.first_pts.get_or_insert(pts_micros);
        let mut ticks =
            (pts_micros.saturating_sub(first) * u64::from(TIMESCALE) + 500_000) / 1_000_000;
        if let Some(last) = self.samples.last() {
            ticks = ticks.max(last.ticks + 1);
        }
        self.out.write_all(&sample)?;
        self.position += sample.len() as u64;
        self.samples.push(Sample {
            size: sample.len() as u32,
            ticks,
            sync: idr,
        });
        Ok(true)
    }

    /// Patch the `mdat` size and write `moov`. Fails when no sample was
    /// written (an empty MP4 plays nowhere).
    pub fn finish(mut self) -> io::Result<(W, Mp4Summary)> {
        let (Some(sps), Some(pps)) = (self.sps.take(), self.pps.take()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no decodable H.264 frames (no keyframe with SPS/PPS)",
            ));
        };
        let info = parse_sps(&sps).expect("checked when it was stored");
        let durations = sample_durations(&self.samples);
        let track = Track {
            sps: &sps,
            pps: &pps,
            width: info.width,
            height: info.height,
            samples: &self.samples,
            durations: &durations,
            chunk_offset: self.mdat_at + 16,
        };
        let duration_micros = track.duration_ticks() * 1_000_000 / u64::from(TIMESCALE);
        let header = moov(&track);
        let mdat_size = self.position - self.mdat_at;
        self.out.write_all(&header)?;
        let end = self.position + header.len() as u64;
        self.out.seek(SeekFrom::Start(self.mdat_at + 8))?;
        self.out.write_all(&mdat_size.to_be_bytes())?;
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        let summary = Mp4Summary {
            frames: self.samples.len() as u64,
            duration_micros,
            bytes: end,
            width: info.width,
            height: info.height,
        };
        Ok((self.out, summary))
    }
}

/// Per-sample durations from the timestamps; the last sample lasts as long
/// as the one before it (or a 30 fps frame when it is the only one).
fn sample_durations(samples: &[Sample]) -> Vec<u32> {
    let mut durations: Vec<u32> = samples
        .windows(2)
        .map(|pair| u32::try_from(pair[1].ticks - pair[0].ticks).unwrap_or(u32::MAX))
        .collect();
    let last = durations
        .last()
        .copied()
        .unwrap_or((DEFAULT_SAMPLE_MICROS * u64::from(TIMESCALE) / 1_000_000) as u32);
    if !samples.is_empty() {
        durations.push(last);
    }
    durations
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    // 320x240 Baseline SPS (as written by libx264 -profile baseline) and a PPS.
    const SPS: &[u8] = &[
        0x67, 0x42, 0xC0, 0x0D, 0xD9, 0x01, 0x41, 0xFB, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10,
        0x00, 0x00, 0x03, 0x03, 0xC0, 0xF1, 0x42, 0xA4, 0x80,
    ];
    const PPS: &[u8] = &[0x68, 0xCB, 0x83, 0xCB, 0x20];

    fn annexb(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(unit);
        }
        out
    }

    /// Child boxes `(type, body)` of a box body.
    fn children(data: &[u8]) -> Vec<(String, &[u8])> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            let mut size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            let kind = String::from_utf8_lossy(&data[i + 4..i + 8]).into_owned();
            let mut header = 8;
            if size == 1 {
                size = u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()) as usize;
                header = 16;
            }
            assert!(
                size >= header && i + size <= data.len(),
                "box {kind} overruns"
            );
            out.push((kind, &data[i + header..i + size]));
            i += size;
        }
        assert_eq!(i, data.len(), "trailing bytes after boxes");
        out
    }

    fn child<'a>(data: &'a [u8], path: &[&str]) -> &'a [u8] {
        let mut body = data;
        for name in path {
            // Full boxes with children: skip version/flags (+ entry count).
            let skip = match *name {
                "dref" | "stsd" => 8,
                _ => 0,
            };
            body = children(body)
                .into_iter()
                .find(|(kind, _)| kind == name)
                .unwrap_or_else(|| panic!("no {name}"))
                .1;
            if skip > 0 {
                body = &body[skip..];
            }
        }
        body
    }

    #[test]
    fn annexb_splits_on_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 0,
        ];
        let units = split_annexb(&data);
        assert_eq!(units, vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4]]);
        assert!(is_idr(&data));
        assert!(!is_idr(&[0, 0, 1, 0x41, 9]));
        assert_eq!(
            to_length_prefixed(units),
            vec![0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 2, 0x65, 4]
        );
    }

    #[test]
    fn the_sps_gives_the_cropped_size() {
        let info = parse_sps(SPS).unwrap();
        assert_eq!((info.width, info.height), (320, 240));
        assert_eq!((info.profile_idc, info.level_idc), (66, 13));
        assert!(parse_sps(PPS).is_none());
    }

    #[test]
    fn avcc_carries_profile_level_and_the_parameter_sets() {
        let record = avcc(SPS, PPS);
        let (kind, body) = children(&record).remove(0);
        assert_eq!(kind, "avcC");
        assert_eq!(&body[..6], &[1, 0x42, 0xC0, 0x0D, 0xFF, 0xE1]);
        assert_eq!(u16::from_be_bytes([body[6], body[7]]) as usize, SPS.len());
        assert_eq!(&body[8..8 + SPS.len()], SPS);
        let rest = &body[8 + SPS.len()..];
        assert_eq!(rest[0], 1);
        assert_eq!(&rest[3..], PPS);
    }

    #[test]
    fn the_file_layout_and_sample_tables_match_what_was_pushed() {
        let mut writer = Mp4Writer::new(Cursor::new(Vec::new())).unwrap();
        // A P frame before any keyframe is dropped.
        assert!(!writer.push(&annexb(&[&[0x41, 0xAA]]), 0).unwrap());
        let idr = annexb(&[&[0x09, 0xF0], SPS, PPS, &[0x65, 1, 2, 3]]);
        assert!(writer.push(&idr, 1_000_000).unwrap());
        assert!(writer.push(&annexb(&[&[0x41, 4, 5]]), 1_033_333).unwrap());
        assert!(writer.push(&annexb(&[&[0x41, 6]]), 1_066_666).unwrap());
        assert!(writer.push(&idr, 1_166_666).unwrap());
        let (cursor, summary) = writer.finish().unwrap();
        let file = cursor.into_inner();
        assert_eq!(summary.frames, 4);
        assert_eq!((summary.width, summary.height), (320, 240));
        assert_eq!(summary.bytes, file.len() as u64);
        // 3000 + 3000 + 9000 + last repeats 9000 = 24000 ticks.
        assert_eq!(summary.duration_micros, 266_666);

        let top = children(&file);
        let kinds: Vec<&str> = top.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["ftyp", "mdat", "moov"]);
        assert_eq!(&top[0].1[..4], b"isom");
        assert!(top[0].1.windows(4).any(|w| w == b"avc1"));
        // Samples are length-prefixed, AUD/SPS/PPS stripped.
        let mdat = top[1].1;
        assert_eq!(&mdat[..8], &[0, 0, 0, 4, 0x65, 1, 2, 3]);
        assert_eq!(mdat.len(), 8 + 7 + 6 + 8);

        let moov = top[2].1;
        let stbl = child(moov, &["trak", "mdia", "minf", "stbl"]);
        let avc1 = child(stbl, &["stsd", "avc1"]);
        assert_eq!(u16::from_be_bytes([avc1[24], avc1[25]]), 320);
        assert_eq!(u16::from_be_bytes([avc1[26], avc1[27]]), 240);
        let stts = child(stbl, &["stts"]);
        // 2 runs: (2 x 3000), (2 x 9000).
        assert_eq!(
            &stts[4..],
            &[0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0x0B, 0xB8, 0, 0, 0, 2, 0, 0, 0x23, 0x28]
        );
        let stss = child(stbl, &["stss"]);
        assert_eq!(&stss[4..], &[0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 4]);
        let stsz = child(stbl, &["stsz"]);
        assert_eq!(
            &stsz[8..],
            &[0, 0, 0, 4, 0, 0, 0, 8, 0, 0, 0, 7, 0, 0, 0, 6, 0, 0, 0, 8]
        );
        let stco = child(stbl, &["stco"]);
        let offset = u32::from_be_bytes(stco[8..12].try_into().unwrap()) as usize;
        assert_eq!(&file[offset..offset + 8], &mdat[..8]);
        let mdhd = child(moov, &["trak", "mdia", "mdhd"]);
        assert_eq!(
            u32::from_be_bytes(mdhd[12..16].try_into().unwrap()),
            TIMESCALE
        );
        assert_eq!(u32::from_be_bytes(mdhd[16..20].try_into().unwrap()), 24_000);
    }

    #[test]
    fn a_changed_sps_stays_in_band_and_bad_timestamps_still_rise() {
        let mut writer = Mp4Writer::new(Cursor::new(Vec::new())).unwrap();
        writer.push(&annexb(&[SPS, PPS, &[0x65, 1]]), 500).unwrap();
        let mut other = SPS.to_vec();
        other[3] = 0x1F;
        writer
            .push(&annexb(&[&other, PPS, &[0x65, 2]]), 100)
            .unwrap();
        let (cursor, summary) = writer.finish().unwrap();
        assert_eq!(summary.frames, 2);
        let file = cursor.into_inner();
        let mdat = children(&file)[1].1;
        assert_eq!(&mdat[..6], &[0, 0, 0, 2, 0x65, 1]);
        assert_eq!(&mdat[6..10], &(other.len() as u32).to_be_bytes());
    }

    #[test]
    fn nothing_decodable_is_an_error() {
        let mut writer = Mp4Writer::new(Cursor::new(Vec::new())).unwrap();
        writer.push(&annexb(&[&[0x41, 1]]), 0).unwrap();
        writer.push(&annexb(&[&[0x65, 1]]), 0).unwrap(); // IDR without SPS/PPS
        assert!(writer.finish().is_err());
    }

    fn tool_present(name: &str) -> bool {
        std::process::Command::new(name)
            .arg("-version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// Access units of an Annex-B elementary stream: a new one starts at each
    /// AUD, or at the first slice after another slice (first_mb == 0) when the
    /// stream has no AUDs.
    pub(crate) fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut units: Vec<Vec<u8>> = Vec::new();
        let mut current: Vec<u8> = Vec::new();
        let mut has_slice = false;
        for nal in split_annexb(stream) {
            let kind = nal_type(nal);
            let is_slice = kind == 1 || kind == NAL_IDR;
            // first_mb_in_slice == 0 is ue(0): the top bit of the byte after the header.
            let starts_picture = is_slice && nal.get(1).is_some_and(|b| b & 0x80 != 0);
            let boundary = matches!(kind, NAL_AUD | NAL_SPS | NAL_PPS | 6) || starts_picture;
            if has_slice && boundary {
                units.push(std::mem::take(&mut current));
                has_slice = false;
            }
            current.extend_from_slice(&[0, 0, 0, 1]);
            current.extend_from_slice(nal);
            has_slice |= is_slice;
        }
        if has_slice {
            units.push(current);
        }
        units
    }

    /// A 2 s 320x240 30 fps libx264 clip as Annex-B, or `None` without ffmpeg.
    pub(crate) fn ffmpeg_clip(dir: &std::path::Path) -> Option<Vec<u8>> {
        if !tool_present("ffmpeg") || !tool_present("ffprobe") {
            eprintln!("ffmpeg/ffprobe not on PATH: skipping");
            return None;
        }
        let clip = dir.join("clip.h264");
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
            .args(["-i", "testsrc=size=320x240:rate=30:duration=2"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "30"])
            .args(["-bsf:v", "h264_mp4toannexb", "-f", "h264"])
            .arg(&clip)
            .status()
            .ok()?;
        if !status.success() {
            eprintln!("ffmpeg could not encode with libx264: skipping");
            return None;
        }
        std::fs::read(clip).ok()
    }

    pub(crate) fn ffprobe(path: &std::path::Path) -> serde_json::Value {
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
            .args([
                "-show_entries",
                "stream=codec_name,width,height,nb_read_frames:format=duration",
            ])
            .args(["-of", "json"])
            .arg(path)
            .output()
            .expect("ffprobe runs");
        assert!(
            out.status.success(),
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("ffprobe json")
    }

    #[test]
    fn ffprobe_reads_a_muxed_libx264_clip() {
        let dir = tempfile::tempdir().unwrap();
        let Some(stream) = ffmpeg_clip(dir.path()) else {
            return;
        };
        let units = access_units(&stream);
        assert_eq!(units.len(), 60);
        let path = dir.path().join("out.mp4");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = Mp4Writer::new(io::BufWriter::new(file)).unwrap();
        for (index, unit) in units.iter().enumerate() {
            assert!(writer
                .push(unit, 7_000_000 + index as u64 * 1_000_000 / 30)
                .unwrap());
        }
        let (_, summary) = writer.finish().unwrap();
        assert_eq!(summary.frames, 60);
        assert_eq!(summary.bytes, std::fs::metadata(&path).unwrap().len());

        let probe = ffprobe(&path);
        let stream = &probe["streams"][0];
        assert_eq!(stream["codec_name"], "h264");
        assert_eq!(stream["width"], 320);
        assert_eq!(stream["height"], 240);
        assert_eq!(stream["nb_read_frames"], "60");
        let duration: f64 = probe["format"]["duration"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((duration - 2.0).abs() < 0.05, "duration {duration}");
    }
}
