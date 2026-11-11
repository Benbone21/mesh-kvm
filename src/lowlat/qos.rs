//! Marking the link's and audio's packets as high priority, so that Wi-Fi (WMM) sends them ahead of
//! everything else waiting. Linux sets the DSCP on the socket itself. Windows ignores that unless a
//! policy says otherwise, so there qWAVE adds each socket to a flow of the right traffic type.

use super::policy::Pairs;
use std::net::SocketAddr;

/// What a socket carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The input link: DSCP EF, the WMM voice queue.
    Input,
    /// Sound: DSCP AF41, the WMM video queue.
    Audio,
}

#[cfg(target_os = "linux")]
impl Class {
    /// The DSCP Linux puts on the packets.
    pub fn dscp(self) -> u8 {
        match self {
            Class::Input => 46,
            Class::Audio => 34,
        }
    }
}

/// A socket to mark, as the marker sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub name: &'static str,
    pub class: Class,
    /// The raw fd or SOCKET. Registered sockets last as long as the service does.
    pub raw: u64,
    /// Where its packets go, once that's known. Only qWAVE needs it.
    pub dest: Option<SocketAddr>,
}

/// How the packets get marked on this system.
pub trait Marker {
    /// Whether marking needs the destination, as qWAVE does for a socket that isn't connected.
    fn needs_destination(&self) -> bool;
    /// Marks the socket's packets. Gives back what was done, for the log, and a token for undoing it.
    fn mark(&mut self, target: &Target) -> Result<(Pairs, u32), String>;
    fn unmark(&mut self, target: &Target, token: u32);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Unmarked,
    Marked { dest: Option<SocketAddr>, token: u32 },
    Failed { dest: Option<SocketAddr> },
}

/// One registered socket and how it stands.
#[derive(Debug, Clone)]
pub struct Entry {
    pub target: Target,
    state: State,
    /// Whether any line has been logged for it yet, so that the first one says it's off.
    said: bool,
}

impl Entry {
    pub fn new(target: Target) -> Entry {
        Entry { target, state: State::Unmarked, said: false }
    }

    /// Brings the socket to what `on` calls for. A `qos` line comes back when its state changed.
    pub fn sync(&mut self, on: bool, marker: &mut dyn Marker) -> Option<Pairs> {
        let name = ("socket", self.target.name.to_string());
        if !on {
            let was_marked = matches!(self.state, State::Marked { .. });
            if let State::Marked { token, .. } = self.state {
                marker.unmark(&self.target, token);
            }
            self.state = State::Unmarked;
            if was_marked || !self.said {
                self.said = true;
                return Some(vec![name, ("applied", "off".into()), ("reason", "setting".into())]);
            }
            return None;
        }
        // Only a marker that needs the destination marks again when it moves.
        let dest = if marker.needs_destination() { self.target.dest } else { None };
        if marker.needs_destination() && dest.is_none() {
            return None;
        }
        match self.state {
            State::Marked { dest: d, .. } | State::Failed { dest: d } if d == dest => return None,
            State::Marked { token, .. } => marker.unmark(&self.target, token),
            _ => {}
        }
        self.said = true;
        match marker.mark(&self.target) {
            Ok((what, token)) => {
                self.state = State::Marked { dest, token };
                let mut pairs = vec![name];
                pairs.extend(what);
                Some(pairs)
            }
            Err(error) => {
                self.state = State::Failed { dest };
                Some(vec![name, ("applied", "none".into()), ("error", error)])
            }
        }
    }
}

// ---- Linux ----

/// `IP_TOS` on an IPv4 socket, `IPV6_TCLASS` on an IPv6 one. No privileges needed.
#[cfg(target_os = "linux")]
pub struct TosMarker;

#[cfg(target_os = "linux")]
impl TosMarker {
    fn set(raw: u64, v6: bool, tos: u8) -> Result<(), String> {
        let value = libc::c_int::from(tos);
        let (level, option) = if v6 { (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) } else { (libc::IPPROTO_IP, libc::IP_TOS) };
        // SAFETY: `raw` is a socket that outlives this call, and `value` is the c_int both options take.
        let done = unsafe {
            libc::setsockopt(raw as libc::c_int, level, option, (&value as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t)
        };
        if done == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }

    fn is_v6(raw: u64) -> bool {
        use std::os::fd::BorrowedFd;
        // SAFETY: the registered socket stays open for the service's life.
        let fd = unsafe { BorrowedFd::borrow_raw(raw as libc::c_int) };
        socket_family(fd) == Some(libc::AF_INET6)
    }
}

#[cfg(target_os = "linux")]
fn socket_family(fd: std::os::fd::BorrowedFd) -> Option<libc::c_int> {
    use std::os::fd::AsRawFd;
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `addr` is big enough for any address, and `len` says so.
    let done = unsafe { libc::getsockname(fd.as_raw_fd(), (&mut addr as *mut libc::sockaddr_storage).cast(), &mut len) };
    (done == 0).then_some(libc::c_int::from(addr.ss_family))
}

#[cfg(target_os = "linux")]
impl Marker for TosMarker {
    fn needs_destination(&self) -> bool {
        false
    }

    fn mark(&mut self, target: &Target) -> Result<(Pairs, u32), String> {
        let v6 = Self::is_v6(target.raw);
        let dscp = target.class.dscp();
        Self::set(target.raw, v6, dscp << 2)?;
        let option = if v6 { "ipv6_tclass" } else { "ip_tos" };
        Ok((vec![("dscp", dscp.to_string()), ("applied", option.into())], 0))
    }

    fn unmark(&mut self, target: &Target, _token: u32) {
        let _ = Self::set(target.raw, Self::is_v6(target.raw), 0);
    }
}

/// What the socket's packets are marked with now, as Linux keeps it: the whole TOS or traffic class byte.
#[cfg(all(target_os = "linux", test))]
pub fn read_tos(socket: &std::net::UdpSocket) -> u8 {
    use std::os::fd::{AsFd, AsRawFd};
    let v6 = socket_family(socket.as_fd()) == Some(libc::AF_INET6);
    let (level, option) = if v6 { (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) } else { (libc::IPPROTO_IP, libc::IP_TOS) };
    let mut value: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` describe a c_int, which both options give back.
    let done = unsafe { libc::getsockopt(socket.as_raw_fd(), level, option, (&mut value as *mut libc::c_int).cast(), &mut len) };
    assert_eq!(done, 0, "{}", std::io::Error::last_os_error());
    value as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeMarker {
        needs_dest: bool,
        fail: Option<String>,
        marks: Vec<Option<SocketAddr>>,
        unmarks: Vec<u32>,
    }

    impl Marker for FakeMarker {
        fn needs_destination(&self) -> bool {
            self.needs_dest
        }
        fn mark(&mut self, target: &Target) -> Result<(Pairs, u32), String> {
            self.marks.push(target.dest);
            match &self.fail {
                Some(error) => Err(error.clone()),
                None => Ok((vec![("applied", "fake".into())], self.marks.len() as u32)),
            }
        }
        fn unmark(&mut self, _target: &Target, token: u32) {
            self.unmarks.push(token);
        }
    }

    fn entry(dest: Option<SocketAddr>) -> Entry {
        Entry::new(Target { name: "input", class: Class::Input, raw: 3, dest })
    }

    fn text(pairs: Option<Pairs>) -> Option<String> {
        pairs.map(|p| p.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "))
    }

    #[test]
    fn a_socket_is_marked_once_and_unmarked_when_the_setting_goes_off() {
        let (mut e, mut m) = (entry(None), FakeMarker::default());
        assert_eq!(text(e.sync(true, &mut m)).as_deref(), Some("socket=input applied=fake"));
        assert_eq!(e.sync(true, &mut m), None);
        assert_eq!(m.marks.len(), 1);
        assert_eq!(text(e.sync(false, &mut m)).as_deref(), Some("socket=input applied=off reason=setting"));
        assert_eq!(m.unmarks, [1]);
        assert_eq!(e.sync(false, &mut m), None);
        assert_eq!(text(e.sync(true, &mut m)).as_deref(), Some("socket=input applied=fake"));
    }

    #[test]
    fn off_from_the_start_is_said_once_and_nothing_is_touched() {
        let (mut e, mut m) = (entry(None), FakeMarker::default());
        assert_eq!(text(e.sync(false, &mut m)).as_deref(), Some("socket=input applied=off reason=setting"));
        assert_eq!(e.sync(false, &mut m), None);
        assert!(m.marks.is_empty() && m.unmarks.is_empty());
    }

    #[test]
    fn a_failure_is_logged_once_until_the_setting_or_the_destination_changes() {
        let mut m = FakeMarker { fail: Some("qwave.dll: not found".into()), ..Default::default() };
        let mut e = entry(None);
        assert_eq!(text(e.sync(true, &mut m)).as_deref(), Some("socket=input applied=none error=qwave.dll: not found"));
        assert_eq!(e.sync(true, &mut m), None);
        assert_eq!(m.marks.len(), 1, "not tried again");
        e.sync(false, &mut m);
        assert!(m.unmarks.is_empty(), "nothing was marked, so nothing is undone");
        assert!(e.sync(true, &mut m).is_some());
        assert_eq!(m.marks.len(), 2);
    }

    #[test]
    fn a_destination_that_moves_changes_nothing_where_marks_do_not_need_one() {
        let (mut e, mut m) = (entry(None), FakeMarker::default());
        assert!(e.sync(true, &mut m).is_some());
        e.target.dest = Some("192.168.8.210:40000".parse().unwrap());
        assert_eq!(e.sync(true, &mut m), None);
        assert_eq!((m.marks.len(), m.unmarks.len()), (1, 0));
    }

    #[test]
    fn qwave_waits_for_a_destination_and_follows_it_when_it_moves() {
        let mut m = FakeMarker { needs_dest: true, ..Default::default() };
        let mut e = entry(None);
        assert_eq!(e.sync(true, &mut m), None, "unconnected UDP needs to know where it sends");
        let first: SocketAddr = "192.168.8.241:40000".parse().unwrap();
        e.target.dest = Some(first);
        assert!(e.sync(true, &mut m).is_some());
        assert_eq!(e.sync(true, &mut m), None);
        let second: SocketAddr = "192.168.8.241:40001".parse().unwrap();
        e.target.dest = Some(second);
        assert!(e.sync(true, &mut m).is_some());
        assert_eq!(m.marks, [Some(first), Some(second)]);
        assert_eq!(m.unmarks, [1], "the old flow goes first");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_marks_input_ef_and_audio_af41_and_reads_back_what_it_set() {
        use std::os::fd::AsRawFd;
        let input = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let audio = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(read_tos(&input), 0);
        let mut marker = TosMarker;
        let mut e = Entry::new(Target { name: "input", class: Class::Input, raw: input.as_raw_fd() as u64, dest: None });
        assert_eq!(text(e.sync(true, &mut marker)).as_deref(), Some("socket=input dscp=46 applied=ip_tos"));
        assert_eq!(read_tos(&input), 46 << 2);
        let mut a = Entry::new(Target { name: "audio_tx", class: Class::Audio, raw: audio.as_raw_fd() as u64, dest: None });
        a.sync(true, &mut marker);
        assert_eq!(read_tos(&audio), 34 << 2);
        e.sync(false, &mut marker);
        assert_eq!(read_tos(&input), 0, "off takes the mark away");
        assert_eq!(read_tos(&audio), 34 << 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_marks_an_ipv6_socket_by_its_traffic_class() {
        use std::os::fd::AsRawFd;
        let Ok(socket) = std::net::UdpSocket::bind("[::1]:0") else { return };
        let mut e = Entry::new(Target { name: "input", class: Class::Input, raw: socket.as_raw_fd() as u64, dest: None });
        assert_eq!(text(e.sync(true, &mut TosMarker)).as_deref(), Some("socket=input dscp=46 applied=ipv6_tclass"));
        assert_eq!(read_tos(&socket), 46 << 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_marked_packet_leaves_with_its_dscp() {
        // What arrives over loopback carries the sender's TOS byte, which IP_RECVTOS hands over.
        use std::os::fd::AsRawFd;
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let on: libc::c_int = 1;
        // SAFETY: a c_int flag on a socket that lives through the test.
        let done = unsafe { libc::setsockopt(rx.as_raw_fd(), libc::IPPROTO_IP, libc::IP_RECVTOS, (&on as *const libc::c_int).cast(), 4) };
        assert_eq!(done, 0);
        let mut e = Entry::new(Target { name: "input", class: Class::Input, raw: tx.as_raw_fd() as u64, dest: None });
        e.sync(true, &mut TosMarker);
        tx.send_to(b"x", rx.local_addr().unwrap()).unwrap();
        assert_eq!(received_tos(&rx), Some(46 << 2));
    }

    #[cfg(target_os = "linux")]
    fn received_tos(socket: &std::net::UdpSocket) -> Option<u8> {
        use std::os::fd::AsRawFd;
        let mut data = [0u8; 16];
        let mut control = [0u8; 64];
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
        // SAFETY: msghdr is plain data, and every pointer in it is to a buffer alive for the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len() as _;
        let got = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
        assert!(got > 0, "{}", std::io::Error::last_os_error());
        // SAFETY: the CMSG macros walk the control buffer recvmsg filled in.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::IPPROTO_IP && (*cmsg).cmsg_type == libc::IP_TOS {
                    return Some(*libc::CMSG_DATA(cmsg));
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        None
    }
}
