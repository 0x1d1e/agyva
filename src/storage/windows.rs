//! Windows-only file identity and user-only DACLs.
//!
//! `std` does not expose volume serial or file index on stable, and it cannot
//! set a DACL. These calls are the control for "same file" and for private
//! token writes. A failed call is an error; nothing here falls back to
//! comparing file contents or claiming a Unix mode.

#![allow(unsafe_code)]

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, GetLastError, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_W,
};
use windows_sys::Win32::Security::{EqualSid, GetSecurityDescriptorControl, GetTokenInformation};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::fsutil::PrivateAcl;

const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER: i32 = 1;
const SE_FILE_OBJECT: i32 = 1;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
const SE_DACL_PRESENT: u16 = 0x0004;
const SE_DACL_PROTECTED: u16 = 0x1000;
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
const GENERIC_ALL: u32 = 0x1000_0000;
const SET_ACCESS: i32 = 2;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const OBJECT_INHERIT_ACE: u32 = 0x1;
const CONTAINER_INHERIT_ACE: u32 = 0x2;
const INHERITED_ACE: u8 = 0x10;

fn wide_path(path: &Path) -> Vec<u16> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}

fn os_error(context: &str) -> io::Error {
    let err = io::Error::from_raw_os_error(unsafe { GetLastError() } as i32);
    io::Error::new(err.kind(), format!("{context}: {err}"))
}

/// Volume serial + file index. If either cannot be read, return the error.
/// Do not compare contents.
pub fn same_file_index(a: &Path, b: &Path) -> io::Result<bool> {
    let id_a = file_id(a)?;
    let id_b = file_id(b)?;
    Ok(id_a == id_b)
}

fn file_id(path: &Path) -> io::Result<(u32, u64)> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return Err(os_error(&format!(
            "reading file identity for {}",
            path.display()
        )));
    }
    let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    Ok((info.dwVolumeSerialNumber, index))
}

pub fn restrict_to_current_user(path: &Path) -> io::Result<()> {
    let sid = current_user_sid()?;
    let wide = wide_path(path);
    let (permissions, inheritance) = dacl_entry(path)?;
    let mut acl = ptr::null_mut();
    let mut entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: permissions,
        grfAccessMode: SET_ACCESS,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: 0,
            TrusteeType: 1,
            ptstrName: sid.as_ptr() as *mut u16,
        },
    };
    let built = unsafe { SetEntriesInAclW(1, &mut entry, ptr::null(), &mut acl) };
    if built != ERROR_SUCCESS {
        return Err(io::Error::other(format!(
            "building a user-only ACL for {} failed ({built})",
            path.display()
        )));
    }
    let applied = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        )
    };
    unsafe {
        LocalFree(acl as HANDLE);
    }
    if applied != ERROR_SUCCESS {
        return Err(io::Error::other(format!(
            "setting a user-only ACL on {} failed ({applied})",
            path.display()
        )));
    }
    Ok(())
}

/// Mask and inheritance for one user-only allow ACE.
///
/// `FILE_ALL_ACCESS` (`0x001F01FF`) already includes the directory rights
/// (`FILE_LIST_DIRECTORY`, `FILE_TRAVERSE`, `FILE_DELETE_CHILD`). It is a
/// usable directory mask. `SetEntriesInAclW` does not convert, interpret, or
/// validate the access-mask bits. `GENERIC_ALL` is stored as given. Neither
/// that call nor `SetNamedSecurityInfoW` maps it onto the directory.
///
/// The child denial is propagation. `SetNamedSecurityInfoW` propagates
/// inheritable ACEs to existing children. Removing an inheritable ACE removes
/// those copies. If that removes every ACE, the child is left an empty DACL,
/// which denies access. `create_private_dir` does not protect the directory,
/// so `quota.lock` is created with only inherited ACEs. `grfInheritance == 0`
/// is not copied to children, so those ACEs are removed and not replaced.
/// `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE` is the fix because that ACE is
/// copied onto the existing child. Files have no children; they keep
/// `FILE_ALL_ACCESS` with no inherit flags.
fn dacl_entry(path: &Path) -> io::Result<(u32, u32)> {
    let meta = std::fs::metadata(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "reading {} before setting a user-only ACL: {e}",
                path.display()
            ),
        )
    })?;
    if meta.is_dir() {
        Ok((GENERIC_ALL, OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE))
    } else {
        Ok((FILE_ALL_ACCESS, 0))
    }
}

pub fn acl_status(path: &Path) -> PrivateAcl {
    match acl_status_inner(path) {
        Ok(status) => status,
        Err(e) => PrivateAcl::Unchecked(format!("{}: {e}", path.display())),
    }
}

fn acl_status_inner(path: &Path) -> io::Result<PrivateAcl> {
    let sid = current_user_sid()?;
    let wide = wide_path(path);
    let mut dacl = ptr::null_mut();
    let mut sd = ptr::null_mut();
    let got = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    if got != ERROR_SUCCESS {
        return Err(io::Error::other(format!(
            "reading the DACL on {} failed ({got})",
            path.display()
        )));
    }
    let result = inspect_dacl(path, sd, dacl, sid.as_ptr() as *mut std::ffi::c_void);
    unsafe {
        LocalFree(sd as HANDLE);
    }
    result
}

fn inspect_dacl(
    path: &Path,
    sd: *mut std::ffi::c_void,
    dacl: *mut windows_sys::Win32::Security::ACL,
    user_sid: *mut std::ffi::c_void,
) -> io::Result<PrivateAcl> {
    if sd.is_null() {
        return Err(io::Error::other(format!(
            "security descriptor for {} was empty",
            path.display()
        )));
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    let ok = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) };
    if ok == 0 {
        return Err(os_error(&format!(
            "reading security descriptor control for {}",
            path.display()
        )));
    }
    if control & SE_DACL_PRESENT == 0 || dacl.is_null() {
        return Ok(PrivateAcl::Shared(format!(
            "{} has no user-only DACL",
            path.display()
        )));
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Ok(PrivateAcl::Shared(format!(
            "{} DACL is not protected from inherited access",
            path.display()
        )));
    }
    let bytes = dacl as *const u8;
    let count = unsafe { std::ptr::read_unaligned(bytes.add(4) as *const u16) };
    if count == 0 {
        return Ok(PrivateAcl::Shared(format!(
            "{} DACL grants access to nobody",
            path.display()
        )));
    }
    let mut offset = 8usize;
    for _ in 0..count {
        let ace = unsafe { bytes.add(offset) };
        let ace_type = unsafe { *ace };
        let ace_flags = unsafe { *ace.add(1) };
        let ace_size = unsafe { std::ptr::read_unaligned(ace.add(2) as *const u16) } as usize;
        if ace_size < 8 {
            return Err(io::Error::other(format!(
                "truncated ACE in the DACL on {}",
                path.display()
            )));
        }
        // Object/container inherit (0x1/0x2) mark an explicit ACE as
        // inheritable. That is not INHERITED_ACE (0x10), which means the ACE
        // was copied from a parent. An inheritable allow for this user is
        // still user-only. A copied ACE, a non-allow ACE, or any other SID
        // is not: the caller must replace it with an explicit protected DACL.
        if ace_flags & INHERITED_ACE != 0 || ace_type != ACCESS_ALLOWED_ACE_TYPE {
            return Ok(PrivateAcl::Shared(format!(
                "{} DACL is not limited to the current user",
                path.display()
            )));
        }
        let ace_sid = unsafe { ace.add(8) as *mut std::ffi::c_void };
        let same = unsafe { EqualSid(ace_sid, user_sid) };
        if same == 0 {
            return Ok(PrivateAcl::Shared(format!(
                "{} DACL grants access to another account",
                path.display()
            )));
        }
        offset += ace_size;
    }
    Ok(PrivateAcl::UserOnly)
}

struct SidBuf(Vec<u8>);

impl SidBuf {
    fn as_ptr(&self) -> *mut u8 {
        self.0.as_ptr() as *mut u8
    }
}

fn current_user_sid() -> io::Result<SidBuf> {
    let mut token = ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(os_error("opening the process token"));
    }
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(token, TOKEN_USER, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        unsafe { CloseHandle(token) };
        return Err(os_error("reading the process user SID"));
    }
    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER,
            buf.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(os_error("reading the process user SID"));
    }
    // TOKEN_USER starts with a SID pointer into this buffer. Copy the SID
    // bytes so the trustee does not depend on the token buffer's layout.
    let sid_ptr = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const *mut u8) };
    if sid_ptr.is_null() {
        return Err(io::Error::other("process token has no user SID"));
    }
    let sid_len = unsafe { windows_sys::Win32::Security::GetLengthSid(sid_ptr as *mut _) };
    if sid_len == 0 {
        return Err(os_error("measuring the process user SID"));
    }
    let mut sid = vec![0u8; sid_len as usize];
    let copied = unsafe {
        windows_sys::Win32::Security::CopySid(
            sid_len,
            sid.as_mut_ptr() as *mut _,
            sid_ptr as *mut _,
        )
    };
    if copied == 0 {
        return Err(os_error("copying the process user SID"));
    }
    Ok(SidBuf(sid))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn restrict_directory_keeps_existing_child_readable_and_removable() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child.txt");
        std::fs::write(&child, b"kept").unwrap();

        restrict_to_current_user(dir.path()).unwrap();

        assert_eq!(std::fs::read(&child).unwrap(), b"kept");
        let listed = std::fs::read_dir(dir.path())
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(listed.len(), 1, "directory listing still works");
        std::fs::remove_file(&child).unwrap();
        assert!(!child.exists(), "existing child can be deleted");
        assert!(
            matches!(acl_status(dir.path()), PrivateAcl::UserOnly),
            "inheritable allow for the current user stays user-only"
        );
    }
}
