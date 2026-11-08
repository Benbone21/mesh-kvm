//! The audio packets on UDP 24851.
//!
//! Every packet starts `MKAU`, a version byte, a kind byte and a stream byte. Anything else is ignored: it is
//! never played, so two builds that disagree are quiet rather than noisy. The magic differs from
//! the `MKVA` of the first format, as that one had no version byte and its sequence number could
//! look like one.

use super::codec::Format;

pub const MAGIC: [u8; 4] = *b"MKAU";
const OLD_MAGIC: [u8; 4] = *b"MKVA";
pub const VERSION: u8 = 3;
const KIND_OPUS: u8 = 1;
const KIND_PCM: u8 = 2;
const KIND_QUIET: u8 = 3;
const KIND_REPORT: u8 = 4;
/// magic, version, kind, stream, epoch, sequence.
pub const HEADER: usize = 13;
const REPORT_BODY: usize = 10;

/// Which of the two audio streams a packet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Sound = 0,
    Mic = 1,
}

impl Stream {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Sound),
            1 => Some(Self::Mic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Report {
    /// Audio packets that arrived since the sender started, in a counter that may wrap.
    pub received: u32,
    /// Of the packets due lately, the percentage that never arrived in time.
    pub loss_percent: u8,
    /// The far side has an output open for this stream.
    pub playing: bool,
    /// Real sound, not silence, has lately come out of that output.
    pub sounding: bool,
    /// Play is on there, so what arrives will be played, whether or not an output is open for it
    /// right now.
    pub ready: bool,
    /// How much audio the far side is holding back, in ms.
    pub buffered_ms: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Body<'a> {
    Audio(Format, &'a [u8]),
    /// Nothing to hear. The stream is alive, so nothing is lost and no packets are due.
    Quiet,
    Report(Report),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    /// Changes when the sender restarts, as its sequence numbers start again.
    pub epoch: u16,
    pub seq: u32,
    pub stream: Stream,
    pub body: Body<'a>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not ours at all.
    Stranger,
    /// A mesh-kvm from another version.
    OtherVersion,
}

fn write_header(out: &mut Vec<u8>, kind: u8, stream: Stream, epoch: u16, seq: u32) {
    out.clear();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[VERSION, kind, stream as u8]);
    out.extend_from_slice(&epoch.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
}

pub fn write_audio(out: &mut Vec<u8>, format: Format, stream: Stream, epoch: u16, seq: u32, payload: &[u8]) {
    let kind = match format {
        Format::Opus => KIND_OPUS,
        Format::Pcm => KIND_PCM,
    };
    write_header(out, kind, stream, epoch, seq);
    out.extend_from_slice(payload);
}

pub fn write_quiet(out: &mut Vec<u8>, stream: Stream, epoch: u16, seq: u32) {
    write_header(out, KIND_QUIET, stream, epoch, seq);
}

pub fn write_report(out: &mut Vec<u8>, stream: Stream, epoch: u16, report: Report) {
    write_header(out, KIND_REPORT, stream, epoch, 0);
    out.extend_from_slice(&report.received.to_le_bytes());
    out.push(report.loss_percent);
    out.push(u8::from(report.playing) | u8::from(report.sounding) << 1 | u8::from(report.ready) << 2);
    out.extend_from_slice(&report.buffered_ms.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
}

fn read_report(body: &[u8]) -> Option<Report> {
    (body.len() >= REPORT_BODY).then(|| Report {
        received: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
        loss_percent: body[4],
        playing: body[5] & 1 != 0,
        sounding: body[5] & 2 != 0,
        ready: body[5] & 4 != 0,
        buffered_ms: u16::from_le_bytes([body[6], body[7]]),
    })
}

pub fn parse(data: &[u8]) -> Result<Packet<'_>, Refused> {
    if data.starts_with(&OLD_MAGIC) {
        return Err(Refused::OtherVersion);
    }
    if !data.starts_with(&MAGIC) || data.len() < MAGIC.len() + 1 {
        return Err(Refused::Stranger);
    }
    if data[4] != VERSION {
        return Err(Refused::OtherVersion);
    }
    if data.len() < HEADER {
        return Err(Refused::Stranger);
    }
    let stream = Stream::from_byte(data[6]).ok_or(Refused::Stranger)?;
    let epoch = u16::from_le_bytes([data[7], data[8]]);
    let seq = u32::from_le_bytes([data[9], data[10], data[11], data[12]]);
    let rest = &data[HEADER..];
    let body = match data[5] {
        KIND_OPUS if !rest.is_empty() => Body::Audio(Format::Opus, rest),
        KIND_PCM if !rest.is_empty() => Body::Audio(Format::Pcm, rest),
        KIND_QUIET => Body::Quiet,
        KIND_REPORT => Body::Report(read_report(rest).ok_or(Refused::Stranger)?),
        _ => return Err(Refused::Stranger),
    };
    Ok(Packet { epoch, seq, stream, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_survives_the_round_trip() {
        let mut out = Vec::new();
        write_audio(&mut out, Format::Opus, Stream::Sound, 7, 300, &[1, 2, 3]);
        assert_eq!(
            parse(&out),
            Ok(Packet { epoch: 7, seq: 300, stream: Stream::Sound, body: Body::Audio(Format::Opus, &[1, 2, 3]) })
        );
        write_audio(&mut out, Format::Pcm, Stream::Sound, 7, 301, &[9]);
        assert_eq!(parse(&out).unwrap().body, Body::Audio(Format::Pcm, &[9]));
    }

    #[test]
    fn a_report_and_a_quiet_marker_survive_the_round_trip() {
        let report = Report { received: 90_000, loss_percent: 12, playing: true, sounding: true, ready: true, buffered_ms: 30 };
        let mut out = Vec::new();
        write_report(&mut out, Stream::Sound, 3, report);
        assert_eq!(parse(&out).unwrap().body, Body::Report(report));
        write_quiet(&mut out, Stream::Sound, 3, 12);
        assert_eq!(parse(&out), Ok(Packet { epoch: 3, seq: 12, stream: Stream::Sound, body: Body::Quiet }));
    }

    #[test]
    fn an_open_output_and_real_sound_are_different_things() {
        let mut out = Vec::new();
        write_report(&mut out, Stream::Sound, 3, Report { playing: true, ..Default::default() });
        assert!(matches!(parse(&out).unwrap().body, Body::Report(Report { playing: true, sounding: false, .. })));
    }

    #[test]
    fn being_ready_to_play_is_not_the_same_as_having_an_output_open() {
        let mut out = Vec::new();
        write_report(&mut out, Stream::Sound, 3, Report { ready: true, ..Default::default() });
        assert!(matches!(parse(&out).unwrap().body, Body::Report(Report { playing: false, sounding: false, ready: true, .. })));
    }

    #[test]
    fn the_first_format_is_turned_away_whatever_its_sequence_number() {
        for seq in [1u64, 2, 258, u64::MAX / 2] {
            let mut old = Vec::from(OLD_MAGIC);
            old.extend_from_slice(&seq.to_le_bytes());
            old.extend_from_slice(&[0x55; 960]);
            assert_eq!(parse(&old), Err(Refused::OtherVersion), "{seq}");
        }
    }

    #[test]
    fn a_later_version_is_turned_away_before_its_body_is_read() {
        let mut out = Vec::new();
        write_audio(&mut out, Format::Opus, Stream::Sound, 1, 1, &[1]);
        out[4] = VERSION + 1;
        assert_eq!(parse(&out), Err(Refused::OtherVersion));
        out.truncate(5);
        assert_eq!(parse(&out), Err(Refused::OtherVersion), "even with nothing behind it");
    }

    #[test]
    fn strangers_and_scraps_are_turned_away() {
        for data in [&b""[..], b"MKAU", b"hello there, this is not ours", b"MKAU\x03\x01\x00"] {
            assert_eq!(parse(data), Err(Refused::Stranger), "{data:?}");
        }
        let mut out = Vec::new();
        write_audio(&mut out, Format::Opus, Stream::Sound, 1, 1, &[]);
        assert_eq!(parse(&out), Err(Refused::Stranger), "audio with nothing in it");
        write_report(&mut out, Stream::Sound, 1, Report::default());
        out.truncate(HEADER + 2);
        assert_eq!(parse(&out), Err(Refused::Stranger), "a cut-off report");
        out[5] = 99;
        assert_eq!(parse(&out), Err(Refused::Stranger), "an unknown kind");
    }

    #[test]
    fn each_stream_survives_the_round_trip() {
        let mut out = Vec::new();
        for stream in [Stream::Sound, Stream::Mic] {
            write_audio(&mut out, Format::Opus, stream, 7, 300, &[1, 2, 3]);
            assert_eq!(parse(&out).unwrap().stream, stream);
            write_quiet(&mut out, stream, 7, 301);
            assert_eq!(parse(&out).unwrap().stream, stream);
            write_report(&mut out, stream, 7, Report { ready: true, ..Default::default() });
            assert_eq!(parse(&out).unwrap().stream, stream);
        }
    }

    #[test]
    fn version_two_is_turned_away() {
        let mut out = Vec::new();
        write_audio(&mut out, Format::Opus, Stream::Sound, 1, 1, &[1]);
        out[4] = 2;
        assert_eq!(parse(&out), Err(Refused::OtherVersion));
    }

    #[test]
    fn an_unknown_stream_is_a_stranger() {
        let mut out = Vec::new();
        write_audio(&mut out, Format::Opus, Stream::Sound, 1, 1, &[1]);
        out[6] = 9;
        assert_eq!(parse(&out), Err(Refused::Stranger));
    }
}
