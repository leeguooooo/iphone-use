//! Make H.264 decoders show every frame at once.
//!
//! VideoToolbox writes SPSs without the VUI `bitstream_restriction`, so a
//! decoder cannot know the stream never reorders frames and holds pictures
//! back until its DPB is full: up to 8 frames at 660×1434 and 12 at
//! 1320×2868 for the levels the phone picks (Chrome's WebCodecs decoder does
//! this). At a steady 30 fps that is ~270–400 ms of hidden latency; with the
//! runner skipping unchanged frames (one heartbeat a second on a still
//! screen) it leaves the picture many seconds stale.
//!
//! [`low_delay_access_unit`] rewrites the SPS of an Annex-B access unit to
//! declare `max_num_reorder_frames = 0` and `max_dec_frame_buffering =
//! max_num_ref_frames`, which is exactly what our encoders do (no frame
//! reordering). Everything else in the SPS is kept bit for bit.

/// Annex-B access unit with every SPS rewritten for low delay. `None` when
/// the unit has no SPS, or an SPS this parser does not understand (the unit
/// is then forwarded unchanged).
pub fn low_delay_access_unit(annexb: &[u8]) -> Option<Vec<u8>> {
    let units = split_annexb(annexb);
    if !units.iter().any(|unit| nal_type(unit) == Some(7)) {
        return None;
    }
    let mut out = Vec::with_capacity(annexb.len() + 8);
    for unit in units {
        out.extend_from_slice(&[0, 0, 0, 1]);
        if nal_type(unit) == Some(7) {
            out.extend_from_slice(&low_delay_sps(unit)?);
        } else {
            out.extend_from_slice(unit);
        }
    }
    Some(out)
}

fn nal_type(unit: &[u8]) -> Option<u8> {
    unit.first().map(|header| header & 0x1F)
}

/// NAL units (without start codes) of an Annex-B byte stream.
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
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
        // A 4-byte start code leaves one more zero in front of the next unit.
        while end > start && data[end - 1] == 0 && starts.get(index + 1).is_some() {
            end -= 1;
        }
        if end > start {
            units.push(&data[start..end]);
        }
    }
    units
}

/// One SPS NAL unit (header byte included) with a low-delay
/// `bitstream_restriction`.
pub fn low_delay_sps(nal: &[u8]) -> Option<Vec<u8>> {
    let (&header, payload) = nal.split_first()?;
    let rbsp = unescape(payload);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.bits(8)?;
    r.bits(16)?; // constraint flags, level_idc
    r.ue()?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bits(1)?;
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
            r.ue()?;
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
    let max_num_ref_frames = r.ue()?;
    r.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if r.bits(1)? == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?; // direct_8x8_inference_flag
    if r.bits(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }

    // Copy everything so far, then the VUI up to bitstream_restriction_flag.
    let vui_flag_at = r.position();
    let mut w = BitWriter::default();
    let vui_present = r.bits(1)? == 1;
    if vui_present {
        // aspect_ratio_info_present, then Extended_SAR (255) carries 32 bits.
        if r.bits(1)? == 1 && r.bits(8)? == 255 {
            r.bits(32)?;
        }
        if r.bits(1)? == 1 {
            r.bits(1)?;
        }
        if r.bits(1)? == 1 {
            r.bits(4)?;
            if r.bits(1)? == 1 {
                r.bits(24)?;
            }
        }
        if r.bits(1)? == 1 {
            r.ue()?;
            r.ue()?;
        }
        if r.bits(1)? == 1 {
            r.bits(32)?;
            r.bits(32)?;
            r.bits(1)?;
        }
        let nal_hrd = r.bits(1)? == 1;
        if nal_hrd {
            skip_hrd(&mut r)?;
        }
        let vcl_hrd = r.bits(1)? == 1;
        if vcl_hrd {
            skip_hrd(&mut r)?;
        }
        if nal_hrd || vcl_hrd {
            r.bits(1)?; // low_delay_hrd_flag
        }
        r.bits(1)?; // pic_struct_present_flag
        let restriction_at = r.position();
        w.copy_bits(&rbsp, restriction_at);
    } else {
        w.copy_bits(&rbsp, vui_flag_at);
        w.put(1, 1); // vui_parameters_present_flag
                     // aspect ratio, overscan, video signal, chroma loc, timing: absent;
                     // no NAL or VCL HRD; pic_struct_present_flag 0.
        w.put(0, 8);
    }
    w.put(1, 1); // bitstream_restriction_flag
    w.put(1, 1); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom (the spec's default)
    w.ue(1); // max_bits_per_mb_denom (the spec's default)
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(max_num_ref_frames.max(1)); // max_dec_frame_buffering
    w.put(1, 1); // rbsp_stop_one_bit
    let mut out = vec![header];
    out.extend_from_slice(&escape(&w.finish()));
    Some(out)
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

fn skip_hrd(r: &mut BitReader) -> Option<()> {
    let count = r.ue()? + 1;
    r.bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..count {
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
    }
    r.bits(20)?; // four 5-bit lengths
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

/// NAL payload from an RBSP: insert 03 wherever 00 00 is followed by 00–03.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
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

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn position(&self) -> usize {
        self.bit
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value: u64 = 0;
        for _ in 0..count {
            let byte = *self.data.get(self.bit / 8)?;
            let bit = (byte >> (7 - self.bit % 8)) & 1;
            value = (value << 1) | u64::from(bit);
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
        Some(((1u64 << zeros) - 1 + u64::from(rest)) as u32)
    }

    fn se(&mut self) -> Option<i64> {
        let value = i64::from(self.ue()?);
        Some(if value % 2 == 1 {
            (value + 1) / 2
        } else {
            -(value / 2)
        })
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bit: usize,
}

impl BitWriter {
    fn put(&mut self, value: u32, count: u32) {
        for index in (0..count).rev() {
            if self.bit.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> index) & 1 == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 1 << (7 - self.bit % 8);
            }
            self.bit += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let coded = u64::from(value) + 1;
        let length = 64 - coded.leading_zeros();
        self.put(0, length - 1);
        for index in (0..length).rev() {
            self.put(((coded >> index) & 1) as u32, 1);
        }
    }

    /// The first `count` bits of `source`, verbatim.
    fn copy_bits(&mut self, source: &[u8], count: usize) {
        for index in 0..count {
            let bit = (source[index / 8] >> (7 - index % 8)) & 1;
            self.put(u32::from(bit), 1);
        }
    }

    /// Pad to a byte boundary with zero bits (after the stop bit).
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// What a decoder learns from an SPS: picture size in macroblocks and
    /// the restriction, if any.
    #[derive(Debug, PartialEq)]
    struct Parsed {
        profile: u32,
        width_mbs: u32,
        height_mbs: u32,
        max_num_ref_frames: u32,
        reorder: Option<(u32, u32)>,
    }

    fn parse(nal: &[u8]) -> Parsed {
        let rbsp = unescape(&nal[1..]);
        let mut r = BitReader::new(&rbsp);
        let profile = r.bits(8).unwrap();
        r.bits(16).unwrap();
        r.ue().unwrap();
        if profile == 100 {
            assert_eq!(r.ue().unwrap(), 1, "4:2:0");
            r.ue().unwrap();
            r.ue().unwrap();
            r.bits(1).unwrap();
            assert_eq!(r.bits(1).unwrap(), 0, "no scaling matrix in these vectors");
        }
        r.ue().unwrap();
        match r.ue().unwrap() {
            0 => {
                r.ue().unwrap();
            }
            2 => {}
            other => panic!("pic_order_cnt_type {other} is not in these vectors"),
        }
        let max_num_ref_frames = r.ue().unwrap();
        r.bits(1).unwrap();
        let width_mbs = r.ue().unwrap() + 1;
        let height_mbs = r.ue().unwrap() + 1;
        assert_eq!(r.bits(1).unwrap(), 1, "frame_mbs_only");
        r.bits(1).unwrap();
        if r.bits(1).unwrap() == 1 {
            for _ in 0..4 {
                r.ue().unwrap();
            }
        }
        let mut reorder = None;
        if r.bits(1).unwrap() == 1 {
            for _ in 0..5 {
                assert_eq!(r.bits(1).unwrap(), 0, "our VUI carries nothing else");
            }
            assert_eq!(r.bits(1).unwrap(), 0, "nal hrd");
            assert_eq!(r.bits(1).unwrap(), 0, "vcl hrd");
            r.bits(1).unwrap();
            if r.bits(1).unwrap() == 1 {
                r.bits(1).unwrap();
                for _ in 0..4 {
                    r.ue().unwrap();
                }
                reorder = Some((r.ue().unwrap(), r.ue().unwrap()));
                assert_eq!(r.bits(1).unwrap(), 1, "rbsp stop bit");
            }
        }
        Parsed {
            profile,
            width_mbs,
            height_mbs,
            max_num_ref_frames,
            reorder,
        }
    }

    // Captured from the device runner on an iPhone 17 Pro Max.
    const PERFORMANCE_SPS: &str = "274d0020ab405405af3c88"; // Main, 660×1434
    const QUALITY_SPS: &str = "27640033ac5680530169e59d"; // High, 1320×2868

    #[test]
    fn the_phones_sps_gains_a_zero_reorder_restriction_and_keeps_its_size() {
        for (sps, profile, width, height) in
            [(PERFORMANCE_SPS, 77, 42, 90), (QUALITY_SPS, 100, 83, 180)]
        {
            let before = parse(&hex(sps));
            assert_eq!(
                (
                    before.profile,
                    before.width_mbs,
                    before.height_mbs,
                    before.reorder
                ),
                (profile, width, height, None),
                "{sps}"
            );
            let rewritten = low_delay_sps(&hex(sps)).unwrap();
            let after = parse(&rewritten);
            assert_eq!(after.profile, before.profile);
            assert_eq!((after.width_mbs, after.height_mbs), (width, height));
            assert_eq!(
                after.reorder,
                Some((0, before.max_num_ref_frames.max(1))),
                "{sps}"
            );
            // Same header byte and profile/level bytes: codec strings derived
            // from the SPS do not change.
            assert_eq!(&rewritten[..4], &hex(sps)[..4]);
        }
    }

    #[test]
    fn an_access_unit_keeps_its_other_nal_units_byte_for_byte() {
        let sps = hex(PERFORMANCE_SPS);
        let pps = hex("28ee3c80");
        let idr = [0x25, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01, 0xff];
        let mut unit = Vec::new();
        for nal in [&sps[..], &pps[..], &idr[..]] {
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(nal);
        }
        let out = low_delay_access_unit(&unit).unwrap();
        let units = split_annexb(&out);
        assert_eq!(units.len(), 3);
        assert_eq!(parse(units[0]).reorder, Some((0, 1)));
        assert_eq!(units[1], &pps[..]);
        assert_eq!(units[2], &idr[..]);
        // A delta frame (no SPS) is left alone.
        let mut delta = vec![0, 0, 0, 1];
        delta.extend_from_slice(&[0x21, 0x9a, 0x00]);
        assert!(low_delay_access_unit(&delta).is_none());
    }

    #[test]
    fn emulation_prevention_round_trips() {
        let rbsp = [0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x03];
        let escaped = escape(&rbsp);
        assert_eq!(
            escaped,
            [0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00, 0x05, 0x00, 0x00, 0x03, 0x03]
        );
        assert_eq!(unescape(&escaped), rbsp);
    }

    #[test]
    fn exp_golomb_round_trips() {
        let mut w = BitWriter::default();
        for value in [0, 1, 2, 3, 16, 255, 1000] {
            w.ue(value);
        }
        w.put(1, 1);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for value in [0, 1, 2, 3, 16, 255, 1000] {
            assert_eq!(r.ue(), Some(value));
        }
    }
}
