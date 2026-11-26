//! Windows only: create a file whose DACL holds exactly one ACE, full control
//! for the current user's SID, with inheritance blocked (`P`). Stricter than
//! the user-profile ACL the bus token relies on: the Device Signing Key must
//! not be readable by another account or by an inheriting parent folder.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The current user's SID as a string (`S-1-5-21-...`).
fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = null_mut();
    // SAFETY: `token` is a valid out pointer; the handle is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut needed = 0u32;
        // SAFETY: a null buffer of length 0 asks for the required size.
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64 elements keep the buffer aligned for TOKEN_USER.
        let mut buffer = vec![0u64; usize::try_from(needed).unwrap_or(0).div_ceil(8)];
        // SAFETY: `buffer` holds at least `needed` bytes.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful TokenUser query wrote a TOKEN_USER at the start.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut text: *mut u16 = null_mut();
        // SAFETY: `sid` is valid for the life of `buffer`; `text` is an out pointer.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `text` is a NUL-terminated wide string until freed.
        let sid_text = unsafe {
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            value
        };
        Ok(sid_text)
    })();
    // SAFETY: `token` came from OpenProcessToken.
    unsafe { CloseHandle(token) };
    result
}

/// Creates `path` (failing if it exists) for writing, with the one-ACE
/// protected DACL set at creation.
pub fn create_new(path: &Path) -> io::Result<File> {
    let sddl: Vec<u16> = format!("D:P(A;;FA;;;{})", current_user_sid()?)
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor: *mut c_void = null_mut();
    // SAFETY: `sddl` is NUL-terminated; `descriptor` is an out pointer freed below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated and `attributes` outlives the call.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: `descriptor` was allocated by the conversion call above.
    unsafe { LocalFree(descriptor) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    // SAFETY: a valid, owned file handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}
