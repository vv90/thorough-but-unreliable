//! Incremental non-TTY framing. Frame lengths never determine allocations.
//! Channel 0 is the runtime's stdout alias; channel 3 is a runtime error.
use super::Error;
use crate::target::{CapturedOutput, CommandOutput};

enum Channel {
    Stdout,
    Stderr,
}
enum Frame {
    Header { bytes: [u8; 8], used: usize },
    Body { channel: Channel, remaining: u32 },
}
impl Frame {
    fn empty() -> Self {
        Self::Header {
            bytes: [0; 8],
            used: 0,
        }
    }
}

pub(super) struct Decoder {
    frame: Frame,
    output: CommandOutput,
    stdout_limit: usize,
    stderr_limit: usize,
}
impl Decoder {
    pub fn new(stdout_limit: usize, stderr_limit: usize) -> Self {
        Self {
            frame: Frame::empty(),
            output: CommandOutput::default(),
            stdout_limit,
            stderr_limit,
        }
    }
    pub fn feed(&mut self, mut input: &[u8]) -> Result<(), Error> {
        while !input.is_empty() {
            match &mut self.frame {
                Frame::Header { bytes, used } => {
                    let space = bytes
                        .get_mut(*used..)
                        .ok_or(Error::InvalidResponse("frame header offset"))?;
                    let count = space.len().min(input.len());
                    let (head, tail) = input
                        .split_at_checked(count)
                        .ok_or(Error::InvalidResponse("frame header length"))?;
                    let destination = space
                        .get_mut(..count)
                        .ok_or(Error::InvalidResponse("frame header length"))?;
                    destination.copy_from_slice(head);
                    *used = used
                        .checked_add(count)
                        .ok_or(Error::InvalidResponse("frame header overflow"))?;
                    input = tail;
                    if *used == bytes.len() {
                        let [channel, a, b, c, n0, n1, n2, n3] = *bytes;
                        if [a, b, c] != [0; 3] {
                            return Err(Error::InvalidResponse("nonzero reserved frame bytes"));
                        }
                        let channel = match channel {
                            0 | 1 => Channel::Stdout,
                            2 => Channel::Stderr,
                            3 => return Err(Error::RuntimeFailure),
                            _ => return Err(Error::InvalidResponse("unknown stream channel")),
                        };
                        let remaining = u32::from_be_bytes([n0, n1, n2, n3]);
                        self.frame = if remaining == 0 {
                            Frame::empty()
                        } else {
                            Frame::Body { channel, remaining }
                        };
                    }
                }
                Frame::Body { channel, remaining } => {
                    let count = input.len().min(
                        usize::try_from(*remaining)
                            .map_err(|_| Error::InvalidResponse("frame length overflow"))?,
                    );
                    let (head, tail) = input
                        .split_at_checked(count)
                        .ok_or(Error::InvalidResponse("frame body length"))?;
                    let (capture, limit) = match channel {
                        Channel::Stdout => (&mut self.output.stdout, self.stdout_limit),
                        Channel::Stderr => (&mut self.output.stderr, self.stderr_limit),
                    };
                    let keep = head.len().min(limit.saturating_sub(capture.bytes.len()));
                    let saved = head
                        .get(..keep)
                        .ok_or(Error::InvalidResponse("capture length"))?;
                    append(capture, saved)?;
                    capture.truncated |= keep < head.len();
                    *remaining = remaining
                        .checked_sub(
                            u32::try_from(count)
                                .map_err(|_| Error::InvalidResponse("frame length overflow"))?,
                        )
                        .ok_or(Error::InvalidResponse("frame length underflow"))?;
                    input = tail;
                    if *remaining == 0 {
                        self.frame = Frame::empty();
                    }
                }
            }
        }
        Ok(())
    }
    pub fn at_boundary(&self) -> bool {
        matches!(self.frame, Frame::Header { used: 0, .. })
    }
    pub fn into_output(self) -> CommandOutput {
        self.output
    }
}
fn append(output: &mut CapturedOutput, bytes: &[u8]) -> Result<(), Error> {
    output
        .bytes
        .try_reserve_exact(bytes.len())
        .map_err(Error::Allocation)?;
    output.bytes.extend_from_slice(bytes);
    Ok(())
}
