//! Gives a browser recording its length. A recording made in the browser (MediaRecorder) is a "live-style" WebM: the header
//! says nothing about the total length, so a player shows a wrong progress bar (the web version saw a 1 h 45 min meeting
//! shown as about a minute). The web version fixes it with GStreamer when it saves the file (`addWebmDuration` in
//! server/audio.ts). The desktop app has no GStreamer, so this does the same job in plain Rust: it adds the missing
//! `Duration` element to the file's Info section. The audio itself is not touched.
//!
//! The rule is the same as in the web version: the file is replaced only when the new one is complete. If anything looks
//! unusual (not a WebM, already has a length, has an index that would move, cut off in the middle of the header) the file
//! is left exactly as it was.
use std::fs;
use std::io::Write;
use std::path::Path;

// Element ids of the WebM (Matroska) format, as written in its specification.
const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const CUES: u32 = 0x1C53_BB6B;
const TAGS: u32 = 0x1254_C367;
const CHAPTERS: u32 = 0x1043_A770;
const ATTACHMENTS: u32 = 0x1941_A469;
const TIMECODE_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const CLUSTER_TIMECODE: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;

/// The elements that sit directly inside the Segment. A cluster whose size is unknown ends where one of these starts.
const TOP_LEVEL: [u32; 8] = [CLUSTER, CUES, TAGS, TRACKS, INFO, SEEK_HEAD, CHAPTERS, ATTACHMENTS];

/// What a browser's Opus recording uses for one packet when a block does not say how long it is (20 ms).
const DEFAULT_FRAME_NS: u64 = 20_000_000;
/// Matroska's default: one tick of every timestamp is one millisecond.
const DEFAULT_TIMECODE_SCALE: u64 = 1_000_000;

/// BLOCKING (call from `spawn_blocking`). Writes the length into the WebM at `path`. Returns true when the file was
/// rewritten, false when it was left as it was (for whatever reason).
pub fn add_duration(path: &Path) -> bool {
    let Ok(data) = fs::read(path) else { return false };
    let Some(patch) = patch_for(&data) else { return false };
    let fixed = path.with_extension("fix");
    let written = (|| -> std::io::Result<u64> {
        let mut out = fs::File::create(&fixed)?;
        out.write_all(&patch.head)?;
        out.write_all(&data[patch.rest_from..])?;
        out.flush()?;
        Ok(out.metadata()?.len())
    })();
    // The new file is the old one plus a few bytes. Anything else means something went wrong, so keep the original.
    match written {
        Ok(len) if len == (patch.head.len() + data.len() - patch.rest_from) as u64 && len > data.len() as u64 && fs::rename(&fixed, path).is_ok() => true,
        _ => {
            let _ = fs::remove_file(&fixed);
            false
        }
    }
}

/// The new beginning of the file (everything up to and including the new Duration element); the rest of the old file
/// follows from `rest_from`.
struct Patch {
    head: Vec<u8>,
    rest_from: usize,
}

// ----- reading the format -----

/// An element id at `at`: (the id, how many bytes it takes).
fn read_id(b: &[u8], at: usize) -> Option<(u32, usize)> {
    let len = b.get(at)?.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let id = b.get(at..at + len)?.iter().fold(0u32, |acc, byte| (acc << 8) | u32::from(*byte));
    Some((id, len))
}

/// An element size at `at`: (the size, None when it is "unknown", how many bytes it takes).
fn read_size(b: &[u8], at: usize) -> Option<(Option<u64>, usize)> {
    let first = *b.get(at)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mut value = u64::from(first) & ((1u64 << (8 - len)) - 1);
    for byte in b.get(at + 1..at + len)? {
        value = (value << 8) | u64::from(*byte);
    }
    let unknown = value == (1u64 << (7 * len)) - 1; // all bits set means "unknown"
    Some((if unknown { None } else { Some(value) }, len))
}

/// A size written in exactly `len` bytes (a size may be written longer than needed), or None if it does not fit.
fn write_size(value: u64, len: usize) -> Option<Vec<u8>> {
    if !(1..=8).contains(&len) || value >= (1u64 << (7 * len)) - 1 {
        return None;
    }
    let mut bytes = value.to_be_bytes()[8 - len..].to_vec();
    bytes[0] |= 0x80 >> (len - 1);
    Some(bytes)
}

/// An unsigned number stored in 1 to 8 bytes.
fn read_uint(b: &[u8]) -> Option<u64> {
    (!b.is_empty() && b.len() <= 8).then(|| b.iter().fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte)))
}

/// One element header inside `b[..end]`: (id, size, where its body starts). A size that is unknown is None.
fn element(b: &[u8], at: usize) -> Option<(u32, Option<u64>, usize)> {
    let (id, id_len) = read_id(b, at)?;
    let (size, size_len) = read_size(b, at + id_len)?;
    Some((id, size, at + id_len + size_len))
}

/// The time, in ticks, at which the audio in a (Simple)Block ends, given its cluster's start time.
fn block_end(body: &[u8], cluster_start: i64, block_ticks: Option<u64>, default_ticks: i64) -> Option<i64> {
    let (_, track_len) = read_size(body, 0)?; // the track number is written like a size
    let relative = i16::from_be_bytes([*body.get(track_len)?, *body.get(track_len + 1)?]);
    Some(cluster_start + i64::from(relative) + block_ticks.map_or(default_ticks, |t| t as i64))
}

/// Looks through one cluster and returns (where it ends, the latest time any block in it reaches).
/// A cluster of unknown size (a live recording) ends where the next top-level element starts.
fn scan_cluster(b: &[u8], body_start: usize, size: Option<u64>, scan_end: usize, default_ticks: i64) -> Option<(usize, i64)> {
    let end = match size {
        Some(s) => body_start.saturating_add(usize::try_from(s).ok()?).min(scan_end),
        None => scan_end,
    };
    let (mut pos, mut cluster_start, mut latest) = (body_start, 0i64, 0i64);
    while pos < end {
        let Some((id, child_size, child_body)) = element(b, pos) else { break }; // a cut-off file: use what is readable
        if size.is_none() && TOP_LEVEL.contains(&id) {
            return Some((pos, latest));
        }
        let child_end = child_body.checked_add(usize::try_from(child_size?).ok()?)?;
        if child_end > end {
            break; // cut off in the middle of an element: it does not count
        }
        let child = &b[child_body..child_end];
        match id {
            CLUSTER_TIMECODE => cluster_start = read_uint(child)? as i64,
            SIMPLE_BLOCK => latest = latest.max(block_end(child, cluster_start, None, default_ticks)?),
            BLOCK_GROUP => {
                let (mut block, mut ticks, mut at) = (None, None, 0);
                while at < child.len() {
                    let (inner, inner_size, inner_body) = element(child, at)?;
                    let inner_end = inner_body.checked_add(usize::try_from(inner_size?).ok()?)?;
                    let inner_bytes = child.get(inner_body..inner_end)?;
                    match inner {
                        BLOCK => block = Some(inner_bytes),
                        BLOCK_DURATION => ticks = read_uint(inner_bytes),
                        _ => {}
                    }
                    at = inner_end;
                }
                if let Some(block) = block {
                    latest = latest.max(block_end(block, cluster_start, ticks, default_ticks)?);
                }
            }
            _ => {}
        }
        pos = child_end;
    }
    Some((end, latest))
}

/// Works out the change to make, or None when the file should stay as it is.
fn patch_for(b: &[u8]) -> Option<Patch> {
    // the EBML header, then the Segment that holds everything else
    let (id, size, body) = element(b, 0)?;
    if id != EBML_HEADER {
        return None;
    }
    let segment_at = body.checked_add(usize::try_from(size?).ok()?)?;
    let (id, segment_size, segment_body) = element(b, segment_at)?;
    if id != SEGMENT {
        return None;
    }
    let segment_size_at = segment_at + read_id(b, segment_at)?.1;
    let segment_size_len = segment_body - segment_size_at;
    let segment_end = match segment_size {
        Some(s) => segment_body.saturating_add(usize::try_from(s).ok()?).min(b.len()),
        None => b.len(),
    };

    // (where the Info element starts, where its body starts and ends, how long its size is written)
    let mut info: Option<(usize, usize, usize, usize)> = None;
    let mut scale = DEFAULT_TIMECODE_SCALE;
    let mut clusters: Vec<(usize, Option<u64>)> = Vec::new();
    let mut pos = segment_body;
    while pos < segment_end {
        let Some((id, size, child_body)) = element(b, pos) else { break };
        match id {
            // An index would point at places that move when bytes are added. A file that has one is already complete.
            SEEK_HEAD | CUES => return None,
            CLUSTER => {
                clusters.push((child_body, size));
                match size {
                    Some(s) => pos = child_body.checked_add(usize::try_from(s).ok()?)?,
                    None => pos = scan_cluster(b, child_body, None, segment_end, 0)?.0,
                }
                continue;
            }
            _ => {}
        }
        let child_end = child_body.checked_add(usize::try_from(size?).ok()?)?;
        if child_end > segment_end {
            return None;
        }
        if id == INFO {
            let size_len = child_body - pos - read_id(b, pos)?.1;
            info = Some((pos, child_body, child_end, size_len));
            let mut at = child_body;
            while at < child_end {
                let (inner, inner_size, inner_body) = element(b, at)?;
                let inner_end = inner_body.checked_add(usize::try_from(inner_size?).ok()?)?;
                match inner {
                    DURATION => return None, // it already has a length
                    TIMECODE_SCALE => scale = read_uint(b.get(inner_body..inner_end)?).filter(|s| *s > 0)?,
                    _ => {}
                }
                at = inner_end;
            }
        }
        pos = child_end;
    }
    let (info_at, info_body, info_end, info_size_len) = info?;

    // the end of the last block in any cluster is the length of the recording
    let default_ticks = (DEFAULT_FRAME_NS / scale) as i64;
    let mut length = 0i64;
    for (start, size) in clusters {
        length = length.max(scan_cluster(b, start, size, segment_end, default_ticks)?.1);
    }
    if length <= 0 {
        return None;
    }

    // The new Duration element: id, size 8, the length as a big-endian 64-bit float (in ticks of the timecode scale).
    let mut duration = vec![0x44, 0x89, 0x88];
    duration.extend_from_slice(&(length as f64).to_be_bytes());

    // Everything before the Info element, with the Segment's size (when it has one) grown to match.
    let mut head = b[..info_at].to_vec();
    if let Some(old) = segment_size {
        let new_size = write_size(old + duration.len() as u64, segment_size_len)?;
        head[segment_size_at..segment_size_at + segment_size_len].copy_from_slice(&new_size);
    }
    let info_id_len = read_id(b, info_at)?.1;
    head.extend_from_slice(&b[info_at..info_at + info_id_len]);
    head.extend(write_size((info_end - info_body + duration.len()) as u64, info_size_len)?);
    head.extend_from_slice(&b[info_body..info_end]);
    head.extend_from_slice(&duration);
    Some(Patch { head, rest_from: info_end })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One element: id bytes, a one-byte size (bodies here are all under 127 bytes), body.
    fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 127);
        [id, &[0x80 | body.len() as u8], body].concat()
    }

    /// A SimpleBlock for track 1 at `relative` ticks after the cluster start.
    fn simple_block(relative: i16) -> Vec<u8> {
        el(&[0xA3], &[&[0x81][..], &relative.to_be_bytes(), &[0x80], b"opus"].concat())
    }

    fn cluster(start: u8, relative: &[i16], unknown_size: bool) -> Vec<u8> {
        let mut body = el(&[0xE7], &[start]);
        for r in relative {
            body.extend(simple_block(*r));
        }
        if unknown_size {
            [&[0x1F, 0x43, 0xB6, 0x75][..], &[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], &body].concat()
        } else {
            el(&[0x1F, 0x43, 0xB6, 0x75], &body)
        }
    }

    /// A WebM as a browser writes it: a Segment of unknown size, an Info without Duration, Tracks, then clusters.
    fn live_webm(clusters: &[Vec<u8>], segment_size_known: bool) -> Vec<u8> {
        let info = el(&[0x15, 0x49, 0xA9, 0x66], &[el(&[0x2A, 0xD7, 0xB1], &[0x0F, 0x42, 0x40]), el(&[0x4D, 0x80], b"test")].concat());
        let tracks = el(&[0x16, 0x54, 0xAE, 0x6B], &el(&[0xAE], b"\xD7\x81\x01"));
        let mut inside = [info, tracks].concat();
        for c in clusters {
            inside.extend_from_slice(c);
        }
        let segment = if segment_size_known {
            // size written in 4 bytes, as a muxer that fills it in later would
            [&[0x18, 0x53, 0x80, 0x67][..], &[0x10, 0x00, 0x00, inside.len() as u8][..], &inside].concat()
        } else {
            [&[0x18, 0x53, 0x80, 0x67][..], &[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], &inside].concat()
        };
        [el(&[0x1A, 0x45, 0xDF, 0xA3], &el(&[0x42, 0x82], b"webm")), segment].concat()
    }

    fn fixed(data: &[u8]) -> Option<Vec<u8>> {
        patch_for(data).map(|p| [p.head, data[p.rest_from..].to_vec()].concat())
    }

    /// The length stored in the file, found the way a player finds it: inside Info, element 0x4489.
    fn stored_duration(data: &[u8]) -> Option<f64> {
        let at = data.windows(4).position(|w| w == [0x15, 0x49, 0xA9, 0x66])?;
        let (_, size, body) = element(data, at)?;
        let info = &data[body..body + size? as usize];
        let d = info.windows(3).position(|w| w == [0x44, 0x89, 0x88])?;
        Some(f64::from_be_bytes(info[d + 3..d + 11].try_into().ok()?))
    }

    #[test]
    fn a_live_recording_gets_its_length_from_the_last_block() {
        // clusters at 0 ms and 40 ms; the last block starts 20 ms into the second one, so the audio ends at 60 + 20 = 80 ms
        let live = live_webm(&[cluster(0, &[0, 20], true), cluster(40, &[0, 20], true)], false);
        let out = fixed(&live).expect("it can be fixed");
        assert_eq!(stored_duration(&out), Some(80.0));
        assert_eq!(out.len(), live.len() + 11);
        assert!(stored_duration(&live).is_none());
    }

    #[test]
    fn the_audio_bytes_are_not_touched() {
        let live = live_webm(&[cluster(0, &[0, 20, 40], false)], false);
        let out = fixed(&live).unwrap();
        let tail = |d: &[u8]| d[d.len() - 40..].to_vec();
        assert_eq!(tail(&out), tail(&live));
    }

    #[test]
    fn clusters_with_a_known_size_work_too_and_a_known_segment_size_is_grown() {
        let live = live_webm(&[cluster(0, &[0, 20], false), cluster(100, &[0], false)], true);
        let out = fixed(&live).unwrap();
        assert_eq!(stored_duration(&out), Some(120.0));
        // the segment's new size covers exactly what follows its header
        let segment_at = out.windows(4).position(|w| w == [0x18, 0x53, 0x80, 0x67]).unwrap();
        let (_, size, body) = element(&out, segment_at).unwrap();
        assert_eq!(body + size.unwrap() as usize, out.len());
    }

    #[test]
    fn a_file_that_already_has_a_length_or_an_index_is_left_alone() {
        let live = live_webm(&[cluster(0, &[0, 20], true)], false);
        let once = fixed(&live).unwrap();
        assert!(patch_for(&once).is_none(), "a second pass finds the length already there");
        let with_index = live_webm(&[el(&[0x1C, 0x53, 0xBB, 0x6B], b"x"), cluster(0, &[0], true)], false);
        assert!(patch_for(&with_index).is_none());
    }

    #[test]
    fn things_that_are_not_a_webm_are_left_alone() {
        assert!(patch_for(b"this is not a webm file at all").is_none());
        assert!(patch_for(b"").is_none());
        let live = live_webm(&[cluster(0, &[0, 20], true)], false);
        for cut in [3, 10, 20, 40] {
            assert!(patch_for(&live[..cut]).is_none(), "cut at {cut} bytes");
        }
    }

    #[test]
    fn a_recording_cut_off_in_the_middle_still_gets_a_length() {
        let live = live_webm(&[cluster(0, &[0, 20, 40], true)], false);
        let cut = &live[..live.len() - 6]; // the last block lost its tail
        let out = fixed(cut).expect("the readable part is enough");
        assert_eq!(stored_duration(&out), Some(40.0)); // blocks at 0 and 20 ms remain: the last ends at 40 ms
    }

    #[test]
    fn the_file_is_replaced_only_when_it_could_be_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("audio.webm");
        std::fs::write(&good, live_webm(&[cluster(0, &[0, 20], true)], false)).unwrap();
        assert!(add_duration(&good));
        assert_eq!(stored_duration(&std::fs::read(&good).unwrap()), Some(40.0));
        assert!(!add_duration(&good), "already done");

        let junk = dir.path().join("junk.webm");
        std::fs::write(&junk, b"not a webm").unwrap();
        assert!(!add_duration(&junk));
        assert_eq!(std::fs::read(&junk).unwrap(), b"not a webm");
        assert!(!dir.path().join("junk.fix").exists());
        assert!(!add_duration(&dir.path().join("missing.webm")));
    }
}
