//! Byte statistics for an arbitrary stream, independent of CLI and filesystem policy.

use std::io::{self, Read};

use serde::Serialize;

/// Input chunk size. Memory use does not grow with input or line length.
pub const BUFFER_BYTES: usize = 64 * 1024;

/// Counts bytes and LF newline terminators (the line convention used by `wc -l`).
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub bytes: u64,
    pub lines: u64,
}

/// Read through EOF without requiring UTF-8 or retaining complete lines.
///
/// An unterminated final fragment contributes bytes but no newline. Interrupted
/// reads are retried; other read failures are returned without a partial summary.
pub fn count(reader: &mut impl Read) -> io::Result<Stats> {
    let mut buffer = vec![0; BUFFER_BYTES];
    let mut stats = Stats::default();
    loop {
        let size = match reader.read(&mut buffer) {
            Ok(0) => return Ok(stats),
            Ok(size) => size,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        stats.bytes = stats
            .bytes
            .checked_add(size as u64)
            .ok_or_else(|| io::Error::other("byte count exceeds u64"))?;
        stats.lines += memchr::memchr_iter(b'\n', &buffer[..size]).count() as u64;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{self, Read};

    use super::{BUFFER_BYTES, Stats, count};

    #[test]
    fn byte_and_newline_semantics() {
        for (input, bytes, lines) in [
            (&b""[..], 0, 0),
            (&b"tail"[..], 4, 0),
            (&b"a\r\nb"[..], 4, 1),
            (&b"\n\n"[..], 2, 2),
            (&b"\xff\0\n\x80"[..], 4, 1),
        ] {
            assert_eq!(count(&mut &input[..]).unwrap(), Stats { bytes, lines });
        }
    }

    #[test]
    fn delimiters_across_chunks_and_a_very_long_line() {
        let mut input = vec![b'x'; 3 * BUFFER_BYTES + 7];
        input[BUFFER_BYTES - 1] = b'\r';
        input[BUFFER_BYTES] = b'\n';
        input[2 * BUFFER_BYTES] = b'\n';
        assert_eq!(
            count(&mut input.as_slice()).unwrap(),
            Stats {
                bytes: (3 * BUFFER_BYTES + 7) as u64,
                lines: 2,
            }
        );
    }

    struct ScriptedReader(VecDeque<io::Result<&'static [u8]>>);

    impl Read for ScriptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let bytes = self.0.pop_front().unwrap_or(Ok(b""))?;
            buffer[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    #[test]
    fn short_reads_and_interrupted_reads_are_not_eof() {
        let mut reader = ScriptedReader(VecDeque::from([
            Ok(&b"a"[..]),
            Err(io::ErrorKind::Interrupted.into()),
            Ok(&b"\n"[..]),
            Ok(&b"tail"[..]),
        ]));
        assert_eq!(count(&mut reader).unwrap(), Stats { bytes: 6, lines: 1 });
    }

    #[test]
    fn read_failure_after_progress_is_not_partial_success() {
        let mut reader = ScriptedReader(VecDeque::from([
            Ok(&b"line\n"[..]),
            Err(io::ErrorKind::BrokenPipe.into()),
        ]));
        assert_eq!(
            count(&mut reader).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
