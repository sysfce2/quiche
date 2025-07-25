// Copyright (C) 2024, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! h3i - low-level HTTP/3 debug and testing
//!
//! HTTP/3 ([RFC 9114]) is the wire format for HTTP semantics ([RFC 9110]). The
//! RFCs contain a range of requirements about how Request or Response messages
//! are generated, serialized, sent, received, parsed, and consumed. QUIC ([RFC
//! 9000]) streams are used for these messages along with other control and
//! QPACK ([RFC 9204]) header compression instructions.
//!
//! h3i provides a highly configurable HTTP/3 client that can bend RFC rules in
//! order to test the behavior of servers. QUIC streams can be opened, fin'd,
//! stopped or reset at any point in time. HTTP/3 frames can be sent on any
//! stream, in any order, containing user-controlled content (both legal and
//! illegal).
//!
//! # Example
//!
//! The following example sends a request with its Content-Length header set to
//! 5, but with its body only consisting of 4 bytes. This is classified as a
//! [malformed request], and the server should respond with a 400 Bad Request
//! response. Once h3i receives the response, it will close the connection.
//!
//! ```no_run
//! use h3i::actions::h3::Action;
//! use h3i::actions::h3::StreamEvent;
//! use h3i::actions::h3::StreamEventType;
//! use h3i::actions::h3::WaitType;
//! use h3i::client::sync_client;
//! use h3i::config::Config;
//! use quiche::h3::frame::Frame;
//! use quiche::h3::Header;
//! use quiche::h3::NameValue;
//!
//! fn main() {
//!    /// The QUIC stream to send the frames on. See
//!    /// https://datatracker.ietf.org/doc/html/rfc9000#name-streams and
//!    /// https://datatracker.ietf.org/doc/html/rfc9114#request-streams for more.
//!    const STREAM_ID: u64 = 0;
//!
//!    let config = Config::new()
//!        .with_host_port("blog.cloudflare.com".to_string())
//!        .with_idle_timeout(2000)
//!        .build()
//!        .unwrap();
//!
//!    let headers = vec![
//!        Header::new(b":method", b"POST"),
//!        Header::new(b":scheme", b"https"),
//!        Header::new(b":authority", b"blog.cloudflare.com"),
//!        Header::new(b":path", b"/"),
//!        // We say that we're going to send a body with 5 bytes...
//!        Header::new(b"content-length", b"5"),
//!    ];
//!
//!    let header_block = encode_header_block(&headers).unwrap();
//!
//!    let actions = vec![
//!        Action::SendHeadersFrame {
//!            stream_id: STREAM_ID,
//!            fin_stream: false,
//!            headers,
//!            frame: Frame::Headers { header_block },
//!            literal_headers: false,
//!        },
//!        Action::SendFrame {
//!            stream_id: STREAM_ID,
//!            fin_stream: true,
//!            frame: Frame::Data {
//!                // ...but, in actuality, we only send 4 bytes. This should yield a
//!                // 400 Bad Request response from an RFC-compliant
//!                // server: https://datatracker.ietf.org/doc/html/rfc9114#section-4.1.2-3
//!                payload: b"test".to_vec(),
//!            },
//!        },
//!        Action::Wait {
//!            wait_type: WaitType::StreamEvent(StreamEvent {
//!                stream_id: STREAM_ID,
//!                event_type: StreamEventType::Headers,
//!            }),
//!        },
//!        Action::ConnectionClose {
//!            error: quiche::ConnectionError {
//!                is_app: true,
//!                error_code: quiche::h3::WireErrorCode::NoError as u64,
//!                reason: vec![],
//!            },
//!        },
//!    ];
//!
//!    // This example doesn't use close trigger frames, since we manually close the connection upon
//!    // receiving a HEADERS frame on stream 0.
//!    let close_trigger_frames = None;
//!    let summary = sync_client::connect(config, actions, close_trigger_frames);
//!
//!    println!(
//!        "=== received connection summary! ===\n\n{}",
//!        serde_json::to_string_pretty(&summary).unwrap_or_else(|e| e.to_string())
//!    );
//! }
//!
//! // SendHeadersFrame requires a QPACK-encoded header block. h3i provides a
//! // `send_headers_frame` helper function to abstract this, but for clarity, we do
//! // it here.
//! fn encode_header_block(
//!     headers: &[quiche::h3::Header],
//! ) -> std::result::Result<Vec<u8>, String> {
//!     let mut encoder = quiche::h3::qpack::Encoder::new();
//!
//!     let headers_len = headers
//!         .iter()
//!         .fold(0, |acc, h| acc + h.value().len() + h.name().len() + 32);
//!
//!     let mut header_block = vec![0; headers_len];
//!     let len = encoder
//!         .encode(headers, &mut header_block)
//!         .map_err(|_| "Internal Error")?;
//!
//!     header_block.truncate(len);
//!
//!     Ok(header_block)
//! }
//! ```

//! [RFC 9000]: https://www.rfc-editor.org/rfc/rfc9000.html
//! [RFC 9110]: https://www.rfc-editor.org/rfc/rfc9110.html
//! [RFC 9114]: https://www.rfc-editor.org/rfc/rfc9114.html
//! [RFC 9204]: https://www.rfc-editor.org/rfc/rfc9204.html
//! [malformed request]: https://datatracker.ietf.org/doc/html/rfc9114#section-4.1.2-3

use qlog::events::quic::PacketHeader;
use qlog::events::quic::PacketSent;
use qlog::events::quic::PacketType;
use qlog::events::quic::QuicFrame;
use qlog::events::EventData;
use quiche::h3::qpack::encode_int;
use quiche::h3::qpack::encode_str;
use quiche::h3::qpack::LITERAL;
use quiche::h3::NameValue;
use smallvec::SmallVec;

#[cfg(not(feature = "async"))]
pub use quiche;
#[cfg(feature = "async")]
pub use tokio_quiche::quiche;

/// The ID for an HTTP/3 control stream type.
///
/// See <https://datatracker.ietf.org/doc/html/rfc9114#name-control-streams>.
pub const HTTP3_CONTROL_STREAM_TYPE_ID: u64 = 0x0;

/// The ID for an HTTP/3 push stream type.
///
/// See <https://datatracker.ietf.org/doc/html/rfc9114#name-push-streams>.
pub const HTTP3_PUSH_STREAM_TYPE_ID: u64 = 0x1;

/// The ID for a QPACK encoder stream type.
///
/// See <https://datatracker.ietf.org/doc/html/rfc9204#section-4.2-2.1>.
pub const QPACK_ENCODER_STREAM_TYPE_ID: u64 = 0x2;

/// The ID for a QPACK decoder stream type.
///
/// See <https://datatracker.ietf.org/doc/html/rfc9204#section-4.2-2.2>.
pub const QPACK_DECODER_STREAM_TYPE_ID: u64 = 0x3;

#[derive(Default)]
struct StreamIdAllocator {
    id: u64,
}

impl StreamIdAllocator {
    pub fn take_next_id(&mut self) -> u64 {
        let old = self.id;
        self.id += 4;

        old
    }

    pub fn peek_next_id(&mut self) -> u64 {
        self.id
    }
}

/// Encodes a header block literally. Unlike [`encode_header_block`],
/// this function encodes all the headers exactly as provided. This
/// means it does not use the huffman lookup table, nor does it convert
/// the header names to lowercase before encoding.
fn encode_header_block_literal(
    headers: &[quiche::h3::Header],
) -> std::result::Result<Vec<u8>, String> {
    // This is a combination of a modified `quiche::h3::qpack::Encoder::encode`
    // and the [`encode_header_block`] function.
    let headers_len = headers
        .iter()
        .fold(0, |acc, h| acc + h.value().len() + h.name().len() + 32);

    let mut header_block = vec![0; headers_len];

    let mut b = octets::OctetsMut::with_slice(&mut header_block);

    // Required Insert Count.
    encode_int(0, 0, 8, &mut b).map_err(|e| format!("{e:?}"))?;

    // Base.
    encode_int(0, 0, 7, &mut b).map_err(|e| format!("{e:?}"))?;

    for h in headers {
        encode_str::<false>(h.name(), LITERAL, 3, &mut b)
            .map_err(|e| format!("{e:?}"))?;
        encode_str::<false>(h.value(), 0, 7, &mut b)
            .map_err(|e| format!("{e:?}"))?;
    }

    let len = b.off();

    header_block.truncate(len);
    Ok(header_block)
}

fn encode_header_block(
    headers: &[quiche::h3::Header],
) -> std::result::Result<Vec<u8>, String> {
    let mut encoder = quiche::h3::qpack::Encoder::new();

    let headers_len = headers
        .iter()
        .fold(0, |acc, h| acc + h.value().len() + h.name().len() + 32);

    let mut header_block = vec![0; headers_len];
    let len = encoder
        .encode(headers, &mut header_block)
        .map_err(|_| "Internal Error")?;

    header_block.truncate(len);

    Ok(header_block)
}

fn fake_packet_header() -> PacketHeader {
    PacketHeader {
        packet_type: PacketType::OneRtt,
        packet_number: None,
        flags: None,
        token: None,
        length: None,
        version: None,
        scil: None,
        dcil: None,
        scid: None,
        dcid: None,
    }
}

fn fake_packet_sent(frames: Option<SmallVec<[QuicFrame; 1]>>) -> EventData {
    EventData::PacketSent(PacketSent {
        header: fake_packet_header(),
        frames,
        ..Default::default()
    })
}

pub mod actions;
pub mod client;
pub mod config;
pub mod frame;
pub mod frame_parser;
pub mod prompts;
pub mod recordreplay;
