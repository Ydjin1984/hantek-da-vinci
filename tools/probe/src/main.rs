//! Spike: can we drive the Hantek 1008C through the vendor DLL `HTNTLVDll.dll`
//! from a 32-bit process, while `Scope.exe` owns the device?
//!
//! Everything here is deliberately defensive: we only call entry points whose
//! signature is safe to guess (`int(void)`), flush after each step, and bail out
//! instead of guessing further. A crash here is contained in this process.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryA(name: *const c_char) -> *mut c_void;
    fn GetProcAddress(h: *mut c_void, name: *const c_char) -> *mut c_void;
    fn SetDllDirectoryA(path: *const c_char) -> c_int;
    fn FreeLibrary(h: *mut c_void) -> c_int;
}

const DLL_DIR: &str = r"C:\Program Files (x86)\Hantek\Scope1008";

fn sym(h: *mut c_void, name: &str) -> *mut c_void {
    let c = CString::new(name).unwrap();
    unsafe { GetProcAddress(h, c.as_ptr()) }
}

fn main() {
    println!("dso-probe: {}-bit", std::mem::size_of::<usize>() * 8);
    unsafe {
        let dir = CString::new(DLL_DIR).unwrap();
        SetDllDirectoryA(dir.as_ptr());

        let dll = CString::new("HTNTLVDll.dll").unwrap();
        let h = LoadLibraryA(dll.as_ptr());
        if h.is_null() {
            println!("LoadLibraryA failed: {}", std::io::Error::last_os_error());
            return;
        }
        println!("HTNTLVDll.dll loaded at {h:p}");

        let wanted = [
            "dsoCheck",
            "dsoInitDevice",
            "dsoSearch",
            "dsoGetARMVersion",
            "dsoGetDevEnable",
            "dsoSetCHNum",
            "dsoSetCHOpen",
            "dsoSetCHVolt",
            "dsoSetSampleRate",
            "dsoSetDivNum",
            "dsoGetDivNum",
            "dsoSetTrigger",
            "dsoSetTrigLevel",
            "dsoTriggerEnable",
            "dsoStartCaptureData",
            "dsoCaptureFinish",
            "dsoHTMiniReadNormalData",
            "dsoHTMiniReadAD",
            "dsoReadNormalData",
            "dsoGetVoltDIVFactor",
            "dsoGetRealVoltDIV",
            "dsoGetVoltZoom",
            "dsoReadCalData",
            "dsoGetNowT",
            "dsoGetCalT",
        ];
        let mut present = 0;
        for name in wanted {
            let p = sym(h, name);
            if p.is_null() {
                println!("  {name:<26} MISSING");
            } else {
                present += 1;
                println!("  {name:<26} {p:p}");
            }
        }
        println!("resolved {present}/{} entry points", wanted.len());

        // dsoCheck() is the least dangerous probe: it should only report whether
        // the driver sees the hardware.
        let check = sym(h, "dsoCheck");
        if !check.is_null() {
            print!("calling dsoCheck() ... ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let f: extern "C" fn() -> c_int = std::mem::transmute(check);
            let r = f();
            println!("-> {r}");
        }

        let init = sym(h, "dsoInitDevice");
        if !init.is_null() {
            print!("calling dsoInitDevice() ... ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let f: extern "C" fn() -> c_int = std::mem::transmute(init);
            let r = f();
            println!("-> {r}");
        }

        // If init succeeded, the ARM firmware version is usually a C string.
        if !sym(h, "dsoGetARMVersion").is_null() {
            print!("calling dsoGetARMVersion() as char* ... ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let f: extern "C" fn() -> *const c_char = std::mem::transmute(sym(h, "dsoGetARMVersion"));
            let p = f();
            if p.is_null() {
                println!("-> null");
            } else {
                let s = CStr::from_ptr(p).to_string_lossy().into_owned();
                println!("-> {s:?}");
            }
        }

        FreeLibrary(h);
    }
}
