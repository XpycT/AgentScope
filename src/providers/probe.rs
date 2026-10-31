//! Bounded metadata probes shared by the file-backed readers: a short prefix and a
//! short tail of one source, never the body between them.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// Bounded metadata probe (spec §5.1): a 4 KiB prefix (the header line is read to its
/// end, up to `HEADER_MAX`) and a 32 KiB tail. Nothing between them is read.
pub(crate) const HEAD_WINDOW: usize = 4 * 1024;
pub(crate) const HEADER_MAX: usize = 256 * 1024;
pub(crate) const TAIL_WINDOW: u64 = 32 * 1024;
/// Leading records applied to the probe parse (identity, cwd, title, first model).
pub(crate) const HEAD_RECORDS: usize = 16;

pub(crate) struct Probe {
    /// Complete leading lines with their file offsets.
    pub head: Vec<(u64, Vec<u8>)>,
    /// Complete trailing lines, oldest first.
    pub tail: Vec<Vec<u8>>,
    /// Bytes read from the file.
    pub bytes: usize,
}

fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Complete (newline-terminated) lines of `buf`, whose first byte is file offset
/// `base`. With `mid_line`, the bytes before the first newline are a fragment.
fn complete_lines(buf: &[u8], base: u64, mid_line: bool) -> Vec<(u64, Vec<u8>)> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut skip = mid_line;
    while let Some(at) = buf[start..].iter().position(|b| *b == b'\n') {
        let end = start + at;
        if skip {
            skip = false;
        } else {
            lines.push((base + start as u64, buf[start..end].to_vec()));
        }
        start = end + 1;
    }
    lines
}

pub(crate) fn probe_file(path: &Path, len: u64) -> io::Result<Probe> {
    let mut file = File::open(path)?;
    let mut head = vec![0u8; len.min(HEAD_WINDOW as u64) as usize];
    let got = read_full(&mut file, &mut head)?;
    head.truncate(got);
    let mut bytes = got;
    // A header longer than the window is read to its newline, up to the cap.
    let mut searched = 0usize;
    while !head[searched..].contains(&b'\n') && head.len() < HEADER_MAX && (head.len() as u64) < len
    {
        searched = head.len();
        let at = head.len();
        head.resize(at + 8192.min(HEADER_MAX - at), 0);
        let more = read_full(&mut file, &mut head[at..])?;
        head.truncate(at + more);
        bytes += more;
        if more == 0 {
            break;
        }
    }
    let head_lines = complete_lines(&head, 0, false);
    let mut tail = Vec::new();
    if (head.len() as u64) < len {
        let start = len.saturating_sub(TAIL_WINDOW).max(head.len() as u64);
        // One byte of look-behind tells whether the tail starts on a line boundary.
        let back = start.min(1);
        file.seek(SeekFrom::Start(start - back))?;
        let mut buf = vec![0u8; (len - start + back) as usize];
        let got = read_full(&mut file, &mut buf)?;
        buf.truncate(got);
        bytes += got;
        let mid_line = back == 1 && buf.first() != Some(&b'\n');
        let from = (back as usize).min(buf.len());
        tail = complete_lines(&buf[from..], start, mid_line)
            .into_iter()
            .map(|(_, line)| line)
            .collect();
    }
    crate::perf::bytes_read(bytes);
    Ok(Probe {
        head: head_lines,
        tail,
        bytes,
    })
}

pub(crate) fn probe_value(line: &[u8], max: usize) -> Option<Value> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() || line.len() > max {
        return None;
    }
    crate::perf::record_parsed();
    serde_json::from_slice(line).ok()
}
