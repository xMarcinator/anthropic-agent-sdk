//! Reading the CLI's output one line at a time with a bounded buffer: a line longer than the
//! limit is drained without being held in memory, and a malformed line never affects the next.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// One line of output, without its line ending.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// The output ended.
    End,
    /// A whole line.
    Whole(Vec<u8>),
    /// A line longer than the limit: its first `limit` bytes and its full length.
    Oversized {
        /// The line's first bytes
        head: Vec<u8>,
        /// The line's length
        len: usize,
    },
}

impl Line {
    /// The line's text (or the head of an oversized one), lossily decoded.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        match self {
            Self::End => None,
            Self::Whole(bytes) | Self::Oversized { head: bytes, .. } => {
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
        }
    }
}

/// Reads the next line from `reader`, keeping at most `limit` bytes of it. A last line without
/// a line ending counts as a line.
///
/// # Errors
/// The reader's I/O error.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Line> {
    let mut head = Vec::new();
    let mut len = 0usize;
    let mut read_any = false;
    let mut last = None;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if !read_any {
                return Ok(Line::End);
            }
            break;
        }
        read_any = true;
        let (chunk, ended) = match available.iter().position(|byte| *byte == b'\n') {
            Some(at) => (&available[..at], true),
            None => (available, false),
        };
        let room = limit.saturating_add(1).saturating_sub(head.len());
        head.extend_from_slice(&chunk[..chunk.len().min(room)]);
        len += chunk.len();
        if let Some(byte) = chunk.last() {
            last = Some(*byte);
        }
        let used = chunk.len() + usize::from(ended);
        reader.consume(used);
        if ended {
            break;
        }
    }
    // A `\r` before the `\n` is part of the line ending, not of the line.
    if last == Some(b'\r') {
        len -= 1;
        head.truncate(len);
    }
    head.truncate(limit);
    if len > limit {
        Ok(Line::Oversized { head, len })
    } else {
        Ok(Line::Whole(head))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn lines(input: &[u8], limit: usize) -> Vec<Line> {
        // A one-byte buffer splits every line across reads.
        let mut reader = tokio::io::BufReader::with_capacity(1, input);
        let mut lines = Vec::new();
        loop {
            let line = read_line(&mut reader, limit).await.unwrap();
            if line == Line::End {
                return lines;
            }
            lines.push(line);
        }
    }

    #[tokio::test]
    async fn lines_are_read_one_at_a_time_without_their_endings() {
        assert_eq!(
            lines(b"{\"a\":1}\r\n\n{\nlast", 64).await,
            [
                Line::Whole(b"{\"a\":1}".to_vec()),
                Line::Whole(Vec::new()),
                Line::Whole(b"{".to_vec()),
                Line::Whole(b"last".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn an_oversized_line_keeps_its_head_and_the_next_line_is_whole() {
        assert_eq!(
            lines(b"abcdefgh\nok\nabcd\r\n", 4).await,
            [
                Line::Oversized {
                    head: b"abcd".to_vec(),
                    len: 8,
                },
                Line::Whole(b"ok".to_vec()),
                Line::Whole(b"abcd".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn a_line_exactly_at_the_limit_is_whole() {
        assert_eq!(
            lines(b"abcd\nabcde\r\n", 4).await,
            [
                Line::Whole(b"abcd".to_vec()),
                Line::Oversized {
                    head: b"abcd".to_vec(),
                    len: 5,
                },
            ]
        );
    }
}
