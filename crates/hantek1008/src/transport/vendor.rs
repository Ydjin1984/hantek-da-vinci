//! Vendor-driver transport — **do not enable without reading this**.
//!
//! Hantek's `DSO6CDE` driver exposes the USB bulk pipe as a file, which is what
//! `HTNTLVDll.dll` uses. Talking to it directly from our own process is tempting
//! because nothing has to be installed, but on the hardware we tested it is
//! unstable: reads come back as `ERROR_GEN_FAILURE`, and after a couple of
//! retries the driver raises an exception in kernel mode and the machine
//! bugchecks with `0x3B` (SYSTEM_SERVICE_EXCEPTION, `0xC0000046`).
//!
//! Two blue screens were produced that way while developing this project. The
//! code is kept for reference and for machines with a different driver build,
//! and it is compiled only with `--features vendor-driver`.

use super::Transport;
use crate::{Error, Result};
use std::ffi::{c_void, CString};
use std::ptr;

type Handle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// Hantek device interface class, recovered from `HTNTLVDll.dll` (`0x100451E0`).
pub const HANTEK_INTERFACE_GUID: Guid = Guid {
    data1: 0x4B6C_73B2,
    data2: 0x015B,
    data3: 0x4855,
    data4: [0x8B, 0xB1, 0xA7, 0xA6, 0xC0, 0x3A, 0xFA, 0x87],
};

const DIGCF_PRESENT: u32 = 0x02;
const DIGCF_DEVICEINTERFACE: u32 = 0x10;
const GENERIC_READ_WRITE: u32 = 0xC000_0000;
const FILE_SHARE_RW: u32 = 0x03;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_IO_PENDING: u32 = 997;
const WAIT_TIMEOUT: u32 = 0x102;
const WAIT_OBJECT_0: u32 = 0;

#[repr(C)]
struct SpDeviceInterfaceData {
    cb_size: u32,
    interface_class_guid: Guid,
    flags: u32,
    reserved: usize,
}

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: Handle,
}

#[link(name = "setupapi")]
extern "system" {
    fn SetupDiGetClassDevsA(g: *const Guid, e: *const u8, p: Handle, f: u32) -> Handle;
    fn SetupDiEnumDeviceInterfaces(
        s: Handle,
        i: *mut c_void,
        g: *const Guid,
        n: u32,
        d: *mut SpDeviceInterfaceData,
    ) -> i32;
    fn SetupDiGetDeviceInterfaceDetailA(
        s: Handle,
        d: *mut SpDeviceInterfaceData,
        detail: *mut u8,
        size: u32,
        needed: *mut u32,
        info: *mut c_void,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(s: Handle) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileA(
        p: *const u8,
        a: u32,
        s: u32,
        sec: *mut c_void,
        c: u32,
        f: u32,
        t: Handle,
    ) -> Handle;
    fn WriteFile(h: Handle, b: *const u8, n: u32, w: *mut u32, o: *mut Overlapped) -> i32;
    fn ReadFile(h: Handle, b: *mut u8, n: u32, r: *mut u32, o: *mut Overlapped) -> i32;
    fn GetOverlappedResult(h: Handle, o: *mut Overlapped, t: *mut u32, wait: i32) -> i32;
    fn CreateEventA(s: *mut c_void, m: i32, i: i32, n: *const u8) -> Handle;
    fn ResetEvent(h: Handle) -> i32;
    fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// Enumerate device paths registered under the Hantek interface class.
pub fn device_paths() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsA(
            &HANTEK_INTERFACE_GUID,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set.is_null() || set as isize == -1 {
            return out;
        }
        let mut index = 0u32;
        loop {
            let mut iface = SpDeviceInterfaceData {
                cb_size: std::mem::size_of::<SpDeviceInterfaceData>() as u32,
                interface_class_guid: Guid { data1: 0, data2: 0, data3: 0, data4: [0; 8] },
                flags: 0,
                reserved: 0,
            };
            if SetupDiEnumDeviceInterfaces(set, ptr::null_mut(), &HANTEK_INTERFACE_GUID, index, &mut iface) == 0 {
                break;
            }
            index += 1;
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailA(
                set,
                &mut iface,
                ptr::null_mut(),
                0,
                &mut needed,
                ptr::null_mut(),
            );
            if needed == 0 || last_error() != ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            let mut buf = vec![0u8; needed as usize + 16];
            let detail_cb = if cfg!(target_pointer_width = "64") { 8u32 } else { 5u32 };
            buf[0..4].copy_from_slice(&detail_cb.to_le_bytes());
            if SetupDiGetDeviceInterfaceDetailA(
                set,
                &mut iface,
                buf.as_mut_ptr(),
                buf.len() as u32,
                ptr::null_mut(),
                ptr::null_mut(),
            ) == 0
            {
                continue;
            }
            let s = &buf[4..];
            let end = s.iter().position(|&b| b == 0).unwrap_or(0);
            if end > 0 {
                out.push(String::from_utf8_lossy(&s[..end]).into_owned());
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// Open the first scope held by the vendor driver.
pub fn open_first() -> Result<VendorTransport> {
    let paths = device_paths();
    let Some(p) = paths.first() else { return Err(Error::NoDevice) };
    VendorTransport::open(p)
}

/// File-based transport to Hantek's kernel driver. See the module warning.
pub struct VendorTransport {
    handle: Handle,
    event: Handle,
    path: String,
}

unsafe impl Send for VendorTransport {}

impl VendorTransport {
    pub fn open(path: &str) -> Result<VendorTransport> {
        let c = CString::new(path).map_err(|_| Error::Protocol("path contains NUL".into()))?;
        unsafe {
            let handle = CreateFileA(
                c.as_ptr() as *const u8,
                GENERIC_READ_WRITE,
                FILE_SHARE_RW,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            );
            if handle == (-1isize) as Handle {
                return Err(Error::Win32 { call: "CreateFileA", code: last_error() });
            }
            let event = CreateEventA(ptr::null_mut(), 1, 0, ptr::null());
            if event.is_null() {
                let e = last_error();
                CloseHandle(handle);
                return Err(Error::Win32 { call: "CreateEventA", code: e });
            }
            Ok(VendorTransport { handle, event, path: path.to_string() })
        }
    }

    fn transfer(&mut self, is_write: bool, buf: &mut [u8]) -> Result<usize> {
        unsafe {
            ResetEvent(self.event);
            let mut ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                h_event: self.event,
            };
            let mut done = 0u32;
            let ok = if is_write {
                WriteFile(self.handle, buf.as_ptr(), buf.len() as u32, &mut done, &mut ov)
            } else {
                ReadFile(self.handle, buf.as_mut_ptr(), buf.len() as u32, &mut done, &mut ov)
            };
            if ok == 0 && last_error() != ERROR_IO_PENDING {
                return Err(Error::Win32 {
                    call: if is_write { "WriteFile" } else { "ReadFile" },
                    code: last_error(),
                });
            }
            match WaitForSingleObject(self.event, 1000) {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Err(Error::Timeout("vendor transfer")),
                _ => return Err(Error::Win32 { call: "WaitForSingleObject", code: last_error() }),
            }
            let mut n = 0u32;
            if GetOverlappedResult(self.handle, &mut ov, &mut n, 0) == 0 {
                return Err(Error::Win32 { call: "GetOverlappedResult", code: last_error() });
            }
            Ok(n as usize)
        }
    }
}

impl Transport for VendorTransport {
    fn write(&mut self, buf: &[u8]) -> Result<()> {
        let mut tmp = buf.to_vec();
        let n = self.transfer(true, &mut tmp)?;
        if n != buf.len() {
            return Err(Error::Protocol(format!("short write {n}/{}", buf.len())));
        }
        Ok(())
    }

    fn read(&mut self, len: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        let n = self.transfer(false, &mut out)?;
        if n != len {
            return Err(Error::Protocol(format!("short read {n}/{len}")));
        }
        Ok(out)
    }

    fn describe(&self) -> String {
        self.path.clone()
    }
}

impl Drop for VendorTransport {
    fn drop(&mut self) {
        unsafe {
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
            if !self.handle.is_null() && self.handle != (-1isize) as Handle {
                CloseHandle(self.handle);
            }
        }
    }
}
