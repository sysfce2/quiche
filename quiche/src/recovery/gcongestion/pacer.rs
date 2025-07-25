// Copyright (c) 2013 The Chromium Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Copyright (C) 2023, Cloudflare, Inc.
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

use std::time::Instant;

use crate::recovery::gcongestion::Bandwidth;
use crate::recovery::rtt::RttStats;
use crate::recovery::RecoveryStats;
use crate::recovery::ReleaseDecision;
use crate::recovery::ReleaseTime;

use super::Acked;
use super::Congestion;
use super::CongestionControl;
use super::Lost;

/// Congestion window fraction that the pacing sender allows in bursts during
/// pacing.
const LUMPY_PACING_CWND_FRACTION: f64 = 0.25;

/// Number of packets that the pacing sender allows in bursts during pacing.
/// This is ignored if a flow's estimated bandwidth is lower than 1200 kbps.
const LUMPY_PACING_SIZE: usize = 2;

/// The minimum estimated client bandwidth below which the pacing sender will
/// not allow bursts.
const LUMPY_PACING_MIN_BANDWIDTH_KBPS: Bandwidth =
    Bandwidth::from_kbits_per_second(1_200);

/// Configured maximum size of the burst coming out of quiescence.  The burst is
/// never larger than the current CWND in packets.
const INITIAL_UNPACED_BURST: usize = 10;

#[derive(Debug)]
pub struct Pacer {
    /// Should this [`Pacer`] be making any release decisions?
    enabled: bool,
    /// Underlying sender
    sender: Congestion,
    /// The maximum rate the [`Pacer`] will use.
    max_pacing_rate: Option<Bandwidth>,
    /// Number of unpaced packets to be sent before packets are delayed.
    burst_tokens: usize,
    /// When can the next packet be sent.
    ideal_next_packet_send_time: ReleaseTime,
    initial_burst_size: usize,
    /// Number of unpaced packets to be sent before packets are delayed. This
    /// token is consumed after [`Self::burst_tokens`] ran out.
    lumpy_tokens: usize,
    /// Indicates whether pacing throttles the sending. If true, make up for
    /// lost time.
    pacing_limited: bool,
}

impl Pacer {
    /// Create a new [`Pacer`] with and underlying [`Congestion`]
    /// implementation, and an optional throttling as specified by
    /// `max_pacing_rate`.
    pub(crate) fn new(
        enabled: bool, congestion: Congestion, max_pacing_rate: Option<Bandwidth>,
    ) -> Self {
        Pacer {
            enabled,
            sender: congestion,
            max_pacing_rate,
            burst_tokens: INITIAL_UNPACED_BURST,
            ideal_next_packet_send_time: ReleaseTime::Immediate,
            initial_burst_size: INITIAL_UNPACED_BURST,
            lumpy_tokens: 0,
            pacing_limited: false,
        }
    }

    pub fn get_next_release_time(&self) -> ReleaseDecision {
        if !self.enabled {
            return ReleaseDecision {
                time: ReleaseTime::Immediate,
                allow_burst: true,
            };
        }

        let allow_burst = self.burst_tokens > 0 || self.lumpy_tokens > 0;
        ReleaseDecision {
            time: self.ideal_next_packet_send_time,
            allow_burst,
        }
    }

    #[cfg(feature = "qlog")]
    pub fn state_str(&self) -> &'static str {
        self.sender.state_str()
    }

    pub fn get_congestion_window(&self) -> usize {
        self.sender.get_congestion_window()
    }

    pub fn on_packet_sent(
        &mut self, sent_time: Instant, bytes_in_flight: usize,
        packet_number: u64, bytes: usize, is_retransmissible: bool,
        rtt_stats: &RttStats,
    ) {
        self.sender.on_packet_sent(
            sent_time,
            bytes_in_flight,
            packet_number,
            bytes,
            is_retransmissible,
            rtt_stats,
        );

        if !self.enabled || !is_retransmissible {
            return;
        }

        // If in recovery, the connection is not coming out of quiescence.
        if bytes_in_flight == 0 && !self.sender.is_in_recovery() {
            // Add more burst tokens anytime the connection is leaving quiescence,
            // but limit it to the equivalent of a single bulk write,
            // not exceeding the current CWND in packets.
            self.burst_tokens = self
                .initial_burst_size
                .min(self.sender.get_congestion_window_in_packets());
        }

        if self.burst_tokens > 0 {
            self.burst_tokens -= 1;
            self.ideal_next_packet_send_time = ReleaseTime::Immediate;
            self.pacing_limited = false;
            return;
        }

        // The next packet should be sent as soon as the current packet has been
        // transferred. PacingRate is based on bytes in flight including this
        // packet.
        let delay = self
            .pacing_rate(bytes_in_flight + bytes, rtt_stats)
            .transfer_time(bytes);

        if !self.pacing_limited || self.lumpy_tokens == 0 {
            // Reset lumpy_tokens_ if either application or cwnd throttles sending
            // or token runs out.
            self.lumpy_tokens = 1.max(LUMPY_PACING_SIZE.min(
                (self.sender.get_congestion_window_in_packets() as f64 *
                    LUMPY_PACING_CWND_FRACTION) as usize,
            ));

            if self.sender.bandwidth_estimate(rtt_stats) <
                LUMPY_PACING_MIN_BANDWIDTH_KBPS
            {
                // Below 1.2Mbps, send 1 packet at once, because one full-sized
                // packet is about 10ms of queueing.
                self.lumpy_tokens = 1;
            }

            if bytes_in_flight + bytes >= self.sender.get_congestion_window() {
                // Don't add lumpy_tokens if the congestion controller is CWND
                // limited.
                self.lumpy_tokens = 1;
            }
        }

        self.lumpy_tokens -= 1;
        self.ideal_next_packet_send_time.set_max(sent_time);
        self.ideal_next_packet_send_time.inc(delay);
        // Stop making up for lost time if underlying sender prevents sending.
        self.pacing_limited = self.sender.can_send(bytes_in_flight + bytes);
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn on_congestion_event(
        &mut self, rtt_updated: bool, prior_in_flight: usize,
        bytes_in_flight: usize, event_time: Instant, acked_packets: &[Acked],
        lost_packets: &[Lost], least_unacked: u64, rtt_stats: &RttStats,
        recovery_stats: &mut RecoveryStats,
    ) {
        self.sender.on_congestion_event(
            rtt_updated,
            prior_in_flight,
            bytes_in_flight,
            event_time,
            acked_packets,
            lost_packets,
            least_unacked,
            rtt_stats,
            recovery_stats,
        );

        if !self.enabled {
            return;
        }

        if !lost_packets.is_empty() {
            // Clear any burst tokens when entering recovery.
            self.burst_tokens = 0;
        }

        if let Some(max_pacing_rate) = self.max_pacing_rate {
            if rtt_updated {
                let max_rate = max_pacing_rate * 1.25f32;
                let max_cwnd =
                    max_rate.to_bytes_per_period(rtt_stats.smoothed_rtt);
                self.sender.limit_cwnd(max_cwnd as usize);
            }
        }
    }

    pub fn on_packet_neutered(&mut self, packet_number: u64) {
        self.sender.on_packet_neutered(packet_number);
    }

    pub fn on_retransmission_timeout(&mut self, packets_retransmitted: bool) {
        self.sender.on_retransmission_timeout(packets_retransmitted)
    }

    pub fn pacing_rate(
        &self, bytes_in_flight: usize, rtt_stats: &RttStats,
    ) -> Bandwidth {
        let sender_rate = self.sender.pacing_rate(bytes_in_flight, rtt_stats);
        match self.max_pacing_rate {
            Some(rate) if self.enabled => rate.min(sender_rate),
            _ => sender_rate,
        }
    }

    pub fn bandwidth_estimate(&self, rtt_stats: &RttStats) -> Bandwidth {
        self.sender.bandwidth_estimate(rtt_stats)
    }

    pub fn on_app_limited(&mut self, bytes_in_flight: usize) {
        self.pacing_limited = false;
        self.sender.on_app_limited(bytes_in_flight);
    }

    pub fn update_mss(&mut self, new_mss: usize) {
        self.sender.update_mss(new_mss)
    }

    #[cfg(feature = "qlog")]
    pub fn ssthresh(&self) -> Option<u64> {
        self.sender.ssthresh()
    }

    #[cfg(test)]
    pub fn is_app_limited(&self, bytes_in_flight: usize) -> bool {
        !self.is_cwnd_limited(bytes_in_flight)
    }

    #[cfg(test)]
    fn is_cwnd_limited(&self, bytes_in_flight: usize) -> bool {
        !self.pacing_limited && self.sender.is_cwnd_limited(bytes_in_flight)
    }
}
