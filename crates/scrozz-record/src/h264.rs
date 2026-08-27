//! H.264 framing helpers shared by the VA-API encoder and pure tests.

use scrozz_core::{Error, Result};

/// Converts an Annex-B access unit into ISO-BMFF length-prefixed NAL units.
///
/// Already length-prefixed data is returned unchanged.
///
/// # Errors
///
/// Returns an error for an empty Annex-B access unit or a NAL larger than u32.
pub fn to_length_prefixed(sample: &[u8]) -> Result<Vec<u8>> {
    if !starts_with_start_code(sample) {
        return Ok(sample.to_vec());
    }
    let nalus = annex_b_nalus(sample);
    if nalus.is_empty() {
        return Err(Error::Platform(
            "VA-API produced an empty Annex-B access unit".into(),
        ));
    }
    let mut output = Vec::with_capacity(sample.len());
    for nalu in nalus {
        let length = u32::try_from(nalu.len())
            .map_err(|_| Error::Platform("H.264 NAL unit exceeds u32".into()))?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(nalu);
    }
    Ok(output)
}

/// Normalises FFmpeg's global header into an AVCDecoderConfigurationRecord.
///
/// FFmpeg encoders may expose either a ready `avcC` record or Annex-B SPS/PPS
/// NAL units depending on the driver.
///
/// # Errors
///
/// Returns an error when Annex-B data lacks an SPS or PPS.
pub fn decoder_configuration(extradata: &[u8]) -> Result<Vec<u8>> {
    if extradata.first() == Some(&1) {
        return Ok(extradata.to_vec());
    }

    let nalus = annex_b_nalus(extradata);
    let sps = nalus
        .iter()
        .copied()
        .find(|nalu| nalu.first().is_some_and(|byte| byte & 0x1f == 7))
        .ok_or_else(|| Error::Platform("VA-API global header contains no H.264 SPS".into()))?;
    let pps = nalus
        .iter()
        .copied()
        .find(|nalu| nalu.first().is_some_and(|byte| byte & 0x1f == 8))
        .ok_or_else(|| Error::Platform("VA-API global header contains no H.264 PPS".into()))?;
    if sps.len() < 4 {
        return Err(Error::Platform(
            "VA-API produced a truncated H.264 SPS".into(),
        ));
    }
    let sps_len =
        u16::try_from(sps.len()).map_err(|_| Error::Platform("H.264 SPS exceeds u16".into()))?;
    let pps_len =
        u16::try_from(pps.len()).map_err(|_| Error::Platform("H.264 PPS exceeds u16".into()))?;

    let mut output = Vec::with_capacity(sps.len() + pps.len() + 11);
    output.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xff, 0xe1]);
    output.extend_from_slice(&sps_len.to_be_bytes());
    output.extend_from_slice(sps);
    output.push(1);
    output.extend_from_slice(&pps_len.to_be_bytes());
    output.extend_from_slice(pps);
    Ok(output)
}

fn starts_with_start_code(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0, 0, 1]) || bytes.starts_with(&[0, 0, 0, 1])
}

fn annex_b_nalus(bytes: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut index = 0_usize;
    while index + 3 <= bytes.len() {
        let code_len = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        starts.push((index, index + code_len));
        index += code_len;
    }

    starts
        .iter()
        .enumerate()
        .filter_map(|(position, (_, start))| {
            let end = starts
                .get(position + 1)
                .map_or(bytes.len(), |(code_start, _)| *code_start);
            let mut end = end;
            while end > *start && bytes[end - 1] == 0 {
                end -= 1;
            }
            (*start < end).then_some(&bytes[*start..end])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{decoder_configuration, to_length_prefixed};

    #[test]
    fn converts_mixed_start_codes() {
        let sample = [0, 0, 0, 1, 0x65, 1, 2, 0, 0, 1, 0x41, 3];
        assert_eq!(
            to_length_prefixed(&sample).unwrap(),
            vec![0, 0, 0, 3, 0x65, 1, 2, 0, 0, 0, 2, 0x41, 3]
        );
    }

    #[test]
    fn builds_avcc_from_annex_b_sps_and_pps() {
        let header = [0, 0, 0, 1, 0x67, 0x64, 0, 0x1f, 0xaa, 0, 0, 1, 0x68, 0xee];
        let avcc = decoder_configuration(&header).unwrap();
        assert_eq!(&avcc[..6], &[1, 0x64, 0, 0x1f, 0xff, 0xe1]);
        assert_eq!(&avcc[6..8], &[0, 5]);
        assert_eq!(*avcc.last().unwrap(), 0xee);
    }

    #[test]
    fn preserves_ready_length_prefixed_forms() {
        let avcc = [1, 0x64, 0, 0x1f];
        assert_eq!(decoder_configuration(&avcc).unwrap(), avcc);
        let sample = [0, 0, 0, 2, 0x65, 0x88];
        assert_eq!(to_length_prefixed(&sample).unwrap(), sample);
    }
}
