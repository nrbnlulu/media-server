//! A read-only fMP4 source with a virtual random-access index during preparation.
//!
//! `isofmp4mux` does not write an mfra while recording. Without one, qtdemux
//! scans and retains every preceding sample when seeking. Indexing just the moof
//! headers supplies sparse seek points so qtdemux can jump to a fragment.
//! After preroll, reads follow the original file as the recorder appends data.
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct IndexedFile {
    file: File,
    media_len: u64,
    footer: Vec<u8>,
    position: u64,
    following_growth: bool,
}

#[derive(Clone, Copy)]
struct Entry {
    time: u64,
    offset: u64,
    traf: u8,
    trun: u8,
}

impl IndexedFile {
    pub fn open(path: &Path, offset_ms: u64) -> Result<Self> {
        let mut file = File::open(path).context("open DVR recording")?;
        let snapshot_len = file.metadata()?.len();
        let (first_offset, timescales) = read_header(&mut file, snapshot_len)?;
        let first = next_fragment(&mut file, first_offset, snapshot_len)?
            .context("DVR recording has no complete indexed fragments")?;
        ensure!(
            first.entries.len() == 1,
            "DVR snapshot requires one video track"
        );
        let track = first.entries[0].0;
        let timescale = *timescales
            .get(&track)
            .context("missing DVR track timescale")?;
        let target = u64::try_from(offset_ms as u128 * timescale as u128 / 1000)?;

        // Find a complete fragment at the tail, excluding a moof/mdat currently
        // being written. Reads stay bounded even for a multi-day recording.
        let mut window = 64 * 1024u64;
        let last = loop {
            let start = snapshot_len.saturating_sub(window).max(first_offset);
            let mut cursor = start;
            let mut last = None;
            while let Some(fragment) = next_fragment(&mut file, cursor, snapshot_len)? {
                cursor = fragment.end;
                last = Some(fragment);
            }
            if let Some(last) = last {
                break last;
            }
            ensure!(
                start > first_offset,
                "DVR recording has no complete tail fragment"
            );
            window = window.saturating_mul(2);
        };

        // Fragment decode times are monotonic for the recorder's single video
        // track. Binary-search byte positions, then locate the next valid moof.
        // Only three seek points are needed: beginning, selected fragment, tail.
        let mut low = first.offset;
        let mut high = last.offset;
        let mut selected = first.clone();
        while low < high {
            let mid = low + (high - low) / 2;
            let fragment = next_fragment(&mut file, mid, last.end)?
                .context("missing DVR fragment during seek")?;
            let time = fragment.time(track)?;
            if time <= target {
                low = fragment.end.min(high);
                selected = fragment;
            } else {
                high = mid;
            }
        }
        // A midpoint can land inside a fragment; retain the preceding point.
        // If the target is at/after the final fragment, select it directly.
        if last.time(track)? <= target {
            selected = last.clone();
        }
        let mut points = BTreeMap::new();
        for fragment in [&first, &selected, &last] {
            points.insert(fragment.offset, fragment.entries[0].1);
        }
        let entries = BTreeMap::from([(track, points.into_values().collect())]);
        let footer = make_footer(entries)?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            file,
            media_len: last.end,
            footer,
            position: 0,
            following_growth: false,
        })
    }

    pub fn size(&self) -> u64 {
        self.media_len
            + if self.following_growth {
                0
            } else {
                self.footer.len() as u64
            }
    }

    /// Once qtdemux has loaded the virtual index and completed its seek, expose
    /// the original growing file. The index is retained by the demuxer; its
    /// virtual footer must not replace fragments subsequently appended here.
    pub fn follow_growth(&mut self) -> Result<()> {
        self.media_len = self.file.metadata()?.len();
        self.following_growth = true;
        Ok(())
    }

    pub fn seek(&mut self, position: u64) -> Result<()> {
        if self.following_growth {
            self.media_len = self.file.metadata()?.len();
        }
        ensure!(position <= self.size(), "seek beyond DVR snapshot");
        self.position = position;
        Ok(())
    }

    pub fn read(&mut self, length: usize) -> Result<Vec<u8>> {
        if self.following_growth {
            self.media_len = self.file.metadata()?.len();
        }
        let length = (length as u64).min(self.size() - self.position) as usize;
        let mut data = vec![0; length];
        let media_bytes = self
            .media_len
            .saturating_sub(self.position)
            .min(length as u64) as usize;
        if media_bytes > 0 {
            self.file.seek(SeekFrom::Start(self.position))?;
            self.file.read_exact(&mut data[..media_bytes])?;
        }
        if media_bytes < length {
            let footer_offset = (self.position + media_bytes as u64 - self.media_len) as usize;
            data[media_bytes..]
                .copy_from_slice(&self.footer[footer_offset..footer_offset + length - media_bytes]);
        }
        self.position += length as u64;
        Ok(data)
    }
}

#[derive(Clone)]
struct Fragment {
    offset: u64,
    end: u64,
    entries: Vec<(u32, Entry)>,
}

impl Fragment {
    fn time(&self, track: u32) -> Result<u64> {
        self.entries
            .iter()
            .find(|(id, _)| *id == track)
            .map(|(_, entry)| entry.time)
            .context("missing DVR video track in fragment")
    }
}

fn box_header(file: &mut File, offset: u64, limit: u64) -> Result<Option<([u8; 4], u64, u64)>> {
    if offset.saturating_add(8) > limit {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0; 16];
    file.read_exact(&mut header[..8])?;
    let size = u32::from_be_bytes(header[..4].try_into()?) as u64;
    let (size, header_len) = match size {
        0 => return Ok(None),
        1 => {
            if offset.saturating_add(16) > limit {
                return Ok(None);
            }
            file.read_exact(&mut header[8..])?;
            (u64::from_be_bytes(header[8..].try_into()?), 16)
        }
        size => (size, 8),
    };
    if size < header_len || offset.checked_add(size).is_none_or(|end| end > limit) {
        return Ok(None);
    }
    Ok(Some((header[4..8].try_into()?, size, header_len)))
}

fn read_header(file: &mut File, limit: u64) -> Result<(u64, BTreeMap<u32, u32>)> {
    let mut offset = 0;
    let mut timescales = BTreeMap::new();
    while let Some((kind, size, header_len)) = box_header(file, offset, limit)? {
        if &kind == b"moof" {
            return Ok((offset, timescales));
        }
        if &kind == b"moov" {
            ensure!(size <= 32 * 1024 * 1024, "DVR moov is too large");
            let mut body = vec![0; (size - header_len) as usize];
            file.read_exact(&mut body)?;
            for (kind, trak) in boxes(&body)? {
                if &kind != b"trak" {
                    continue;
                }
                let children = boxes(trak)?;
                let tkhd = children
                    .iter()
                    .find(|(k, _)| k == b"tkhd")
                    .context("missing DVR tkhd")?
                    .1;
                let track = word(tkhd, if word(tkhd, 0)? >> 24 == 1 { 20 } else { 12 })?;
                let mdia = children
                    .iter()
                    .find(|(k, _)| k == b"mdia")
                    .context("missing DVR mdia")?
                    .1;
                let children = boxes(mdia)?;
                let mdhd = children
                    .iter()
                    .find(|(k, _)| k == b"mdhd")
                    .context("missing DVR mdhd")?
                    .1;
                let scale = word(mdhd, if word(mdhd, 0)? >> 24 == 1 { 20 } else { 12 })?;
                ensure!(scale > 0, "invalid DVR timescale");
                timescales.insert(track, scale);
            }
        }
        offset += size;
    }
    bail!("DVR recording has no fragment header")
}

fn next_fragment(file: &mut File, start: u64, limit: u64) -> Result<Option<Fragment>> {
    let mut cursor = start;
    let mut buffer = vec![0; 64 * 1024];
    while cursor.saturating_add(8) <= limit {
        let length = (limit - cursor).min(buffer.len() as u64) as usize;
        file.seek(SeekFrom::Start(cursor))?;
        file.read_exact(&mut buffer[..length])?;
        // Searching media bytes is only a way to locate candidate headers. A
        // candidate must have a valid mfhd/traf and a complete following mdat.
        for position in 4..length.saturating_sub(3) {
            if &buffer[position..position + 4] != b"moof" {
                continue;
            }
            let offset = cursor + position as u64 - 4;
            let Some((_, size, header_len)) = box_header(file, offset, limit)? else {
                continue;
            };
            if size > 32 * 1024 * 1024 {
                continue;
            }
            let mut body = vec![0; (size - header_len) as usize];
            file.read_exact(&mut body)?;
            if body.get(4..8) != Some(b"mfhd") {
                continue;
            }
            let Ok(entries) = fragment_entries(&body, offset) else {
                continue;
            };
            if entries.is_empty() {
                continue;
            }
            let Some((kind, mdat_size, _)) = box_header(file, offset + size, limit)? else {
                continue;
            };
            if &kind == b"mdat" {
                return Ok(Some(Fragment {
                    offset,
                    end: offset + size + mdat_size,
                    entries,
                }));
            }
        }
        // Retain enough overlap for a size/type header split across reads.
        cursor += (length - 7) as u64;
    }
    Ok(None)
}

fn boxes(mut data: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut result = Vec::new();
    while !data.is_empty() {
        ensure!(data.len() >= 8, "truncated DVR MP4 box");
        let size = u32::from_be_bytes(data[..4].try_into()?) as usize;
        ensure!(size >= 8 && size <= data.len(), "invalid DVR MP4 child box");
        result.push((data[4..8].try_into()?, &data[8..size]));
        data = &data[size..];
    }
    Ok(result)
}

fn word(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        data.get(offset..offset + 4)
            .context("truncated DVR fragment field")?
            .try_into()?,
    ))
}

fn fragment_entries(body: &[u8], offset: u64) -> Result<Vec<(u32, Entry)>> {
    let mut entries = Vec::new();
    let mut traf_number = 0u8;
    for (kind, traf) in boxes(body)? {
        if &kind != b"traf" {
            continue;
        }
        traf_number = traf_number.checked_add(1).context("too many DVR tracks")?;
        let children = boxes(traf)?;
        let tfhd = children
            .iter()
            .find(|(k, _)| k == b"tfhd")
            .context("missing DVR tfhd")?
            .1;
        let tfdt = children
            .iter()
            .find(|(k, _)| k == b"tfdt")
            .context("missing DVR tfdt")?
            .1;
        let flags = word(tfhd, 0)? & 0x00ff_ffff;
        // The virtual footer preserves all original offsets, including absolute
        // base_data_offset values. No fragment or media bytes are rewritten.
        let track = word(tfhd, 4)?;
        let mut pos = 8;
        for (flag, size) in [(1, 8), (2, 4), (8, 4), (16, 4)] {
            if flags & flag != 0 {
                pos += size;
            }
        }
        let default_flags = if flags & 32 != 0 {
            Some(word(tfhd, pos)?)
        } else {
            None
        };
        let version = word(tfdt, 0)? >> 24;
        let time = match version {
            0 => word(tfdt, 4)? as u64,
            1 => ((word(tfdt, 4)? as u64) << 32) | word(tfdt, 8)? as u64,
            _ => bail!("unsupported DVR tfdt version"),
        };
        // The recorder starts each fragment with a random-access sample. Do not
        // fabricate a seek point for a fragment explicitly marked non-sync.
        let trun = children
            .iter()
            .find(|(k, _)| k == b"trun")
            .context("missing DVR trun")?
            .1;
        ensure!(word(trun, 4)? > 0, "empty DVR fragment");
        let flags = word(trun, 0)? & 0x00ff_ffff;
        let pos = 8 + if flags & 1 != 0 { 4 } else { 0 };
        let first_flags = if flags & 4 != 0 {
            Some(word(trun, pos)?)
        } else if flags & 0x400 != 0 {
            Some(word(
                trun,
                pos + if flags & 0x100 != 0 { 4 } else { 0 }
                    + if flags & 0x200 != 0 { 4 } else { 0 },
            )?)
        } else {
            default_flags
        };
        if first_flags.is_some_and(|flags| flags & 0x10000 == 0) {
            entries.push((
                track,
                Entry {
                    time,
                    offset,
                    traf: traf_number,
                    trun: 1,
                },
            ));
        }
    }
    Ok(entries)
}

fn make_footer(tracks: BTreeMap<u32, Vec<Entry>>) -> Result<Vec<u8>> {
    let mut footer = vec![0; 8];
    footer[4..8].copy_from_slice(b"mfra");
    for (track, entries) in tracks {
        let size = 24usize
            .checked_add(
                entries
                    .len()
                    .checked_mul(19)
                    .context("DVR index overflow")?,
            )
            .context("DVR index overflow")?;
        footer.extend_from_slice(&u32::try_from(size)?.to_be_bytes());
        footer.extend_from_slice(b"tfra");
        footer.extend_from_slice(&0x01000000u32.to_be_bytes()); // 64-bit timestamps and offsets
        footer.extend_from_slice(&track.to_be_bytes());
        footer.extend_from_slice(&0u32.to_be_bytes()); // one byte each for traf/trun/sample
        footer.extend_from_slice(&u32::try_from(entries.len())?.to_be_bytes());
        for entry in entries {
            footer.extend_from_slice(&entry.time.to_be_bytes());
            footer.extend_from_slice(&entry.offset.to_be_bytes());
            footer.extend_from_slice(&[entry.traf, entry.trun, 1]);
        }
    }
    let size = u32::try_from(footer.len() + 16)?;
    footer[..4].copy_from_slice(&size.to_be_bytes());
    footer.extend_from_slice(&16u32.to_be_bytes());
    footer.extend_from_slice(b"mfro");
    footer.extend_from_slice(&0u32.to_be_bytes());
    footer.extend_from_slice(&size.to_be_bytes());
    Ok(footer)
}

#[cfg(test)]
mod tests {
    use super::*;
    const RECORDING: &[u8] = include_bytes!("../../../tests/fixtures/dvr-fragmented.mp4");

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new(data: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("dvr-index-{}.mp4", uuid::Uuid::new_v4()));
            std::fs::write(&path, data).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn sparse_seek_points_cover_requested_positions() {
        let fixture = Fixture::new(RECORDING);
        for offset in [0, 500, 1500, 4000, 5500, 7000, 9500, 11000] {
            let mut indexed = IndexedFile::open(&fixture.0, offset).unwrap();
            let footer = boxes(&indexed.footer).unwrap()[0].1;
            let children = boxes(footer).unwrap();
            let tfra = children.iter().find(|(kind, _)| kind == b"tfra").unwrap().1;
            let count = word(tfra, 12).unwrap() as usize;
            assert!(count <= 3);
            let times: Vec<_> = (0..count)
                .map(|i| u64::from_be_bytes(tfra[16 + i * 19..24 + i * 19].try_into().unwrap()))
                .collect();
            // This fixture has a 3000 Hz timescale and one-second GOPs.
            let target = offset * 3;
            let selected = times
                .iter()
                .copied()
                .filter(|&t| t <= target)
                .max()
                .unwrap();
            assert!(target - selected < 3000, "offset {offset}: {times:?}");
            indexed.seek(0).unwrap();
            let virtual_file = indexed.read(indexed.size() as usize).unwrap();
            assert_eq!(
                &virtual_file[..indexed.media_len as usize],
                &RECORDING[..indexed.media_len as usize]
            );
            assert_eq!(&virtual_file[indexed.media_len as usize..], indexed.footer);
            assert_eq!(std::fs::read(&fixture.0).unwrap(), RECORDING);
        }
    }

    #[test]
    fn excludes_fragment_whose_media_is_still_being_written() {
        let fixture = Fixture::new(&RECORDING[..RECORDING.len() - 10]);
        let indexed = IndexedFile::open(&fixture.0, 9000).unwrap();
        assert!(indexed.media_len < (RECORDING.len() - 10) as u64);
        let mut reader = File::open(&fixture.0).unwrap();
        let fragment =
            next_fragment(&mut reader, indexed.media_len, RECORDING.len() as u64 - 10).unwrap();
        assert!(fragment.is_none());
    }

    #[test]
    fn reads_across_virtual_footer_boundary_and_checks_seeks() {
        let fixture = Fixture::new(RECORDING);
        let mut indexed = IndexedFile::open(&fixture.0, 7000).unwrap();
        let start = indexed.media_len - 4;
        indexed.seek(start).unwrap();
        let bytes = indexed.read(12).unwrap();
        assert_eq!(&bytes[..4], &RECORDING[start as usize..start as usize + 4]);
        assert_eq!(&bytes[4..], &indexed.footer[..8]);
        assert!(indexed.seek(indexed.size() + 1).is_err());
        indexed.seek(indexed.size()).unwrap();
        assert!(indexed.read(100).unwrap().is_empty());
    }

    #[test]
    fn rejects_truncated_and_invalid_headers_without_panicking() {
        for bytes in [
            &b""[..],
            &b"\0\0\0\x04moov"[..],
            &b"\0\0\0\x20moovshort"[..],
        ] {
            let fixture = Fixture::new(bytes);
            assert!(IndexedFile::open(&fixture.0, 0).is_err());
        }
        assert!(fragment_entries(b"\0\0\0\x04traf", 0).is_err());
    }
}
