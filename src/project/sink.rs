//! Output sinks for projection: owned `Vec`, streaming `Write`, and counting.

use std::io::Write;

use crate::error::Error;

/// Byte sink used by the projector emitter.
///
/// - [`Vec<u8>`]: in-memory project
/// - [`WriteSink`]: stream to any [`Write`] without buffering the full output
/// - [`CountingSink`]: exact length / capacity planning without storing bytes
pub(crate) trait EmitOut {
    fn emit_byte(&mut self, b: u8) -> Result<(), Error>;
    fn emit_bytes(&mut self, s: &[u8]) -> Result<(), Error>;
    /// Last byte written (for pretty-print close-brace decisions).
    fn last_byte(&self) -> Option<u8>;
}

impl EmitOut for Vec<u8> {
    #[inline]
    fn emit_byte(&mut self, b: u8) -> Result<(), Error> {
        self.push(b);
        Ok(())
    }

    #[inline]
    fn emit_bytes(&mut self, s: &[u8]) -> Result<(), Error> {
        self.extend_from_slice(s);
        Ok(())
    }

    #[inline]
    fn last_byte(&self) -> Option<u8> {
        self.last().copied()
    }
}

/// Streams projected bytes to `W` (no full-output buffer).
pub struct WriteSink<W: Write> {
    w: W,
    last: Option<u8>,
    /// Total bytes successfully written.
    pub written: usize,
}

impl<W: Write> WriteSink<W> {
    pub fn new(w: W) -> Self {
        Self {
            w,
            last: None,
            written: 0,
        }
    }

    pub fn into_inner(self) -> W {
        self.w
    }
}

impl<W: Write> EmitOut for WriteSink<W> {
    fn emit_byte(&mut self, b: u8) -> Result<(), Error> {
        self.w.write_all(&[b]).map_err(|_| Error::InvalidJsonSyntax {
            pos: 0,
            msg: "I/O error while writing projected JSON",
        })?;
        self.last = Some(b);
        self.written += 1;
        Ok(())
    }

    fn emit_bytes(&mut self, s: &[u8]) -> Result<(), Error> {
        if s.is_empty() {
            return Ok(());
        }
        self.w.write_all(s).map_err(|_| Error::InvalidJsonSyntax {
            pos: 0,
            msg: "I/O error while writing projected JSON",
        })?;
        self.last = Some(s[s.len() - 1]);
        self.written += s.len();
        Ok(())
    }

    fn last_byte(&self) -> Option<u8> {
        self.last
    }
}

/// Counts bytes that would be written (exact projected length without retaining output).
#[derive(Debug, Default, Clone)]
pub struct CountingSink {
    pub len: usize,
    last: Option<u8>,
}

impl EmitOut for CountingSink {
    fn emit_byte(&mut self, b: u8) -> Result<(), Error> {
        self.len += 1;
        self.last = Some(b);
        Ok(())
    }

    fn emit_bytes(&mut self, s: &[u8]) -> Result<(), Error> {
        self.len += s.len();
        if let Some(&b) = s.last() {
            self.last = Some(b);
        }
        Ok(())
    }

    fn last_byte(&self) -> Option<u8> {
        self.last
    }
}
