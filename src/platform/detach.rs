//! Starting a process that outlives this one, without handing it anything of this one's. Rust's
//! `Command` on Windows always lets the child inherit every inheritable handle, and a network
//! filter can make a socket inheritable. The child would then hold this service's ports for as
//! long as it runs, and a new service couldn't take them. So on Windows the children are made
//! here, with `CreateProcessW` told to pass on no handles at all.
//!
//! The command line and environment are built in plain code, so they're tested everywhere.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))] // Only Windows starts processes this way.

/// One argument, quoted the way Windows programs split their command line: backslashes only
/// count before a quote, so those are doubled, and a quote inside is escaped.
pub fn quote_arg(arg: &str, always: bool) -> String {
    let quote = always || arg.is_empty() || arg.contains([' ', '\t', '"']);
    let mut out = String::new();
    if quote {
        out.push('"');
    }
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                out.extend(std::iter::repeat('\\').take(backslashes + 1));
            }
            backslashes = 0;
        }
        out.push(c);
    }
    if quote {
        out.extend(std::iter::repeat('\\').take(backslashes));
        out.push('"');
    }
    out
}

/// The whole command line: the program, always quoted, then each argument.
pub fn command_line(exe: &str, args: &[String]) -> String {
    let mut line = quote_arg(exe, true);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg, false));
    }
    line
}

/// An environment block for `CreateProcessW`: `base` with `extra` set over it, as UTF-16, sorted
/// the way Windows keeps it, ending in two NULs. Names are compared ignoring case, as Windows does.
pub fn environment_block(base: Vec<(Vec<u16>, Vec<u16>)>, extra: &[(&str, &str)]) -> Vec<u16> {
    let wide = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let upper = |name: &[u16]| String::from_utf16_lossy(name).to_uppercase();
    let mut all: Vec<(Vec<u16>, Vec<u16>)> =
        base.into_iter().filter(|(name, _)| !extra.iter().any(|(e, _)| upper(name) == e.to_uppercase())).collect();
    all.extend(extra.iter().map(|(name, value)| (wide(name), wide(value))));
    all.sort_by_key(|(name, _)| upper(name));
    let mut block = Vec::new();
    for (name, value) in all {
        block.extend(name);
        block.push(u16::from(b'='));
        block.extend(value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(target_os = "windows")]
mod real {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
        CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, STARTUPINFOW,
    };

    /// A process started here. Dropping it lets it run on.
    pub struct Child {
        process: HANDLE,
    }

    // A process handle can be used from any thread.
    unsafe impl Send for Child {}

    impl Child {
        pub fn kill(&mut self) {
            unsafe { TerminateProcess(self.process, 1) };
        }

        /// Waits for it to end, for at most `time`. True if it has.
        pub fn wait(&mut self, time: Duration) -> bool {
            unsafe { WaitForSingleObject(self.process, millis(time)) != WAIT_TIMEOUT }
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.process) };
        }
    }

    fn millis(time: Duration) -> u32 {
        time.as_millis().min(u128::from(u32::MAX - 1)) as u32
    }

    fn image_name(process: HANDLE) -> String {
        let mut name = vec![0u16; 1024];
        let mut size = name.len() as u32;
        if unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, name.as_mut_ptr(), &mut size) } == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&name[..size as usize])
    }

    fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Starts `exe` with `args`, and `envs` set on top of this process's environment. It has no
    /// console, no standard input or output, and none of this process's handles.
    pub fn spawn(exe: &Path, args: &[String], envs: &[(&str, &str)]) -> std::io::Result<Child> {
        let application = wide(exe.as_os_str());
        let line = super::command_line(&exe.to_string_lossy(), args);
        let mut line: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        let base = std::env::vars_os().map(|(k, v)| (k.encode_wide().collect(), v.encode_wide().collect())).collect();
        let environment = super::environment_block(base, envs);
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let made = unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast(),
                std::ptr::null(),
                &startup,
                &mut info,
            )
        };
        if made == 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { CloseHandle(info.hThread) };
        Ok(Child { process: info.hProcess })
    }

    /// Waits up to `patience` for process `pid` to end, and ends it if it hasn't. True if it was
    /// still there and had to be ended. A process that's already gone is fine. One that isn't
    /// mesh-kvm has the id only because the old one is gone and Windows gave it out again, so it's
    /// left alone.
    pub fn wait_for_exit(pid: u32, patience: Duration) -> bool {
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        if !super::is_mesh_kvm(&image_name(process)) {
            unsafe { CloseHandle(process) };
            return false;
        }
        let ended = unsafe { WaitForSingleObject(process, millis(patience)) } != WAIT_TIMEOUT;
        if !ended {
            unsafe {
                TerminateProcess(process, 1);
                WaitForSingleObject(process, 5000);
            }
        }
        unsafe { CloseHandle(process) };
        !ended
    }
}

/// True for the path of a mesh-kvm exe, including one an update has moved aside.
pub fn is_mesh_kvm(image: &str) -> bool {
    let name = image.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase();
    name.starts_with("mesh-kvm")
}

#[cfg(target_os = "windows")]
pub use real::{spawn, wait_for_exit, Child};

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn plain_arguments_are_left_as_they_are() {
        assert_eq!(command_line(r"C:\a\mesh-kvm.exe", &args(&["--role", "windows", "--port", "24860"])), r#""C:\a\mesh-kvm.exe" --role windows --port 24860"#);
    }

    #[test]
    fn a_program_in_a_folder_with_spaces_stays_one_word() {
        let line = command_line(r"C:\Users\Mary Jane\AppData\Local\mesh-kvm\mesh-kvm.exe", &args(&["--tray"]));
        assert_eq!(line, r#""C:\Users\Mary Jane\AppData\Local\mesh-kvm\mesh-kvm.exe" --tray"#);
    }

    #[test]
    fn spaces_quotes_and_backslashes_survive_the_trip() {
        assert_eq!(quote_arg("a b", false), r#""a b""#);
        assert_eq!(quote_arg("", false), r#""""#);
        assert_eq!(quote_arg(r#"say "hi""#, false), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir\", false), r"C:\dir\");
        assert_eq!(quote_arg(r"C:\my dir\", false), r#""C:\my dir\\""#);
        assert_eq!(quote_arg(r#"a\"b"#, false), r#""a\\\"b""#);
    }

    fn block_text(block: &[u16]) -> Vec<String> {
        String::from_utf16_lossy(block).split('\0').map(str::to_string).collect()
    }

    fn entry(name: &str, value: &str) -> (Vec<u16>, Vec<u16>) {
        (name.encode_utf16().collect(), value.encode_utf16().collect())
    }

    #[test]
    fn the_environment_keeps_what_is_there_and_sets_the_extra_over_it() {
        let base = vec![entry("Path", r"C:\Windows"), entry("MESH_KVM_RESTARTING", "0"), entry("APPDATA", r"C:\a")];
        let block = environment_block(base, &[("mesh_kvm_restarting", "1")]);
        assert_eq!(block_text(&block), vec![r"APPDATA=C:\a", "mesh_kvm_restarting=1", r"Path=C:\Windows", "", ""]);
    }

    #[test]
    fn only_a_mesh_kvm_process_is_waited_for_or_ended() {
        assert!(is_mesh_kvm(r"C:\Users\me\AppData\Local\mesh-kvm\mesh-kvm.exe.old"));
        assert!(is_mesh_kvm(r"C:\Users\me\AppData\Local\mesh-kvm\MESH-KVM.EXE"));
        assert!(is_mesh_kvm(r"C:\Users\me\AppData\Local\mesh-kvm\mesh-kvm-recover.exe"));
        assert!(!is_mesh_kvm(r"C:\Program Files\Something\other.exe"));
        assert!(!is_mesh_kvm(r"C:\Users\me\AppData\Local\mesh-kvm\notepad.exe"), "the folder's name doesn't count");
        assert!(!is_mesh_kvm(""));
    }

    #[test]
    fn an_empty_environment_still_ends_properly() {
        assert_eq!(environment_block(Vec::new(), &[]), vec![0, 0]);
    }
}
