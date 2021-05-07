use std::{collections::HashMap, convert::TryInto, fmt};

use super::common::{self, compute_checksum};
use nom::{
    branch::alt,
    bytes::streaming::{tag, take, take_until},
    error::{context, ContextError, ParseError},
    multi::{many0, many1},
    sequence::{pair, preceded, tuple},
};

type IResult<I, O> = nom::IResult<I, O, Error>;

pub fn halt_reason(i: &[u8]) -> IResult<&[u8], HaltReason> {
    // See GdbConnection::send_stop_reply_packet
    let (i, _) = tag(b"T")(i)?;
    let (i, signal_num) = two_digit_hex(i)?;
    let (i, _) = tag(b"thread:")(i)?;
    let (i, thread) = thread_id(i)?;

    let (reason, _) = tag(b";")(i)?;
    let reason = if reason.is_empty() {
        None
    } else {
        let reason =
            std::str::from_utf8(reason).map_err(|err| Error::new(reason, ErrorKind::Utf8(err)))?;
        Some(reason.to_owned())
    };

    let reply = HaltReason {
        signal_num,
        thread,
        reason,
    };

    Ok((&[], reply))
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HaltReason {
    signal_num: u8,
    thread: ThreadId,
    reason: Option<String>,
}

fn thread_id(i: &[u8]) -> IResult<&[u8], ThreadId> {
    context(
        "thread_id",
        alt((multiprocess_thread_id, singleprocess_thread_id)),
    )(i)
}

fn multiprocess_thread_id(i: &[u8]) -> IResult<&[u8], ThreadId> {
    let (i, (pid, _, tid)) = preceded(tag(b"p"), tuple((hex_number, tag(b"."), hex_number)))(i)?;
    let id = ThreadId::MultiProcess { pid, tid };
    Ok((i, id))
}

fn singleprocess_thread_id(i: &[u8]) -> IResult<&[u8], ThreadId> {
    let (i, tid) = hex_number(i)?;
    let id = ThreadId::SingleProcess { tid };
    Ok((i, id))
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum ThreadId {
    MultiProcess { pid: u64, tid: u64 },
    SingleProcess { tid: u64 },
}

fn parse_dict(i: &[u8]) -> IResult<&[u8], HashMap<Vec<u8>, Vec<u8>>> {
    let (i, pairs) = many0(pair(take_until("="), take_until(";")))(i)?;
    let mut map = HashMap::new();
    for (k, v) in pairs {
        map.insert(k.to_owned(), v.to_owned());
    }
    Ok((i, map))
}

pub fn packet_body(i: &[u8]) -> IResult<&[u8], Vec<u8>> {
    let (i, _) = tag(&[common::PACKET_START][..])(i)?;

    let (i, body) = take_until(&[common::CHECKSUM_START][..])(i)?;
    let i = &i[1..]; // take off #

    let (i, expected) = checksum(i)?;
    let actual = compute_checksum(body);
    if expected != actual {
        return Err(Error::new(
            body,
            ErrorKind::FailedChecksum { expected, actual },
        ));
    }

    if body.starts_with(b"E") {
        let body = &body[1..];
        let (rest, code) = two_digit_hex(body)?;
        assert!(rest.is_empty());
        return Err(Error::new(body, ErrorKind::App(code)));
    }

    let body = expand_body(body).map_err(|err| Error::new(body, ErrorKind::ExpandBody(err)))?;
    Ok((i, body))
}

fn expand_body(body: &[u8]) -> Result<Vec<u8>, ExpandError> {
    // See <https://www.embecosm.com/appnotes/ean4/embecosm-howto-rsp-server-ean4-issue-2.html#sec_presentation_layer>
    let mut out = Vec::with_capacity(body.len());
    let mut idx = 0;
    while idx < body.len() {
        let byte = body[idx];
        if byte == ESCAPE_INDICATOR {
            let escaped = body.get(idx + 1).ok_or(ExpandError::EscapedByte)?;
            let escaped = escaped ^ 0x20;
            out.push(escaped);
            idx += 2;
        } else if byte == RUN_LENGTH_INDICATOR {
            let count = body.get(idx + 1).ok_or(ExpandError::RunLengthCount)?;
            let count = count - 28;
            let encoded = *body.get(idx - 1).ok_or(ExpandError::RunLengthByte)?;
            // We subtract one to account for the fact we pushed it once already
            for _ in 0..count - 1 {
                out.push(encoded);
            }
            idx += 2;
        } else {
            out.push(byte);
            idx += 1;
        }
    }
    Ok(out)
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum ExpandError {
    /// Expected an escaped byte
    EscapedByte,
    /// Expected count for run length encoding
    RunLengthCount,
    /// Expected a byte to be repeated to precede run length indicator
    RunLengthByte,
}

const ESCAPE_INDICATOR: u8 = b'}';
const RUN_LENGTH_INDICATOR: u8 = b'*';

fn checksum(i: &[u8]) -> IResult<&[u8], u8> {
    two_digit_hex(i)
}

fn two_digit_hex(i: &[u8]) -> IResult<&[u8], u8> {
    let (i, d1) = hex_digit(i)?;
    let (i, d2) = hex_digit(i)?;
    let sum = (d1 << 4) + d2;
    Ok((i, sum))
}

fn hex_number(i: &[u8]) -> IResult<&[u8], u64> {
    let (i, mut digits) = many1(hex_digit)(i)?;
    digits.reverse();
    let mut num = 0;
    for (idx, digit) in digits.into_iter().enumerate() {
        num += u64::from(digit) << (4 * idx)
    }
    Ok((i, num))
}

const HEX_RADIX: u32 = 16;

fn hex_digit(i: &[u8]) -> IResult<&[u8], u8> {
    let (i, digit) = take(1_usize)(i)?;
    let digit = digit[0] as char;

    digit.to_digit(HEX_RADIX).map_or_else(
        || Err(Error::new(i, ErrorKind::ExpectedHexDigit(digit))),
        |digit| Ok((i, digit.try_into().unwrap())),
    )
}

#[derive(Debug, thiserror::Error)]
pub struct Error {
    input: Vec<u8>,
    kind: ErrorKind,
    context: Option<&'static str>,
    causes: Vec<ErrorKind>,
}

#[derive(Debug, displaydoc::Display)]
pub enum ErrorKind {
    /// Expected hex digit, got {0}
    ExpectedHexDigit(char),
    /// Failed checksum check. Expected {expected}, got {actual}
    FailedChecksum { expected: u8, actual: u8 },
    /// Failed to expand body: {0}
    ExpandBody(ExpandError),
    /// Failed to parse as utf-8: {0}
    Utf8(std::str::Utf8Error),
    /// Application level error. Code: {0}
    App(u8),
    /// Nom error: {0:?}
    Nom(nom::error::ErrorKind),
}

impl Error {
    fn new(input: &[u8], kind: ErrorKind) -> nom::Err<Self> {
        Self::new_inner(input, kind).into()
    }

    fn new_inner(input: &[u8], kind: ErrorKind) -> Self {
        Self {
            input: input.into(),
            kind,
            context: None,
            causes: Vec::new(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(context) = &self.context {
            write!(f, "{} ", context)?;
        }
        write!(f, "{}", &self.kind)?;
        if !self.causes.is_empty() {
            let causes: String = self.causes.iter().map(|c| format!("{}", c)).collect();
            write!(f, ". Caused by: {}", causes)?;
        }
        write!(f, "\n\t{:?}", &self.input)?;
        Ok(())
    }
}

impl ParseError<&[u8]> for Error {
    fn from_error_kind(input: &[u8], kind: nom::error::ErrorKind) -> Self {
        Self::new_inner(input, ErrorKind::Nom(kind))
    }

    fn append(_input: &[u8], kind: nom::error::ErrorKind, mut other: Self) -> Self {
        other.causes.push(ErrorKind::Nom(kind));
        other
    }
}

impl ContextError<&[u8]> for Error {
    fn add_context(_input: &[u8], ctx: &'static str, mut other: Self) -> Self {
        other.context = Some(ctx);
        other
    }
}

impl From<Error> for nom::Err<Error> {
    fn from(err: Error) -> Self {
        nom::Err::Error(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn stop_reply_single_threaded() -> eyre::Result<()> {
        let actual = halt_reason(b"T00thread:29164b;")?;
        let expected = (
            &[][..],
            HaltReason {
                signal_num: 0,
                thread: ThreadId::SingleProcess { tid: 0x0029_164b },
                reason: None,
            },
        );
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test]
    fn test_hex_number() -> eyre::Result<()> {
        let data: &[(_, (_, u64))] = &[
            (&b"00z"[..], (&b"z"[..], 0x00)),
            (&b"01z"[..], (&b"z"[..], 0x01)),
            (&b"22_foo"[..], (&b"_foo"[..], 0x22)),
            (&b"111111_"[..], (&b"_"[..], 0x0011_1111)),
        ];

        for (input, expected) in data {
            let actual = hex_number(input)?;
            assert_eq!(*expected, actual);
        }

        assert_matches!(hex_number(b"zaa1"), Err(_));
        Ok(())
    }

    #[test]
    fn test_checksum_zero_padded() -> eyre::Result<()> {
        let actual = checksum(b"0c_trailing")?;
        let expected = (&b"_trailing"[..], 12_u8);
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test]
    fn test_checksum() -> eyre::Result<()> {
        let actual = checksum(b"44_trailing")?;
        let expected = (&b"_trailing"[..], 68_u8);
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test]
    fn test_expand_body_escape() -> eyre::Result<()> {
        // } is ascii 0x7d
        // # is ascii 0x23. 23 xor 0x20 = 0x3.
        let actual = expand_body(&[0x45, 0x7d, 0x03, 0x45])?;
        let expected = vec![0x45, 0x23, 0x45];
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test]
    fn test_expand_body_only_escape() -> eyre::Result<()> {
        // } is ascii 0x7d
        // # is ascii 0x23. 23 xor 0x20 = 0x3.
        let actual = expand_body(&[0x7d, 0x03])?;
        let expected = vec![0x23];
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test]
    fn test_expand_body_rle() -> eyre::Result<()> {
        let actual = expand_body(&b"foo_X*!_bar"[..])?;
        let expected = b"foo_XXXXX_bar".to_vec();
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test]
    fn test_expand_body_only_rle() -> eyre::Result<()> {
        let actual = expand_body(&b"X*!"[..])?;
        let expected = b"XXXXX".to_vec();
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test]
    fn test_packet_body() -> eyre::Result<()> {
        // (f + o + o) % 256 = 324 % 256 = 68 = 0x44
        let actual = packet_body(b"$foo#44")?;
        let expected = (&b""[..], b"foo".to_vec());
        assert_eq!(expected, actual);
        Ok(())
    }
}