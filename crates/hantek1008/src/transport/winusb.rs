//! WinUSB transport: the safe path.
//!
//! Bound with Zadig (or an INF), the scope appears as a plain USB device and
//! `winusb.dll` gives us raw pipe access. Nothing vendor-supplied runs in the
//! kernel, so a bad day costs us a process, not a bugcheck.

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

/// `GUID_DEVINTERFACE_USB_DEVICE`, the generic USB device interface class.
const GUID_DEVINTERFACE_USB_DEVICE: Guid = Guid {
    data1: 0xA5DC_BF10,
    data2: 0x6530,
    data3: 0x11D2,
    data4: [0x90, 0x1F, 0x00, 0xC0, 0x4F, 0xB9, 0x51, 0xED],
};

const DIGCF_PRESENT: u32 = 0x02;
const DIGCF_DEVICEINTERFACE: u32 = 0x10;
const GENERIC_READ_WRITE: u32 = 0xC000_0000;
const FILE_SHARE_RW: u32 = 0x03;
const OPEN_EXISTING: u32 = 3;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

const USB_ENDPOINT_TYPE_BULK: u32 = 2;

#[repr(C)]
struct SpDeviceInterfaceData {
    cb_size: u32,
    interface_class_guid: Guid,
    flags: u32,
    reserved: usize,
}

#[repr(C)]
#[derive(Default)]
struct UsbInterfaceDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_interface_number: u8,
    b_alternate_setting: u8,
    b_num_endpoints: u8,
    b_interface_class: u8,
    b_interface_sub_class: u8,
    b_interface_protocol: u8,
    i_interface: u8,
}

#[repr(C)]
#[derive(Default)]
struct WinusbPipeInformation {
    pipe_type: u32,
    pipe_id: u8,
    maximum_packet_size: u16,
    interval: u8,
}

#[link(name = "setupapi")]
extern "system" {
    fn SetupDiGetClassDevsA(
        class_guid: *const Guid,
        enumerator: *const u8,
        parent: Handle,
        flags: u32,
    ) -> Handle;
    fn SetupDiEnumDeviceInterfaces(
        set: Handle,
        info: *mut c_void,
        guid: *const Guid,
        index: u32,
        data: *mut SpDeviceInterfaceData,
    ) -> i32;
    fn SetupDiGetDeviceInterfaceDetailA(
        set: Handle,
        data: *mut SpDeviceInterfaceData,
        detail: *mut u8,
        size: u32,
        needed: *mut u32,
        info: *mut c_void,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(set: Handle) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileA(
        path: *const u8,
        access: u32,
        share: u32,
        security: *mut c_void,
        creation: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
}

#[link(name = "winusb")]
extern "system" {
    fn WinUsb_Initialize(device: Handle, iface: *mut Handle) -> i32;
    fn WinUsb_Free(iface: Handle) -> i32;
    fn WinUsb_QueryInterfaceSettings(
        iface: Handle,
        alt: u8,
        desc: *mut UsbInterfaceDescriptor,
    ) -> i32;
    fn WinUsb_QueryPipe(
        iface: Handle,
        alt: u8,
        index: u8,
        info: *mut WinusbPipeInformation,
    ) -> i32;
    fn WinUsb_SetPipePolicy(iface: Handle, pipe: u8, policy: u32, len: u32, value: *const c_void)
        -> i32;
    fn WinUsb_WritePipe(
        iface: Handle,
        pipe: u8,
        buf: *const u8,
        len: u32,
        transferred: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WinUsb_ReadPipe(
        iface: Handle,
        pipe: u8,
        buf: *mut u8,
        len: u32,
        transferred: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

const PIPE_TRANSFER_TIMEOUT: u32 = 3;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const ERROR_IO_PENDING: u32 = 997;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: Handle,
}

extern "system" {
    fn CreateEventA(security: *mut c_void, manual: i32, initial: i32, name: *const u8) -> Handle;
    fn ResetEvent(h: Handle) -> i32;
    fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
    fn GetOverlappedResult(h: Handle, ov: *mut Overlapped, transferred: *mut u32, wait: i32) -> i32;
    fn SetLastError(code: u32);
    fn CancelIoEx(h: Handle, ov: *mut c_void) -> i32;
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// Enumerate every USB device path whose instance id mentions the scope.
///
/// WinUSB binding changes the interface class, so we cannot filter by the
/// Hantek interface GUID any more; instead we ask for all USB devices and keep
/// the ones whose device path names VID 0783 / PID 5725.
pub fn device_paths() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsA(
            &GUID_DEVINTERFACE_USB_DEVICE,
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
            if SetupDiEnumDeviceInterfaces(set, ptr::null_mut(), &GUID_DEVINTERFACE_USB_DEVICE, index, &mut iface) == 0 {
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
            if end == 0 {
                continue;
            }
            let path = String::from_utf8_lossy(&s[..end]).into_owned();
            let lower = path.to_ascii_lowercase();
            if lower.contains("vid_0783") && lower.contains("pid_5725") {
                out.push(path);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// Open the first scope bound to WinUSB, waiting briefly for re-enumeration.
///
/// The device logically disconnects after ~7 s without commands and then
/// re-appears; a caller that arrives during that window would otherwise see
/// "not found" for a device that is physically plugged in.
pub fn open_first() -> Result<WinUsbTransport> {
    open_first_within(std::time::Duration::from_secs(6))
}

/// Same, with an explicit enumeration window.
///
/// The acquisition supervisor uses a short window for a reopen so the window can
/// report "переподключение" instead of blocking for six seconds; the caller
/// retries with a growing pause anyway.
pub fn open_first_within(timeout: std::time::Duration) -> Result<WinUsbTransport> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last: Option<Error> = None;
    loop {
        let paths = device_paths();
        if paths.is_empty() {
            last = Some(Error::NoDevice);
        }
        for p in &paths {
            match WinUsbTransport::open(p) {
                Ok(t) => return Ok(t),
                Err(e) => last = Some(e),
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(last.unwrap_or(Error::NoDevice));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Raw bulk access through `winusb.dll`.
pub struct WinUsbTransport {
    device: Handle,
    iface: Handle,
    event: Handle,
    ep_in: u8,
    ep_out: u8,
    path: String,
    overlapped: bool,
}

unsafe impl Send for WinUsbTransport {}

impl WinUsbTransport {
    pub fn open(path: &str) -> Result<WinUsbTransport> {
        // Always overlapped. The protocol needs `drain` to return after a short
        // wait, and a synchronous handle cannot do that: on machines where the
        // old "try simple first" succeeded, every command response stayed in the
        // pipe and the command stream desynchronised. A synchronous reopen is
        // not worth that risk.
        Self::open_with(path, FILE_FLAG_OVERLAPPED).map_err(|e| {
            Error::Protocol(format!("WinUSB не принял устройство: {e}"))
        })
    }

    fn open_with(path: &str, flags: u32) -> Result<WinUsbTransport> {
        let c = CString::new(path).map_err(|_| Error::Protocol("path contains NUL".into()))?;
        let overlapped = flags & FILE_FLAG_OVERLAPPED != 0;
        unsafe {
            SetLastError(0);
            let device = CreateFileA(
                c.as_ptr() as *const u8,
                GENERIC_READ_WRITE,
                FILE_SHARE_RW,
                ptr::null_mut(),
                OPEN_EXISTING,
                flags,
                ptr::null_mut(),
            );
            if device == (-1isize) as Handle {
                return Err(Error::Win32 { call: "CreateFileA", code: last_error() });
            }
            SetLastError(0);
            let mut iface: Handle = ptr::null_mut();
            if WinUsb_Initialize(device, &mut iface) == 0 {
                let e = last_error();
                CloseHandle(device);
                return Err(Error::Win32 {
                    call: if overlapped { "WinUsb_Initialize(overlapped)" } else { "WinUsb_Initialize" },
                    code: e,
                });
            }

            let mut desc = UsbInterfaceDescriptor::default();
            if WinUsb_QueryInterfaceSettings(iface, 0, &mut desc) == 0 {
                let e = last_error();
                WinUsb_Free(iface);
                CloseHandle(device);
                return Err(Error::Win32 { call: "WinUsb_QueryInterfaceSettings", code: e });
            }

            let mut ep_in = 0u8;
            let mut ep_out = 0u8;
            for i in 0..desc.b_num_endpoints {
                let mut pipe = WinusbPipeInformation::default();
                if WinUsb_QueryPipe(iface, 0, i, &mut pipe) == 0 {
                    continue;
                }
                if pipe.pipe_type == USB_ENDPOINT_TYPE_BULK {
                    if pipe.pipe_id & 0x80 != 0 && ep_in == 0 {
                        ep_in = pipe.pipe_id;
                    } else if pipe.pipe_id & 0x80 == 0 && ep_out == 0 {
                        ep_out = pipe.pipe_id;
                    }
                }
            }
            if ep_in == 0 || ep_out == 0 {
                WinUsb_Free(iface);
                CloseHandle(device);
                return Err(Error::Protocol("no bulk endpoints on interface 0".into()));
            }

            // A stuck read must fail instead of hanging the acquisition thread.
            let timeout: u32 = 1000;
            for ep in [ep_in, ep_out] {
                let _ = WinUsb_SetPipePolicy(
                    iface,
                    ep,
                    PIPE_TRANSFER_TIMEOUT,
                    std::mem::size_of::<u32>() as u32,
                    &timeout as *const u32 as *const c_void,
                );
            }

            let event = if overlapped {
                let e = CreateEventA(ptr::null_mut(), 1, 0, ptr::null());
                if e.is_null() {
                    WinUsb_Free(iface);
                    CloseHandle(device);
                    return Err(Error::Win32 { call: "CreateEventA", code: last_error() });
                }
                e
            } else {
                ptr::null_mut()
            };

            Ok(WinUsbTransport { device, iface, event, ep_in, ep_out, path: path.to_string(), overlapped })
        }
    }

    /// Read up to `max` bytes, giving up after `timeout_ms` of silence.
    pub fn read_available(&mut self, max: usize, timeout_ms: u64) -> Result<Vec<u8>> {
        if !self.overlapped {
            return Ok(Vec::new());
        }
        unsafe {
            ResetEvent(self.event);
            let mut ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                h_event: self.event,
            };
            let mut buf = vec![0u8; max.max(1)];
            let mut done = 0u32;
            let ovp = &mut ov as *mut Overlapped as *mut c_void;
            let ok = WinUsb_ReadPipe(
                self.iface,
                self.ep_in,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut done,
                ovp,
            );
            if ok == 0 && last_error() != ERROR_IO_PENDING {
                return Ok(Vec::new());
            }
            match WaitForSingleObject(self.event, timeout_ms as u32) {
                WAIT_OBJECT_0 => {
                    let mut n = 0u32;
                    if GetOverlappedResult(self.device, &mut ov, &mut n, 0) == 0 {
                        return Ok(Vec::new());
                    }
                    buf.truncate(n as usize);
                    Ok(buf)
                }
                _ => {
                    // Nothing arrived in time: cancel the pending read and reap
                    // it before the buffer goes out of scope, otherwise the
                    // cancelled IRP would write into freed memory.
                    let _ = CancelIoEx(self.device, ovp);
                    let _ = WaitForSingleObject(self.event, 200);
                    let mut n = 0u32;
                    let _ = GetOverlappedResult(self.device, &mut ov, &mut n, 1);
                    Ok(Vec::new())
                }
            }
        }
    }

    fn transfer(&mut self, is_write: bool, buf: &mut [u8]) -> Result<usize> {
        let mut done = 0u32;
        if !self.overlapped {
            let ok = unsafe {
                if is_write {
                    WinUsb_WritePipe(self.iface, self.ep_out, buf.as_ptr(), buf.len() as u32, &mut done, ptr::null_mut())
                } else {
                    WinUsb_ReadPipe(self.iface, self.ep_in, buf.as_mut_ptr(), buf.len() as u32, &mut done, ptr::null_mut())
                }
            };
            if ok == 0 {
                return Err(Error::Win32 {
                    call: if is_write { "WinUsb_WritePipe" } else { "WinUsb_ReadPipe" },
                    code: last_error(),
                });
            }
            return Ok(done as usize);
        }

        unsafe {
            ResetEvent(self.event);
            let mut ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                h_event: self.event,
            };
            let ovp = &mut ov as *mut Overlapped as *mut c_void;
            let ok = if is_write {
                WinUsb_WritePipe(self.iface, self.ep_out, buf.as_ptr(), buf.len() as u32, &mut done, ovp)
            } else {
                WinUsb_ReadPipe(self.iface, self.ep_in, buf.as_mut_ptr(), buf.len() as u32, &mut done, ovp)
            };
            if ok == 0 && last_error() != ERROR_IO_PENDING {
                return Err(Error::Win32 {
                    call: if is_write { "WinUsb_WritePipe" } else { "WinUsb_ReadPipe" },
                    code: last_error(),
                });
            }
            match WaitForSingleObject(self.event, 2000) {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Err(Error::Timeout("winusb transfer")),
                _ => return Err(Error::Win32 { call: "WaitForSingleObject", code: last_error() }),
            }
            let mut n = 0u32;
            if GetOverlappedResult(self.device, &mut ov, &mut n, 0) == 0 {
                return Err(Error::Win32 { call: "GetOverlappedResult", code: last_error() });
            }
            Ok(n as usize)
        }
    }
}

impl Transport for WinUsbTransport {
    fn write(&mut self, buf: &[u8]) -> Result<()> {
        let mut tmp = buf.to_vec();
        let n = self.transfer(true, &mut tmp)?;
        if n != buf.len() {
            return Err(Error::Protocol(format!("short write {n}/{}", buf.len())));
        }
        Ok(())
    }

    fn read(&mut self, len: usize) -> Result<Vec<u8>> {
        // Bulk transfers can come back in pieces; keep pulling until the
        // requested amount is here. A short answer is a protocol error, not a
        // reason to desynchronise the command stream silently.
        let mut out: Vec<u8> = Vec::with_capacity(len);
        while out.len() < len {
            let mut chunk = vec![0u8; len - out.len()];
            match self.transfer(false, &mut chunk) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    return Err(Error::Protocol(format!(
                        "short read {}/{} after {e}",
                        out.len(),
                        len
                    )))
                }
            }
        }
        if out.len() != len {
            return Err(Error::Protocol(format!("short read {}/{len}", out.len())));
        }
        Ok(out)
    }

    fn describe(&self) -> String {
        format!(
            "{} (bulk out {:#04x}, in {:#04x}{})",
            self.path,
            self.ep_out,
            self.ep_in,
            if self.overlapped { ", overlapped" } else { "" }
        )
    }

    fn drain(&mut self, max: usize, timeout_ms: u64) -> Result<Vec<u8>> {
        self.read_available(max, timeout_ms)
    }
}

impl Drop for WinUsbTransport {
    fn drop(&mut self) {
        unsafe {
            if !self.iface.is_null() {
                WinUsb_Free(self.iface);
            }
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
            if !self.device.is_null() && self.device != (-1isize) as Handle {
                CloseHandle(self.device);
            }
        }
    }
}
