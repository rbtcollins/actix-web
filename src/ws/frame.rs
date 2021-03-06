use std::{fmt, mem};
use std::io::{Error, ErrorKind};
use std::iter::FromIterator;
use bytes::{BytesMut, BufMut};
use byteorder::{ByteOrder, BigEndian, NetworkEndian};
use rand;

use body::Binary;
use ws::proto::{OpCode, CloseCode};
use ws::mask::apply_mask;

/// A struct representing a `WebSocket` frame.
#[derive(Debug)]
pub(crate) struct Frame {
    finished: bool,
    rsv1: bool,
    rsv2: bool,
    rsv3: bool,
    opcode: OpCode,
    payload: Binary,
}

impl Frame {

    /// Destruct frame
    pub fn unpack(self) -> (bool, OpCode, Binary) {
        (self.finished, self.opcode, self.payload)
    }

    /// Create a new Close control frame.
    #[inline]
    pub fn close(code: CloseCode, reason: &str, genmask: bool) -> Binary {
        let raw: [u8; 2] = unsafe {
            let u: u16 = code.into();
            mem::transmute(u.to_be())
        };

        let payload = if let CloseCode::Empty = code {
            Vec::new()
        } else {
            Vec::from_iter(
                raw[..].iter()
                    .chain(reason.as_bytes().iter())
                    .cloned())
        };

        Frame::message(payload, OpCode::Close, true, genmask)
    }

    /// Parse the input stream into a frame.
    pub fn parse(buf: &mut BytesMut, server: bool) -> Result<Option<Frame>, Error> {
        let mut idx = 2;
        let mut size = buf.len();

        if size < 2 {
            return Ok(None)
        }
        size -= 2;
        let first = buf[0];
        let second = buf[1];
        let finished = first & 0x80 != 0;

        // check masking
        let masked = second & 0x80 != 0;
        if !masked && server {
            return Err(Error::new(
                ErrorKind::Other, "Received an unmasked frame from client"))
        } else if masked && !server {
            return Err(Error::new(
                ErrorKind::Other, "Received a masked frame from server"))
        }

        let rsv1 = first & 0x40 != 0;
        let rsv2 = first & 0x20 != 0;
        let rsv3 = first & 0x10 != 0;
        let opcode = OpCode::from(first & 0x0F);
        let len = second & 0x7F;

        let length = if len == 126 {
            if size < 2 {
                return Ok(None)
            }
            let len = NetworkEndian::read_uint(&buf[idx..], 2) as usize;
            size -= 2;
            idx += 2;
            len
        } else if len == 127 {
            if size < 8 {
                return Ok(None)
            }
            let len = NetworkEndian::read_uint(&buf[idx..], 8) as usize;
            size -= 8;
            idx += 8;
            len
        } else {
            len as usize
        };

        let mask = if server {
            if size < 4 {
                return Ok(None)
            } else {
                let mut mask_bytes = [0u8; 4];
                size -= 4;
                mask_bytes.copy_from_slice(&buf[idx..idx+4]);
                idx += 4;
                Some(mask_bytes)
            }
        } else {
            None
        };

        if size < length {
            return Ok(None)
        }

        // get body
        buf.split_to(idx);
        let mut data = if length > 0 {
            buf.split_to(length)
        } else {
            BytesMut::new()
        };

        // Disallow bad opcode
        if let OpCode::Bad = opcode {
            return Err(
                Error::new(
                    ErrorKind::Other,
                    format!("Encountered invalid opcode: {}", first & 0x0F)))
        }

        // control frames must have length <= 125
        match opcode {
            OpCode::Ping | OpCode::Pong if length > 125 => {
                return Err(
                    Error::new(
                        ErrorKind::Other,
                        format!("Rejected WebSocket handshake.Received control frame with length: {}.", length)))
            }
            OpCode::Close if length > 125 => {
                debug!("Received close frame with payload length exceeding 125. Morphing to protocol close frame.");
                return Ok(Some(Frame::default()))
            }
            _ => ()
        }

        // unmask
        if let Some(ref mask) = mask {
            apply_mask(&mut data, mask);
        }

        Ok(Some(Frame {
            finished: finished,
            rsv1: rsv1,
            rsv2: rsv2,
            rsv3: rsv3,
            opcode: opcode,
            payload: data.into(),
        }))
    }

    /// Generate binary representation
    pub fn message<B: Into<Binary>>(data: B, code: OpCode,
                                    finished: bool, genmask: bool) -> Binary
    {
        let payload = data.into();
        let one: u8 = if finished {
            0x80 | Into::<u8>::into(code)
        } else {
            code.into()
        };
        let payload_len = payload.len();
        let (two, p_len) = if genmask {
            (0x80, payload_len + 4)
        } else {
            (0, payload_len)
        };

        let mut buf = if payload_len < 126 {
            let mut buf = BytesMut::with_capacity(p_len + 2);
            buf.put_slice(&[one, two | payload_len as u8]);
            buf
        } else if payload_len <= 65_535 {
            let mut buf = BytesMut::with_capacity(p_len + 4);
            buf.put_slice(&[one, two | 126]);
            {
                let buf_mut = unsafe{buf.bytes_mut()};
                BigEndian::write_u16(&mut buf_mut[..2], payload_len as u16);
            }
            unsafe{buf.advance_mut(2)};
            buf
        } else {
            let mut buf = BytesMut::with_capacity(p_len + 8);
            buf.put_slice(&[one, two | 127]);
            {
                let buf_mut = unsafe{buf.bytes_mut()};
                BigEndian::write_u64(&mut buf_mut[..8], payload_len as u64);
            }
            unsafe{buf.advance_mut(8)};
            buf
        };

        if genmask {
            let mask: [u8; 4] = rand::random();
            unsafe {
                {
                    let buf_mut = buf.bytes_mut();
                    buf_mut[..4].copy_from_slice(&mask);
                    buf_mut[4..payload_len+4].copy_from_slice(payload.as_ref());
                    apply_mask(&mut buf_mut[4..], &mask);
                }
                buf.advance_mut(payload_len + 4);
            }
            buf.into()
        } else {
            buf.put_slice(payload.as_ref());
            buf.into()
        }
    }
}

impl Default for Frame {
    fn default() -> Frame {
        Frame {
            finished: true,
            rsv1: false,
            rsv2: false,
            rsv3: false,
            opcode: OpCode::Close,
            payload: Binary::from(&b""[..]),
        }
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
            "
<FRAME>
    final: {}
    reserved: {} {} {}
    opcode: {}
    payload length: {}
    payload: 0x{}
</FRAME>",
               self.finished,
               self.rsv1,
               self.rsv2,
               self.rsv3,
               self.opcode,
               self.payload.len(),
               self.payload.as_ref().iter().map(
                   |byte| format!("{:x}", byte)).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        let mut buf = BytesMut::from(&[0b00000001u8, 0b00000001u8][..]);
        assert!(Frame::parse(&mut buf, false).unwrap().is_none());
        buf.extend(b"1");
        let frame = Frame::parse(&mut buf, false).unwrap().unwrap();
        println!("FRAME: {}", frame);
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.as_ref(), &b"1"[..]);
    }

    #[test]
    fn test_parse_length0() {
        let mut buf = BytesMut::from(&[0b00000001u8, 0b00000000u8][..]);
        let frame = Frame::parse(&mut buf, false).unwrap().unwrap();
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn test_parse_length2() {
        let mut buf = BytesMut::from(&[0b00000001u8, 126u8][..]);
        assert!(Frame::parse(&mut buf, false).unwrap().is_none());
        buf.extend(&[0u8, 4u8][..]);
        buf.extend(b"1234");

        let frame = Frame::parse(&mut buf, false).unwrap().unwrap();
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.as_ref(), &b"1234"[..]);
    }

    #[test]
    fn test_parse_length4() {
        let mut buf = BytesMut::from(&[0b00000001u8, 127u8][..]);
        assert!(Frame::parse(&mut buf, false).unwrap().is_none());
        buf.extend(&[0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 4u8][..]);
        buf.extend(b"1234");

        let frame = Frame::parse(&mut buf, false).unwrap().unwrap();
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.as_ref(), &b"1234"[..]);
    }

    #[test]
    fn test_parse_frame_mask() {
        let mut buf = BytesMut::from(&[0b00000001u8, 0b10000001u8][..]);
        buf.extend(b"0001");
        buf.extend(b"1");

        assert!(Frame::parse(&mut buf, false).is_err());

        let frame = Frame::parse(&mut buf, true).unwrap().unwrap();
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload, vec![1u8].into());
    }

    #[test]
    fn test_parse_frame_no_mask() {
        let mut buf = BytesMut::from(&[0b00000001u8, 0b00000001u8][..]);
        buf.extend(&[1u8]);

        assert!(Frame::parse(&mut buf, true).is_err());

        let frame = Frame::parse(&mut buf, false).unwrap().unwrap();
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload, vec![1u8].into());
    }

    #[test]
    fn test_ping_frame() {
        let frame = Frame::message(Vec::from("data"), OpCode::Ping, true, false);

        let mut v = vec![137u8, 4u8];
        v.extend(b"data");
        assert_eq!(frame, v.into());
    }

    #[test]
    fn test_pong_frame() {
        let frame = Frame::message(Vec::from("data"), OpCode::Pong, true, false);

        let mut v = vec![138u8, 4u8];
        v.extend(b"data");
        assert_eq!(frame, v.into());
    }

    #[test]
    fn test_close_frame() {
        let frame = Frame::close(CloseCode::Normal, "data", false);

        let mut v = vec![136u8, 6u8, 3u8, 232u8];
        v.extend(b"data");
        assert_eq!(frame, v.into());
    }
}
