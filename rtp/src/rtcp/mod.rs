use std::collections::VecDeque;

use crate::Ssrc;

mod fmt;
mod iter;
mod nack;
mod rr;
mod sdes;
mod sr;
mod twcc;

use fmt::{FeedbackMessageType, PayloadType, TransportType};
use iter::FbIter;
pub use nack::Nack;
pub use rr::ReceiverReport;
use sdes::Sdes;
pub use sr::SenderInfo;

#[derive(Debug, PartialEq, Eq)]
pub enum RtcpFb {
    SenderInfo(SenderInfo),
    ReceiverReport(ReceiverReport),
    Sdes(Sdes),
    Goodbye(Ssrc),
    Nack(Nack),
    Pli(Ssrc),
    Fir(Ssrc),
}

#[derive(Debug)]
pub struct RtcpHeader {
    pub version: u8,
    pub has_padding: bool,
    pub fmt: FeedbackMessageType,
    pub packet_type: RtcpType,
    /// Length of RTCP message in bytes, including header.
    pub length: usize,
    /// There is always an ssrc following the first 4 bytes, sometimes
    /// it counts towards the header, sometimes it doesn't.
    pub ssrc: Ssrc,
}

/// Kind of RTCP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcpType {
    /// RTCP_PT_SR
    SenderReport = 200,
    /// RTCP_PT_RR
    ReceiverReport = 201,
    /// RTCP_PT_SDES
    SourceDescription = 202,
    /// RTCP_PT_BYE
    Goodbye = 203,

    /// RTCP_PT_APP
    ApplicationDefined = 204,
    /// RTCP_PT_RTPFB
    // https://tools.ietf.org/html/rfc4585
    TransportLayerFeedback = 205,
    /// RTCP_PT_PSFB
    // https://tools.ietf.org/html/rfc4585
    PayloadSpecificFeedback = 206,
    /// RTCP_PT_XR
    ExtendedReport = 207,
}

impl RtcpType {
    const fn header_len(&self) -> usize {
        use RtcpType::*;
        match self {
            // The sender SSRC is the actual sender info SSRC.
            SenderReport => 4,
            // The first SSRC is the "sender", which is useless and sent as 0.
            ReceiverReport => 8,
            // The first SSRC is part of the chunks of SDES.
            SourceDescription => 4,
            // The first SSRC is an actual goodbye.
            Goodbye => 4,
            ApplicationDefined => 4,
            // The first SSRC is the "sender", which is useless and sent as 0.
            TransportLayerFeedback => 8,
            // The first SSRC is the "sender", which is useless and sent as 0.
            PayloadSpecificFeedback => 8,
            ExtendedReport => 8,
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        use RtcpType::*;
        match v {
            200 => Some(SenderReport),   // sr
            201 => Some(ReceiverReport), // rr
            202 => Some(SourceDescription),
            203 => Some(Goodbye),
            204 => Some(ApplicationDefined),
            205 => Some(TransportLayerFeedback),
            206 => Some(PayloadSpecificFeedback),
            207 => Some(ExtendedReport),
            _ => {
                trace!("Unrecognized RTCP type: {}", v);
                None
            }
        }
    }
}

//         0                   1                   2                   3
//         0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//        +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// header |V=2|P|    RC   |   PT=SR=200   |             length            |
//        +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//        |                         SSRC of sender                        |
//        +=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+

impl RtcpHeader {
    pub fn parse(buf: &[u8], is_srtcp: bool) -> Option<RtcpHeader> {
        use RtcpType::*;

        if buf.len() < 8 {
            trace!("RTCP header too short < 8: {}", buf.len());
            return None;
        }

        let version = (buf[0] & 0b1100_0000_u8) >> 6;
        if version != 2 {
            trace!("RTCP version is not 2");
            return None;
        }
        let has_padding = buf[0] & 0b0010_0000 > 0;

        let fmt_n = buf[0] & 0b0001_1111;
        let packet_type = RtcpType::from_u8(buf[1])?;
        use FeedbackMessageType::*;
        let fmt = match packet_type {
            SenderReport | ReceiverReport => ReceptionReport(fmt_n),
            SourceDescription | Goodbye => SourceCount(fmt_n),
            ApplicationDefined => Subtype(fmt_n),
            TransportLayerFeedback => TransportFeedback(TransportType::from_u8(fmt_n)?),
            PayloadSpecificFeedback => PayloadFeedback(PayloadType::from_u8(fmt_n)?),
            ExtendedReport => NotUsed,
        };

        if is_srtcp && packet_type != SenderReport && packet_type != ReceiverReport {
            // The first RTCP packet in the compound packet MUST
            // always be a report packet to facilitate header validation as
            // described in Appendix A.2.  This is true even if no data has been
            // sent or received, in which case an empty RR MUST be sent, and even
            // if the only other RTCP packet in the compound packet is a BYE.
            trace!("SRTCP packet requires SenderReport or ReceiverReport");
            return None;
        }

        let length_be = [buf[2], buf[3]];

        // https://tools.ietf.org/html/rfc3550#section-6.4.1
        //   The length of this RTCP packet in 32-bit words minus one,
        //   including the header and any padding. (The offset of one makes
        //   zero a valid length ...)
        let length = (u16::from_be_bytes(length_be) + 1) * 4;

        // There's always an SSRC after the first 4 octets, sometimes it counts
        // towards the header, sometimes it doesn't. We can always read it.
        let ssrc = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]).into();

        Some(RtcpHeader {
            version,
            has_padding,
            fmt,
            packet_type,
            length: length as usize,
            ssrc,
        })
    }

    fn write_to(&self, buf: &mut [u8]) {
        assert!(self.length % 4 == 0, "Rtcp length must be a multiple of 4");

        buf[0] = 0b10_0_00000 | self.fmt.as_u8();
        buf[1] = self.packet_type as u8;

        let length = (self.length / 4) - 1;
        (&mut buf[2..4]).copy_from_slice(&(length as u16).to_be_bytes());

        if self.len() == 8 {
            // size indicates we should write the ssrc for the header, and not the body.
            (&mut buf[4..8]).copy_from_slice(&(*self.ssrc).to_be_bytes());
        }
    }

    fn len(&self) -> usize {
        self.packet_type.header_len()
    }
}

const SR_LEN: usize = 6 * 4;
const RR_LEN: usize = 6 * 4;

impl RtcpFb {
    pub fn feedback<'a>(buf: &'a [u8]) -> impl Iterator<Item = RtcpFb> + 'a {
        FbIter::new(buf)
    }

    #[must_use]
    pub fn build_feedback(feedback: &mut VecDeque<Self>, mut buf: &mut [u8]) -> usize {
        let mut abs = 0;

        // Certain grouping is possible, which means we sort the feedback to
        // be able to extract the groups.
        feedback.make_contiguous().sort_by_key(RtcpFb::ord_no);

        // This either writes SenderInfo + ReceiverReport to make SenderReports (SR), or
        // straight up ReceiverReports (RR).
        while matches!(
            feedback.front(),
            Some(RtcpFb::SenderInfo(_)) | Some(RtcpFb::ReceiverReport(_))
        ) {
            // length needed to fit the first item
            let (needed_len, xrr) = if matches!(feedback.front(), Some(RtcpFb::SenderInfo(_))) {
                (RtcpType::SenderReport.header_len() + SR_LEN, 0)
            } else {
                (RtcpType::ReceiverReport.header_len() + RR_LEN, 1)
            };

            if buf.len() < needed_len {
                // can't fit anything more in this buf
                return abs;
            }

            // We are definitely writing the first item
            let fb = feedback.pop_front().unwrap();

            // Figure out how many receiver reports we can fit after the first item.
            let max_rr = {
                let rr_count = feedback
                    .iter()
                    .filter(|f| matches!(f, RtcpFb::ReceiverReport(_)))
                    .count();

                let available_for_rr = buf.len() - needed_len;
                let fitting_sr = available_for_rr / RR_LEN;

                // Each SR can hold at most 31 RR. This is furter restricted by how much
                // space is left in the buffer we write to.
                rr_count.min(31).min(fitting_sr)
            };

            // Total length of the first item + fitted rr.
            let length = needed_len + max_rr * RR_LEN;

            // Number of receiver reports to send.
            let count = max_rr + xrr;

            // The header is either for a SR or RR.
            let header = fb.as_header(count as u8, length);
            header.write_to(buf);

            // First item after the header.
            fb.write_to(&mut buf[header.len()..]);

            // Then we remove the rr as we are grouping them into this rtcp.
            for i in 0..max_rr {
                let pos = feedback
                    .iter()
                    .position(|f| matches!(f, RtcpFb::ReceiverReport(_)))
                    // This fn presupposes there are max_rr available reports.
                    .expect("there to be enough RR to yank");

                let rr = feedback.remove(pos).unwrap();

                // Offset into buffer this is to be at.
                let off = needed_len + i * RR_LEN;

                rr.write_to(&mut buf[off..]);
            }

            buf = &mut buf[length..];
            abs += length;
        }

        while matches!(feedback.front(), Some(RtcpFb::Goodbye(_))) {
            const NEEDED: usize = 4 + 4;

            if buf.len() < NEEDED {
                // No room for more.
                return abs;
            }

            // We're definitely writing one goodbye.
            let fb = feedback.pop_front().unwrap();

            let max_gb = {
                let gb_count = feedback
                    .iter()
                    .filter(|f| matches!(f, RtcpFb::Goodbye(_)))
                    .count();

                let available_for_gb = buf.len() - NEEDED;
                let fitting_gb = available_for_gb / 4;

                gb_count.min(31).min(fitting_gb)
            };

            // Total length of the first item + fitted rr.
            let length = NEEDED + max_gb * 4;

            // The header with counts/length
            let header = fb.as_header(max_gb as u8, length);
            header.write_to(buf);

            // First item after the header.
            fb.write_to(&mut buf[header.len()..]);

            // Then we remove the rr as we are grouping them into this rtcp.
            for i in 0..max_gb {
                let fbn = feedback.pop_front().expect("there be enough goodbye");

                // Offset into buffer this is to be at.
                let off = NEEDED + i * 4;

                fbn.write_to(&mut buf[off..]);
            }

            buf = &mut buf[length..];
            abs += length;
        }

        abs
    }

    fn ord_no(&self) -> usize {
        use RtcpFb::*;
        match self {
            SenderInfo(_) => 0,
            ReceiverReport(_) => 1,
            Goodbye(_) => 2,
            Sdes(_) => 3,
            Nack(_) => 4,
            Pli(_) => 5,
            Fir(_) => 6,
        }
    }

    fn write_to(&self, buf: &mut [u8]) {
        use RtcpFb::*;
        match self {
            SenderInfo(v) => v.write_to(buf),
            ReceiverReport(v) => v.write_to(buf),
            Goodbye(v) => v.write_to(buf),
            Sdes(_) => todo!(),
            Nack(_) => todo!(),
            Pli(_) => todo!(),
            Fir(_) => todo!(),
        }
    }

    pub fn ssrc(&self) -> Ssrc {
        use RtcpFb::*;
        match self {
            SenderInfo(v) => v.ssrc,
            ReceiverReport(v) => v.ssrc,
            Sdes(v) => v.ssrc,
            Goodbye(v) => *v,
            Nack(v) => v.ssrc,
            Pli(v) => *v,
            Fir(v) => *v,
        }
    }

    fn as_header(&self, count: u8, length: usize) -> RtcpHeader {
        let (fmt, packet_type, ssrc) = match self {
            RtcpFb::SenderInfo(v) => (
                FeedbackMessageType::ReceptionReport(count),
                RtcpType::SenderReport,
                v.ssrc,
            ),
            RtcpFb::ReceiverReport(_) => (
                FeedbackMessageType::ReceptionReport(count),
                RtcpType::ReceiverReport,
                0.into(),
            ),
            RtcpFb::Sdes(_) => (
                FeedbackMessageType::SourceCount(count),
                RtcpType::SourceDescription,
                0.into(),
            ),
            RtcpFb::Goodbye(_) => (
                //
                FeedbackMessageType::SourceCount(count),
                RtcpType::Goodbye,
                0.into(),
            ),
            RtcpFb::Nack(_) => (
                FeedbackMessageType::TransportFeedback(TransportType::Nack),
                RtcpType::TransportLayerFeedback,
                0.into(),
            ),
            RtcpFb::Pli(_) => (
                FeedbackMessageType::PayloadFeedback(PayloadType::PictureLossIndication),
                RtcpType::PayloadSpecificFeedback,
                0.into(),
            ),
            RtcpFb::Fir(_) => (
                FeedbackMessageType::PayloadFeedback(PayloadType::FullIntraRequest),
                RtcpType::PayloadSpecificFeedback,
                0.into(),
            ),
        };

        RtcpHeader {
            version: 2,
            has_padding: false,
            fmt,
            packet_type,
            length,
            ssrc,
        }
    }
}

impl Ssrc {
    fn write_to(&self, buf: &mut [u8]) {
        (&mut buf[0..4]).copy_from_slice(&(*self).to_be_bytes())
    }
}

#[cfg(test)]
mod test {
    use crate::MediaTime;

    use super::*;

    fn sr(ssrc: u32, ntp_time: MediaTime) -> RtcpFb {
        RtcpFb::SenderInfo(SenderInfo {
            ssrc: ssrc.into(),
            ntp_time,
            rtp_time: 4,
            sender_packet_count: 5,
            sender_octet_count: 6,
        })
    }

    fn rr(ssrc: u32) -> RtcpFb {
        RtcpFb::ReceiverReport(ReceiverReport {
            ssrc: ssrc.into(),
            fraction_lost: 3,
            packets_lost: 1234,
            max_seq: 4000,
            jitter: 5,
            last_sr_time: 12,
            last_sr_delay: 1,
        })
    }

    #[test]
    fn test_sr() {
        let mut buf = vec![0; 1200];

        let now = MediaTime::now();

        let mut fb = VecDeque::new();
        fb.push_back(sr(1, now));

        let n = RtcpFb::build_feedback(&mut fb, &mut buf);
        buf.truncate(n);
        assert_eq!(n, 28);

        let mut iter = RtcpFb::feedback(&buf);

        assert_eq!(iter.next(), Some(sr(1, now)));
    }

    #[test]
    fn test_rr() {
        let mut buf = vec![0; 1200];

        let mut fb = VecDeque::new();
        fb.push_back(rr(2));

        let n = RtcpFb::build_feedback(&mut fb, &mut buf);
        buf.truncate(n);
        assert_eq!(n, 32);

        let mut iter = RtcpFb::feedback(&buf);

        assert_eq!(iter.next(), Some(rr(2)));
    }

    #[test]
    fn test_sr_rr() {
        let mut buf = vec![0; 1200];

        let now = MediaTime::now();

        let mut fb = VecDeque::new();
        fb.push_back(rr(2));
        fb.push_back(sr(1, now));

        let n = RtcpFb::build_feedback(&mut fb, &mut buf);
        buf.truncate(n);
        assert_eq!(n, 52);

        let mut iter = RtcpFb::feedback(&buf);

        assert_eq!(iter.next(), Some(sr(1, now)));
        assert_eq!(iter.next(), Some(rr(2)));
    }

    #[test]
    fn test_sr_rr_more_than_31() {
        let mut buf = vec![0; 1200];

        let now = MediaTime::now();

        let mut fb = VecDeque::new();
        for i in 0..33 {
            fb.push_back(rr(i + 2));
        }
        fb.push_back(sr(1, now));

        let n = RtcpFb::build_feedback(&mut fb, &mut buf);
        buf.truncate(n);
        assert_eq!(n, 828);

        let mut iter = RtcpFb::feedback(&buf);

        assert_eq!(iter.next(), Some(sr(1, now)));
        for i in 0..33 {
            fb.push_back(rr(i + 2));
        }
    }
}
