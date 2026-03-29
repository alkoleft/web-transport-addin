use std::error::Error;
use std::ffi::{c_long, c_ushort};

use addin1c::{CString1C, Connection};

const ADDIN_E_FAIL: c_ushort = 1006;
const GENERIC_SCODE: c_long = 1;

#[repr(C)]
struct ConnectionVTableCompat {
    dtor: usize,
    #[cfg(target_family = "unix")]
    dtor2: usize,
    add_error:
        unsafe extern "system" fn(&Connection, c_ushort, *const u16, *const u16, c_long) -> bool,
}

#[repr(C)]
struct ConnectionCompat {
    vptr1: *const ConnectionVTableCompat,
}

pub(crate) fn report_platform_error(
    connection: Option<&'static Connection>,
    source: &str,
    err: &dyn Error,
) {
    let Some(connection) = connection else {
        return;
    };

    let source = CString1C::from(source);
    let description = CString1C::from(err.to_string().as_str());
    let raw = connection as *const Connection as *const ConnectionCompat;

    unsafe {
        let vtable = (*raw).vptr1;
        if vtable.is_null() {
            return;
        }
        ((*vtable).add_error)(
            connection,
            ADDIN_E_FAIL,
            source.as_ref().as_ptr(),
            description.as_ref().as_ptr(),
            GENERIC_SCODE,
        );
    }
}
