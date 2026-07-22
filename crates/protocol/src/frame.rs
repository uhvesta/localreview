use crate::{
    ProtocolError, FRAME_COMPRESSION_THRESHOLD_BYTES, MAX_FRAME_BYTES, MAX_UNCOMPRESSED_FRAME_BYTES,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "payload", rename_all = "snake_case")]
enum FrameEnvelope {
    Raw(Vec<u8>),
    Zstd(Vec<u8>),
}

/// A CBOR message with a four-byte big-endian length prefix.  This is used over
/// SSH stdio and the local Unix socket.  It avoids newline ambiguities and lets
/// both ends enforce a bound before allocating a payload.
pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), ProtocolError> {
    let payload = serde_cbor::to_vec(value)?;
    if payload.len() > MAX_UNCOMPRESSED_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    let envelope = if payload.len() >= FRAME_COMPRESSION_THRESHOLD_BYTES {
        FrameEnvelope::Zstd(
            zstd::stream::encode_all(payload.as_slice(), 3)
                .map_err(|error| ProtocolError::Compression(error.to_string()))?,
        )
    } else {
        FrameEnvelope::Raw(payload)
    };
    let payload = serde_cbor::to_vec(&envelope)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    let length =
        u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameTooLarge(payload.len()))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, ProtocolError> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(length));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    let envelope: FrameEnvelope =
        serde_cbor::from_slice(&payload).map_err(ProtocolError::Serialization)?;
    let payload = match envelope {
        FrameEnvelope::Raw(payload) => {
            if payload.len() > MAX_UNCOMPRESSED_FRAME_BYTES {
                return Err(ProtocolError::FrameTooLarge(payload.len()));
            }
            payload
        }
        FrameEnvelope::Zstd(payload) => decompress_bounded(&payload)?,
    };
    serde_cbor::from_slice(&payload).map_err(ProtocolError::Serialization)
}

fn decompress_bounded(payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    let decoder = zstd::stream::read::Decoder::new(payload)
        .map_err(|error| ProtocolError::Compression(error.to_string()))?;
    let mut limited = decoder.take((MAX_UNCOMPRESSED_FRAME_BYTES + 1) as u64);
    let mut output = Vec::with_capacity(payload.len().min(MAX_UNCOMPRESSED_FRAME_BYTES));
    limited.read_to_end(&mut output)?;
    if output.len() > MAX_UNCOMPRESSED_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(output.len()));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_bound_before_payload_allocation() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &vec!["one", "two"]).unwrap();
        let restored: Vec<String> = read_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!(restored, ["one", "two"]);

        let mut oversized = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes().to_vec();
        oversized.extend_from_slice(&[1, 2, 3]);
        assert!(matches!(
            read_frame::<Vec<String>>(&mut oversized.as_slice()),
            Err(ProtocolError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn large_frames_are_compressed_and_round_trip() {
        let mut bytes = Vec::new();
        let source = "diff line\n".repeat(FRAME_COMPRESSION_THRESHOLD_BYTES);
        write_frame(&mut bytes, &source).unwrap();
        assert!(bytes.len() < source.len());
        let restored: String = read_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!(restored, source);
    }
}
