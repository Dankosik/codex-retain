use std::io::Write;

use crate::{cli::Format, error::AppError, stats::Stats};

pub(crate) fn summary(
    writer: &mut impl Write,
    stats: &Stats,
    format: Format,
) -> Result<(), AppError> {
    let mut bytes = match format {
        Format::Text => format!("bytes: {}\nlines: {}", stats.bytes, stats.lines).into_bytes(),
        // This record has two scalar counts; buffering it is bounded independently
        // of input size, and keeps serialization errors distinct from stdout I/O.
        Format::Json => serde_json::to_vec(stats)?,
    };
    bytes.push(b'\n');
    write_bytes(writer, &bytes)
}

pub(crate) fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> Result<(), AppError> {
    writer.write_all(bytes).map_err(AppError::Output)?;
    writer.flush().map_err(AppError::Output)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use super::{Format, Stats, summary};

    #[test]
    fn output_has_stable_bytes_and_field_order() {
        for (format, expected) in [
            (Format::Text, &b"bytes: 5\nlines: 2\n"[..]),
            (Format::Json, &b"{\"bytes\":5,\"lines\":2}\n"[..]),
        ] {
            let mut output = Vec::new();
            summary(&mut output, &Stats { bytes: 5, lines: 2 }, format).unwrap();
            assert_eq!(output, expected);
        }
    }

    struct ShortWriter {
        bytes: Vec<u8>,
        flush_error: Option<io::ErrorKind>,
    }

    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let size = bytes.len().min(2);
            self.bytes.extend_from_slice(&bytes[..size]);
            Ok(size)
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.flush_error {
                Some(kind) => Err(kind.into()),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn short_writes_are_completed_and_final_flush_is_observed() {
        let mut writer = ShortWriter {
            bytes: Vec::new(),
            flush_error: Some(io::ErrorKind::PermissionDenied),
        };
        let error = summary(&mut writer, &Stats::default(), Format::Json).unwrap_err();
        assert_eq!(writer.bytes, b"{\"bytes\":0,\"lines\":0}\n");
        assert!(!error.is_stdout_closed());
        writer.flush_error = Some(io::ErrorKind::BrokenPipe);
        assert!(
            summary(&mut writer, &Stats::default(), Format::Text)
                .unwrap_err()
                .is_stdout_closed()
        );
    }
}
