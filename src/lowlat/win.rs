//! The Windows half: Wi-Fi streaming mode through the Native Wifi API, and packet priority
//! through qWAVE. Both DLLs are loaded when first needed rather than linked, so that a system
//! without one (Wine, Windows without the Wireless LAN service) only logs that it's missing: a
//! linked import that can't be found stops the whole program loading.

use super::policy::{Enabled, Pairs, Wlan};
use super::qos::{Class, Marker, Target};
use std::ffi::c_void;
use std::net::SocketAddr;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{BOOL, HANDLE};
use windows_sys::Win32::NetworkManagement::QoS::{QOSTrafficTypeAudioVideo, QOSTrafficTypeVoice, QOS_NON_ADAPTIVE_FLOW, QOS_TRAFFIC_TYPE, QOS_VERSION};
use windows_sys::Win32::NetworkManagement::WiFi::{
    wlan_interface_state_connected, wlan_intf_opcode_background_scan_enabled, wlan_intf_opcode_media_streaming_mode, WLAN_INTERFACE_INFO,
    WLAN_INTERFACE_INFO_LIST, WLAN_INTF_OPCODE,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, IN6_ADDR, IN6_ADDR_0, IN_ADDR, IN_ADDR_0, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0, SOCKET};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

/// The client version for Windows Vista and later.
const WLAN_CLIENT_VERSION: u32 = 2;

/// A function from a system DLL, or why it isn't there.
fn system_function(dll: &str, name: &str) -> Result<unsafe extern "system" fn() -> isize, String> {
    let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
    // SAFETY: a nul-terminated name, and only System32 is searched, so nothing planted beside the
    // program is loaded. A loaded system DLL stays loaded: nothing here frees it.
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(format!("{dll}: {}", std::io::Error::last_os_error()));
    }
    let cname: Vec<u8> = name.bytes().chain(Some(0)).collect();
    // SAFETY: a nul-terminated name in a module that stays loaded.
    unsafe { GetProcAddress(module, cname.as_ptr()) }.ok_or_else(|| format!("{dll} has no {name}"))
}

/// A Win32 error code as Windows words it.
fn error_text(call: &str, code: u32) -> String {
    format!("{call}: {}", std::io::Error::from_raw_os_error(code as i32))
}

/// Wine's qWAVE fails without setting an error, which would otherwise show an old one.
fn clear_last_error() {
    // SAFETY: only this thread's last-error value.
    unsafe { windows_sys::Win32::Foundation::SetLastError(0) };
}

fn last_error(call: &str) -> String {
    match std::io::Error::last_os_error().raw_os_error() {
        Some(0) | None => format!("{call} failed and gave no reason"),
        Some(code) => error_text(call, code as u32),
    }
}

// ---- Native Wifi ----

type WlanOpenHandle = unsafe extern "system" fn(u32, *const c_void, *mut u32, *mut HANDLE) -> u32;
type WlanCloseHandle = unsafe extern "system" fn(HANDLE, *const c_void) -> u32;
type WlanEnumInterfaces = unsafe extern "system" fn(HANDLE, *const c_void, *mut *mut WLAN_INTERFACE_INFO_LIST) -> u32;
type WlanSetInterface = unsafe extern "system" fn(HANDLE, *const GUID, WLAN_INTF_OPCODE, u32, *const c_void, *const c_void) -> u32;
type WlanFreeMemory = unsafe extern "system" fn(*const c_void);

struct WlanApi {
    open: WlanOpenHandle,
    close: WlanCloseHandle,
    enumerate: WlanEnumInterfaces,
    set: WlanSetInterface,
    free: WlanFreeMemory,
}

fn wlan_api() -> Result<&'static WlanApi, String> {
    static API: OnceLock<Result<WlanApi, String>> = OnceLock::new();
    API.get_or_init(|| {
        let f = |name| system_function("wlanapi.dll", name);
        // SAFETY: each pointer is the export of that name, whose signature wlanapi.h gives and the
        // types above copy.
        unsafe {
            Ok(WlanApi {
                open: std::mem::transmute::<_, WlanOpenHandle>(f("WlanOpenHandle")?),
                close: std::mem::transmute::<_, WlanCloseHandle>(f("WlanCloseHandle")?),
                enumerate: std::mem::transmute::<_, WlanEnumInterfaces>(f("WlanEnumInterfaces")?),
                set: std::mem::transmute::<_, WlanSetInterface>(f("WlanSetInterface")?),
                free: std::mem::transmute::<_, WlanFreeMemory>(f("WlanFreeMemory")?),
            })
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Streaming mode, and background scans off, on each connected wireless interface. Windows keeps
/// both only while this handle is open and the interface stays connected.
pub struct NativeWifi {
    handle: HANDLE,
    /// The interfaces this put in streaming mode.
    set: Vec<GUID>,
}

// SAFETY: the handle is only an identifier for the WLAN service, used from one thread at a time.
unsafe impl Send for NativeWifi {}

impl NativeWifi {
    pub fn new() -> NativeWifi {
        NativeWifi { handle: null_mut(), set: Vec::new() }
    }

    fn set_flag(api: &WlanApi, handle: HANDLE, guid: &GUID, opcode: WLAN_INTF_OPCODE, on: bool) -> u32 {
        let value: BOOL = on.into();
        // SAFETY: a BOOL is what both opcodes take, and the size says so.
        unsafe { (api.set)(handle, guid, opcode, std::mem::size_of::<BOOL>() as u32, (&value as *const BOOL).cast(), null()) }
    }
}

impl Wlan for NativeWifi {
    fn enable(&mut self) -> Result<Enabled, String> {
        let api = wlan_api()?;
        if self.handle.is_null() {
            let (mut version, mut handle) = (0u32, null_mut());
            // SAFETY: out-pointers to locals.
            let code = unsafe { (api.open)(WLAN_CLIENT_VERSION, null(), &mut version, &mut handle) };
            if code != 0 {
                return Err(error_text("WlanOpenHandle", code));
            }
            self.handle = handle;
        }
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
        // SAFETY: an out-pointer the API fills with memory it allocates, freed below.
        let code = unsafe { (api.enumerate)(self.handle, null(), &mut list) };
        if code != 0 {
            return Err(error_text("WlanEnumInterfaces", code));
        }
        // SAFETY: the list holds `dwNumberOfItems` entries from `InterfaceInfo` on.
        let interfaces: Vec<WLAN_INTERFACE_INFO> = unsafe {
            let count = (*list).dwNumberOfItems as usize;
            std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count).to_vec()
        };
        // SAFETY: memory the API allocated, freed once.
        unsafe { (api.free)(list.cast()) };
        let (mut set, mut failed, mut bgscan_off) = (Vec::new(), None, true);
        let connected: Vec<&WLAN_INTERFACE_INFO> = interfaces.iter().filter(|i| i.isState == wlan_interface_state_connected).collect();
        for interface in &connected {
            let guid = &interface.InterfaceGuid;
            let code = Self::set_flag(api, self.handle, guid, wlan_intf_opcode_media_streaming_mode, true);
            if code != 0 {
                failed.get_or_insert_with(|| error_text("WlanSetInterface", code));
                continue;
            }
            set.push(*guid);
            if Self::set_flag(api, self.handle, guid, wlan_intf_opcode_background_scan_enabled, false) != 0 {
                bgscan_off = false;
            }
        }
        self.set = set;
        if self.set.is_empty() {
            return Err(failed.unwrap_or_else(|| {
                if interfaces.is_empty() { "no wireless interface" } else { "no connected wireless interface" }.to_string()
            }));
        }
        Ok(Enabled { ifaces: self.set.len(), bgscan_off })
    }

    fn disable(&mut self) {
        let Ok(api) = wlan_api() else { return };
        if self.handle.is_null() {
            return;
        }
        for guid in std::mem::take(&mut self.set) {
            Self::set_flag(api, self.handle, &guid, wlan_intf_opcode_media_streaming_mode, false);
            Self::set_flag(api, self.handle, &guid, wlan_intf_opcode_background_scan_enabled, true);
        }
        // SAFETY: the handle WlanOpenHandle gave, closed once.
        unsafe { (api.close)(self.handle, null()) };
        self.handle = null_mut();
    }
}

// ---- qWAVE ----

type QosCreateHandle = unsafe extern "system" fn(*const QOS_VERSION, *mut HANDLE) -> BOOL;
type QosAddSocketToFlow = unsafe extern "system" fn(HANDLE, SOCKET, *const SOCKADDR, QOS_TRAFFIC_TYPE, u32, *mut u32) -> BOOL;
type QosRemoveSocketFromFlow = unsafe extern "system" fn(HANDLE, SOCKET, u32, u32) -> BOOL;

struct QosApi {
    create: QosCreateHandle,
    add: QosAddSocketToFlow,
    remove: QosRemoveSocketFromFlow,
}

fn qos_api() -> Result<&'static QosApi, String> {
    static API: OnceLock<Result<QosApi, String>> = OnceLock::new();
    API.get_or_init(|| {
        let f = |name| system_function("qwave.dll", name);
        // SAFETY: each pointer is the export of that name, whose signature qos2.h gives.
        unsafe {
            Ok(QosApi {
                create: std::mem::transmute::<_, QosCreateHandle>(f("QOSCreateHandle")?),
                add: std::mem::transmute::<_, QosAddSocketToFlow>(f("QOSAddSocketToFlow")?),
                remove: std::mem::transmute::<_, QosRemoveSocketFromFlow>(f("QOSRemoveSocketFromFlow")?),
            })
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The traffic type for a class, with the DSCP Windows documents it putting on the packets. These
/// are fixed: choosing the value itself needs an administrator.
fn traffic(class: Class) -> (QOS_TRAFFIC_TYPE, &'static str, u8) {
    match class {
        Class::Input => (QOSTrafficTypeVoice, "voice", 0x38),
        Class::Audio => (QOSTrafficTypeAudioVideo, "audio_video", 0x28),
    }
}

/// A socket address in the form Winsock takes.
enum RawAddr {
    V4(SOCKADDR_IN),
    V6(SOCKADDR_IN6),
}

impl RawAddr {
    fn new(addr: SocketAddr) -> RawAddr {
        match addr {
            SocketAddr::V4(a) => RawAddr::V4(SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: a.port().to_be(),
                sin_addr: IN_ADDR { S_un: IN_ADDR_0 { S_addr: u32::from_ne_bytes(a.ip().octets()) } },
                sin_zero: [0; 8],
            }),
            SocketAddr::V6(a) => RawAddr::V6(SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: IN6_ADDR { u: IN6_ADDR_0 { Byte: a.ip().octets() } },
                Anonymous: SOCKADDR_IN6_0 { sin6_scope_id: a.scope_id() },
            }),
        }
    }

    fn as_ptr(&self) -> *const SOCKADDR {
        match self {
            RawAddr::V4(a) => (a as *const SOCKADDR_IN).cast(),
            RawAddr::V6(a) => (a as *const SOCKADDR_IN6).cast(),
        }
    }
}

/// One qWAVE handle for the service, and a non-adaptive flow per socket and destination.
pub struct QwaveMarker {
    handle: HANDLE,
}

// SAFETY: as for `NativeWifi`.
unsafe impl Send for QwaveMarker {}

impl QwaveMarker {
    pub fn new() -> QwaveMarker {
        QwaveMarker { handle: null_mut() }
    }

    fn handle(&mut self, api: &QosApi) -> Result<HANDLE, String> {
        if self.handle.is_null() {
            let version = QOS_VERSION { MajorVersion: 1, MinorVersion: 0 };
            let mut handle = null_mut();
            clear_last_error();
            // SAFETY: a version and an out-pointer to a local.
            if unsafe { (api.create)(&version, &mut handle) } == 0 {
                return Err(last_error("QOSCreateHandle"));
            }
            self.handle = handle;
        }
        Ok(self.handle)
    }
}

impl Marker for QwaveMarker {
    fn needs_destination(&self) -> bool {
        true
    }

    fn mark(&mut self, target: &Target) -> Result<(Pairs, u32), String> {
        let api = qos_api()?;
        let handle = self.handle(api)?;
        let dest = target.dest.ok_or("no destination yet")?;
        let raw = RawAddr::new(dest);
        let (kind, kind_name, dscp) = traffic(target.class);
        let mut flow = 0u32;
        clear_last_error();
        // SAFETY: a registered socket, an address that lives through the call, and a flow id that
        // starts at 0 as the API asks.
        if unsafe { (api.add)(handle, target.raw as SOCKET, raw.as_ptr(), kind, QOS_NON_ADAPTIVE_FLOW, &mut flow) } == 0 {
            return Err(last_error("QOSAddSocketToFlow"));
        }
        let pairs = vec![
            ("dscp", dscp.to_string()),
            ("applied", "qwave".to_string()),
            ("traffic", kind_name.to_string()),
            ("dest", dest.to_string()),
            ("flow", flow.to_string()),
        ];
        Ok((pairs, flow))
    }

    fn unmark(&mut self, target: &Target, token: u32) {
        let (Ok(api), false) = (qos_api(), self.handle.is_null()) else { return };
        // SAFETY: the flow this socket was added to.
        unsafe { (api.remove)(self.handle, target.raw as SOCKET, token, 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_laid_out_as_winsock_reads_it() {
        let RawAddr::V4(a) = RawAddr::new("192.168.8.241:24850".parse().unwrap()) else { panic!() };
        assert_eq!(a.sin_family, AF_INET);
        assert_eq!(u16::from_be(a.sin_port), 24850);
        // SAFETY: the union was written as S_addr.
        assert_eq!(unsafe { a.sin_addr.S_un.S_addr }.to_ne_bytes(), [192, 168, 8, 241]);
        let RawAddr::V6(b) = RawAddr::new("[fe80::1%3]:24851".parse().unwrap()) else { panic!() };
        assert_eq!((b.sin6_family, u16::from_be(b.sin6_port)), (AF_INET6, 24851));
        // SAFETY: as above.
        assert_eq!(unsafe { b.Anonymous.sin6_scope_id }, 3);
    }

    #[test]
    fn the_real_wifi_and_qos_calls_fail_softly() {
        // Under Wine neither works; on a real laptop either may. Neither may panic or hang.
        let mut wifi = NativeWifi::new();
        let enabled = wifi.enable();
        println!("wifi: {enabled:?}");
        if let Err(error) = &enabled {
            assert!(!error.is_empty());
        }
        wifi.disable();
        wifi.disable();
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let target = Target {
            name: "input",
            class: Class::Input,
            raw: std::os::windows::io::AsRawSocket::as_raw_socket(&socket),
            dest: Some("127.0.0.1:24850".parse().unwrap()),
        };
        let mut marker = QwaveMarker::new();
        let marked = marker.mark(&target);
        println!("qos: {marked:?}");
        if let Ok((_, flow)) = marked {
            marker.unmark(&target, flow);
        }
    }
}
