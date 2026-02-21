
mod http;
mod ws;
mod ws_client;
use std::{
    ffi::{c_int, c_long, c_void},
    sync::atomic::{AtomicI32, Ordering},
};

use addin1c::{create_component, destroy_component, name, AttachType};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub static PLATFORM_CAPABILITIES: AtomicI32 = AtomicI32::new(-1);

unsafe fn cstr1c_to_string(name: *const u16) -> String {
    if name.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    loop {
        if *name.add(len) == 0 {
            break;
        }
        len += 1;
    }
    let slice = std::slice::from_raw_parts(name, len);
    String::from_utf16_lossy(slice)
}

#[allow(non_snake_case)]
#[no_mangle]
/// # Safety
/// This function should be called from 1C.
pub unsafe extern "C" fn GetClassObject(name: *const u16, component: *mut *mut c_void) -> c_long {
    let class_name = cstr1c_to_string(name);
    match class_name.as_str() {
        "ws" => {
            let addin = ws::WsAddIn::new();
            if let Ok(addin) = addin {
                create_component(component, addin)
            } else {
                0
            }
        }
        "http" => {
            let addin = http::HttpAddIn::new();
            if let Ok(addin) = addin {
                create_component(component, addin)
            } else {
                0
            }
        }
        "mcp" => {
            let addin = http::McpServerAddIn::new();
            if let Ok(addin) = addin {
                create_component(component, addin)
            } else {
                0
            }
        }
        _ => 0,
    }
}

#[allow(non_snake_case)]
#[no_mangle]
/// # Safety
/// This function should be called from 1C.
pub unsafe extern "C" fn DestroyObject(component: *mut *mut c_void) -> c_long {
    destroy_component(component)
}

#[allow(non_snake_case)]
#[no_mangle]
pub extern "C" fn GetClassNames() -> *const u16 {
    name!("ws|http|mcp").as_ptr()
}

#[allow(non_snake_case)]
#[no_mangle]
/// # Safety
/// This function should be called from 1C.
pub unsafe extern "C" fn SetPlatformCapabilities(capabilities: c_int) -> c_int {
    PLATFORM_CAPABILITIES.store(capabilities, Ordering::Relaxed);
    3
}

#[allow(non_snake_case)]
#[no_mangle]
pub extern "C" fn GetAttachType() -> AttachType {
    AttachType::Any
}
