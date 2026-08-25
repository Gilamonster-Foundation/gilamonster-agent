//! Jupyter notebook execution + server management tool — gila-native port of
//! newt-agent's `newt-tools/src/jupyter.rs` (newt-agent PR #1730).
//!
//! The extension point is a separate binary, not a plugin slot, so the jupyter
//! surface lives HERE in gila's own tree rather than behind a newt rev bump.
//! The whole module is compiled only when the `jupyter` cargo feature is on
//! (the `pub mod gila_jupyter;` declaration in `lib.rs` is feature-gated), so
//! none of the `nbformat` / `reqwest` / `rand` / `argon2` deps reach a default
//! build.
//!
//! ## Server model
//!
//! `start_server` spawns `jupyter notebook` (through Pixi if available) bound
//! to a typed loopback address only, scrubs the child environment of gila's
//! whole control plane (`env_clear` + a minimal allowlist), and redirects both
//! output streams to a private durable log. A startup guard owns the complete
//! process tree until an authenticated readiness probe succeeds and the
//! versioned registry transaction commits. After that commit the server owns
//! its own lifetime across CLI invocations.
//!
//! `stop_server` / `get_server_status` / `list_servers` resolve opaque handles
//! through that durable registry, validate the stored loopback endpoint before
//! every request, and never treat the informational PID as authority. Stop uses
//! authenticated POST `/api/shutdown`, confirms definite listener refusal, and
//! CAS-deletes the exact registered instance. No bare-PID kill, `kill`, or
//! `taskkill` subprocess is used.
//!
//! Each server receives a random instance identity before spawn. Gila forces
//! that identity into an exact private base path and accepts the endpoint only
//! from Jupyter's bounded runtime metadata file; console URLs are diagnostics,
//! never identity evidence. The locked registry supports concurrent starts,
//! compare-and-swap stops, stale-rebind defense, and bounded concurrent list
//! probes across separate CLI processes.
//!
//! ## Pixi Integration
//!
//! When a project has `pixi.toml`, startup invokes the real executable with
//! `pixi run --executable jupyter notebook ...`. It deliberately does not
//! select a named Pixi task, so a project task called `jupyter` cannot replace
//! the executable. Pixi resolves the environment while Gila still scrubs
//! sensitive variables and appends its final controlled server options.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod windows_private {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::ptr::{addr_of, null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, SET_ACCESS,
        SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        GetTokenInformation, SetSecurityDescriptorOwner, TokenOwner, TokenUser, ACCESS_ALLOWED_ACE,
        ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SE_DACL_PROTECTED, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, FileDispositionInfo, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        DELETE, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_READ, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, SECURITY_DESCRIPTOR_REVISION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: this wrapper owns exactly one real kernel handle.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct OwnedLocal(*mut c_void);

    impl Drop for OwnedLocal {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: LocalFree owns buffers returned by the ACL/security APIs below.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    struct CurrentUserSid {
        // TOKEN_USER contains an interior SID pointer, so the aligned backing
        // allocation must remain alive for every ACL operation using `sid`.
        _storage: Vec<usize>,
        sid: PSID,
        _owner_storage: Vec<usize>,
        owner_sid: PSID,
    }

    fn current_user_sid() -> io::Result<CurrentUserSid> {
        let mut raw_token: HANDLE = null_mut();
        // SAFETY: output points to initialized storage; the process pseudo-handle is borrowed.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(raw_token);

        let mut required = 0u32;
        // SAFETY: the documented sizing call uses a null buffer and length zero.
        let sized =
            unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required) };
        if sized != 0 || required == 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::last_os_error());
        }

        let words = usize::try_from(required)
            .ok()
            .and_then(|bytes| bytes.checked_add(size_of::<usize>() - 1))
            .map(|bytes| bytes / size_of::<usize>())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token SID size overflow"))?;
        let mut storage = vec![0usize; words];
        // SAFETY: Vec<usize> provides TOKEN_USER alignment and at least `required` bytes.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                storage.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful call initialized a TOKEN_USER at the buffer start.
        let user = unsafe { &*(storage.as_ptr().cast::<TOKEN_USER>()) };
        if user.User.Sid.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "current access token has no user SID",
            ));
        }
        let mut owner_required = 0u32;
        let owner_sized =
            unsafe { GetTokenInformation(token.0, TokenOwner, null_mut(), 0, &mut owner_required) };
        if owner_sized != 0
            || owner_required == 0
            || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER
        {
            return Err(io::Error::last_os_error());
        }
        let owner_words = usize::try_from(owner_required)
            .ok()
            .and_then(|bytes| bytes.checked_add(size_of::<usize>() - 1))
            .map(|bytes| bytes / size_of::<usize>())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "token owner size overflow")
            })?;
        let mut owner_storage = vec![0usize; owner_words];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenOwner,
                owner_storage.as_mut_ptr().cast(),
                owner_required,
                &mut owner_required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let owner = unsafe { &*(owner_storage.as_ptr().cast::<TOKEN_OWNER>()) };
        if owner.Owner.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "current access token has no owner SID",
            ));
        }
        Ok(CurrentUserSid {
            _storage: storage,
            sid: user.User.Sid,
            _owner_storage: owner_storage,
            owner_sid: owner.Owner,
        })
    }

    fn private_acl(sid: PSID, is_directory: bool) -> io::Result<OwnedLocal> {
        let inheritance = if is_directory {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: inheritance,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.cast::<u16>(),
            },
        };
        let mut acl: *mut ACL = null_mut();
        // SAFETY: entry and its SID remain alive through this call; the returned ACL is LocalFree'd.
        let status = unsafe { SetEntriesInAclW(1, &entry, null(), &mut acl) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        if acl.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned a null private ACL",
            ));
        }
        Ok(OwnedLocal(acl.cast()))
    }

    fn handle(file: &File) -> HANDLE {
        file.as_raw_handle().cast()
    }

    fn verify_current_user_owner(file: &File, user_sid: PSID, owner_sid: PSID) -> io::Result<()> {
        let mut owner: PSID = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: all output pointers are valid; descriptor is LocalFree-owned on success.
        let status = unsafe {
            GetSecurityInfo(
                handle(file),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let descriptor_owner = OwnedLocal(descriptor);
        if owner.is_null() || descriptor_owner.0.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private Windows owner is missing",
            ));
        }
        // SAFETY: owner points into the live security descriptor.
        if unsafe { EqualSid(owner, user_sid) } == 0 && unsafe { EqualSid(owner, owner_sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private Windows object is not owned by the current user",
            ));
        }
        Ok(())
    }

    fn verify_private_acl(file: &File, sid: PSID, is_directory: bool) -> io::Result<()> {
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: all output pointers are valid; descriptor is LocalFree-owned on success.
        let status = unsafe {
            GetSecurityInfo(
                handle(file),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let descriptor_owner = OwnedLocal(descriptor);
        if owner.is_null() || dacl.is_null() || descriptor_owner.0.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private Windows owner or DACL is missing",
            ));
        }
        // SAFETY: owner points into the live security descriptor.
        if unsafe { EqualSid(owner, sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private Windows object is not owned by the current user",
            ));
        }

        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: descriptor is valid for the lifetime of descriptor_owner.
        if unsafe { GetSecurityDescriptorControl(descriptor_owner.0, &mut control, &mut revision) }
            == 0
            || control & SE_DACL_PROTECTED == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows DACL is not protected from parent inheritance",
            ));
        }

        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: dacl belongs to descriptor_owner; info has the documented size.
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
            || info.AceCount != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows DACL is not current-user-only",
            ));
        }

        let mut raw_ace: *mut c_void = null_mut();
        // SAFETY: the ACL has exactly one ACE and output storage is valid.
        if unsafe { GetAce(dacl, 0, &mut raw_ace) } == 0 || raw_ace.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetAce returned the first ACE; its type is checked before SID access.
        let ace = unsafe { &*(raw_ace.cast::<ACCESS_ALLOWED_ACE>()) };
        let expected_flags = if is_directory { 0x03 } else { 0 };
        if u32::from(ace.Header.AceType) != ACCESS_ALLOWED_ACE_TYPE
            || ace.Mask != FILE_ALL_ACCESS
            || ace.Header.AceFlags != expected_flags
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows DACL grants unexpected access",
            ));
        }
        let ace_sid = addr_of!(ace.SidStart).cast_mut().cast();
        // SAFETY: ACCESS_ALLOWED_ACE stores the variable-length SID at SidStart.
        if unsafe { EqualSid(ace_sid, sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows DACL belongs to a different user",
            ));
        }
        Ok(())
    }

    pub fn enforce_private_handle(file: &File, is_directory: bool) -> io::Result<()> {
        reject_reparse_or_wrong_type(file, is_directory)?;
        let user = current_user_sid()?;
        // Never mutate a foreign-owned object. Current owners have implicit
        // WRITE_DAC and may safely tighten their own inherited/permissive DACL.
        verify_current_user_owner(file, user.sid, user.owner_sid)?;
        let acl = private_acl(user.sid, is_directory)?;
        // SAFETY: handle is open with WRITE_DAC/WRITE_OWNER; ACL and SID remain alive through the call.
        let status = unsafe {
            SetSecurityInfo(
                handle(file),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION
                    | OWNER_SECURITY_INFORMATION,
                null_mut(),
                user.sid,
                acl.0.cast(),
                null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        verify_private_acl(file, user.sid, is_directory)
    }

    pub fn validate_private_handle(file: &File, is_directory: bool) -> io::Result<()> {
        reject_reparse_or_wrong_type(file, is_directory)?;
        let user = current_user_sid()?;
        verify_private_acl(file, user.sid, is_directory)
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows path contains an embedded NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    pub fn open_path_for_security(path: &Path, is_directory: bool) -> io::Result<File> {
        let wide = wide_path(path)?;
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if is_directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            };
        // SAFETY: wide is NUL-terminated and the returned handle is uniquely transferred to File.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | WRITE_OWNER,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                flags,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a newly owned handle and File closes it exactly once.
        let file = unsafe { File::from_raw_handle(raw.cast()) };
        reject_reparse_or_wrong_type(&file, is_directory)?;
        Ok(file)
    }

    /// Open the exact non-reparse object for deletion while denying delete
    /// sharing. Callers can validate its 128-bit identity and then delete this
    /// handle itself, so a same-path replacement is never the deletion target.
    pub fn open_path_for_delete(path: &Path, is_directory: bool) -> io::Result<File> {
        let wide = wide_path(path)?;
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if is_directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            };
        // SAFETY: wide is NUL-terminated and the returned handle is uniquely transferred to File.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_READ_ATTRIBUTES | READ_CONTROL | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                flags,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a newly owned handle and File closes it exactly once.
        let file = unsafe { File::from_raw_handle(raw.cast()) };
        reject_reparse_or_wrong_type(&file, is_directory)?;
        Ok(file)
    }

    /// Pin a managed directory against rename/delete while its descendants
    /// are validated through their paths. Foreign ownership is never taken:
    /// the ordinary private-ACL validation rejects it instead.
    pub fn pin_directory_for_cleanup(path: &Path) -> io::Result<File> {
        let wide = wide_path(path)?;
        // SAFETY: wide is NUL-terminated and the returned handle is uniquely transferred to File.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_READ_ATTRIBUTES | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a newly owned handle and File closes it exactly once.
        let file = unsafe { File::from_raw_handle(raw.cast()) };
        reject_reparse_or_wrong_type(&file, true)?;
        validate_private_handle(&file, true)?;
        Ok(file)
    }

    pub fn delete_open_path(file: &File) -> io::Result<()> {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: `file` is a live handle opened with DELETE access and the
        // disposition buffer has the exact documented layout and size.
        if unsafe {
            SetFileInformationByHandle(
                handle(file),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn enforce_private_path(path: &Path, is_directory: bool) -> io::Result<File> {
        let file = open_path_for_security(path, is_directory)?;
        enforce_private_handle(&file, is_directory)?;
        Ok(file)
    }

    pub fn create_private_directory(path: &Path) -> io::Result<()> {
        let user = current_user_sid()?;
        let acl = private_acl(user.sid, true)?;
        // SAFETY: SECURITY_DESCRIPTOR is initialized before use and borrows the live ACL.
        let mut descriptor: SECURITY_DESCRIPTOR = unsafe { zeroed() };
        if unsafe {
            windows_sys::Win32::Security::InitializeSecurityDescriptor(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SECURITY_DESCRIPTOR_REVISION,
            )
        } == 0
            || unsafe {
                windows_sys::Win32::Security::SetSecurityDescriptorDacl(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    1,
                    acl.0.cast(),
                    0,
                )
            } == 0
            || unsafe {
                SetSecurityDescriptorOwner(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    user.sid,
                    0,
                )
            } == 0
            || unsafe {
                windows_sys::Win32::Security::SetSecurityDescriptorControl(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                )
            } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            bInheritHandle: 0,
        };
        let wide = wide_path(path)?;
        // SAFETY: path and security descriptor remain live for the duration of the call.
        if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = enforce_private_path(path, true)?;
        Ok(())
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct FileIdentity {
        volume: u64,
        id: [u8; 16],
    }

    pub fn file_identity(file: &File) -> io::Result<FileIdentity> {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: info points to initialized, correctly sized output storage.
        if unsafe {
            GetFileInformationByHandleEx(
                handle(file),
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(FileIdentity {
            volume: info.VolumeSerialNumber,
            id: info.FileId.Identifier,
        })
    }

    pub fn reject_reparse_or_wrong_type(file: &File, is_directory: bool) -> io::Result<()> {
        // BY_HANDLE_FILE_INFORMATION is used for attributes; FILE_ID_INFO above
        // supplies the 128-bit replacement identity.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
        // SAFETY: info points to valid output storage for this live handle.
        if unsafe { GetFileInformationByHandle(handle(file), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows reparse points are not accepted for private state",
            ));
        }
        let actual_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        if actual_directory != is_directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private state path has the wrong Windows file type",
            ));
        }
        Ok(())
    }
}

/// Platform-neutral environment required to locate and render Jupyter.  The
/// child starts from `env_clear`; proxy variables, `PYTHON*`, and every
/// Gila/Newt/operator secret therefore remain absent.
const COMMON_CHILD_ENV_ALLOWLIST: &[&str] =
    &["PATH", "HOME", "USER", "LANG", "LC_ALL", "LC_CTYPE", "TERM"];

/// Windows needs its OS/profile/temp roots for WinSock and normal application
/// discovery after `env_clear`.  `PATHEXT` is required for resolving the
/// `jupyter` launcher. `COMSPEC` is deliberately omitted: Gila never invokes a
/// command shell for Jupyter startup.
#[cfg(windows)]
const WINDOWS_CHILD_ENV_ALLOWLIST: &[&str] = &[
    "SYSTEMROOT",
    "WINDIR",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "TEMP",
    "TMP",
    "APPDATA",
    "LOCALAPPDATA",
    "PATHEXT",
];

fn copy_safe_child_environment(command: &mut Command) {
    for key in COMMON_CHILD_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(windows)]
    for key in WINDOWS_CHILD_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

// ---- Registry Store (B1a) -----------------------------------------------

const REGISTRY_SCHEMA_VERSION: u32 = 1;
const REGISTRY_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const REGISTRY_LOCK_RETRY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerRecord {
    pub handle_id: u64,
    #[serde(with = "instance_id_serde")]
    pub instance_id: [u8; 16],
    pub url: String,
    pub port: u16,
    pub token: String,
    pub pid: Option<u32>,
    pub registered_at_unix_ms: u64,
    #[serde(default)]
    pub log_path: Option<String>,
    /// True only when `url` is bound to this record's random instance ID via
    /// the exact `/__gila/<instance-id>/` base path. Records written by older
    /// Gila versions deserialize as unbound and are never sent a shutdown
    /// request while their socket is accepting connections.
    #[serde(default)]
    pub identity_bound: bool,
    /// Private per-instance runtime directory, relative to `GILA_HOME`.
    /// Older records do not have one and remain readable.
    #[serde(default)]
    pub runtime_path: Option<String>,
}

mod instance_id_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(data: &[u8; 16], ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(32);
        for byte in data {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        ser.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(de: D) -> Result<[u8; 16], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        if s.len() != 32
            || !s
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(serde::de::Error::custom(
                "instance_id must be exactly 32 lowercase hex characters",
            ));
        }

        fn nibble(byte: u8) -> u8 {
            match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => unreachable!("validated lowercase hex above"),
            }
        }

        let mut bytes = [0u8; 16];
        let (pairs, remainder) = s.as_bytes().as_chunks::<2>();
        debug_assert!(remainder.is_empty());
        for (index, pair) in pairs.iter().enumerate() {
            bytes[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
        }
        Ok(bytes)
    }
}

fn deserialize_server_map<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<u64, ServerRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct UniqueServerMapVisitor;

    impl<'de> serde::de::Visitor<'de> for UniqueServerMapVisitor {
        type Value = BTreeMap<u64, ServerRecord>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a server map with unique numeric handle keys")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut servers = BTreeMap::new();
            while let Some((handle, record)) = map.next_entry::<u64, ServerRecord>()? {
                if servers.insert(handle, record).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate server handle key: {handle}"
                    )));
                }
            }
            Ok(servers)
        }
    }

    deserializer.deserialize_map(UniqueServerMapVisitor)
}

fn deserialize_string_server_map<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, ServerRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct UniqueStringServerMapVisitor;

    impl<'de> serde::de::Visitor<'de> for UniqueStringServerMapVisitor {
        type Value = BTreeMap<String, ServerRecord>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a server map with unique string handle keys")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut servers = BTreeMap::new();
            while let Some((handle, record)) = map.next_entry::<String, ServerRecord>()? {
                if servers.insert(handle.clone(), record).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate server handle key: {handle}"
                    )));
                }
            }
            Ok(servers)
        }
    }

    deserializer.deserialize_map(UniqueStringServerMapVisitor)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryFile {
    pub schema_version: u32,
    pub revision: u64,
    pub next_handle: u64,
    #[serde(deserialize_with = "deserialize_server_map")]
    pub servers: BTreeMap<u64, ServerRecord>,
}

#[derive(Debug)]
pub struct RegistryStore {
    root: PathBuf,
}

#[derive(Debug)]
pub struct RegistryTransaction {
    data: RegistryFile,
    lock_file: fs::File,
    reg_path: PathBuf,
    allocated_handles: HashSet<u64>,
    dirty: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyServerRecord {
    handle_id: u64,
    url: String,
    port: u16,
    token: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    start_time_unix: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OnDiskRegistryFile {
    schema_version: u32,
    revision: u64,
    next_handle: u64,
    #[serde(deserialize_with = "deserialize_string_server_map")]
    servers: BTreeMap<String, ServerRecord>,
}

impl OnDiskRegistryFile {
    fn into_registry(self) -> Result<RegistryFile> {
        let mut servers = BTreeMap::new();
        for (encoded_handle, record) in self.servers {
            let handle = encoded_handle
                .parse::<u64>()
                .with_context(|| format!("registry server key is not a u64: {encoded_handle}"))?;
            if servers.insert(handle, record).is_some() {
                anyhow::bail!("duplicate numeric server handle key: {handle}");
            }
        }

        Ok(RegistryFile {
            schema_version: self.schema_version,
            revision: self.revision,
            next_handle: self.next_handle,
            servers,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OnDiskRegistry {
    Current(OnDiskRegistryFile),
    Legacy(Vec<LegacyServerRecord>),
}

enum LoadOutcome {
    Current(RegistryFile),
    Missing(RegistryFile),
    Migrated(RegistryFile),
}

fn empty_registry() -> RegistryFile {
    RegistryFile {
        schema_version: REGISTRY_SCHEMA_VERSION,
        revision: 0,
        next_handle: 1,
        servers: BTreeMap::new(),
    }
}

fn validate_registry(registry: &RegistryFile) -> Result<()> {
    if registry.schema_version != REGISTRY_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported registry schema version: {}",
            registry.schema_version
        );
    }
    if registry.next_handle == 0 {
        anyhow::bail!("registry next_handle must not be zero");
    }

    for (&handle, record) in &registry.servers {
        if handle == 0 || record.handle_id == 0 {
            anyhow::bail!("registry server handles must not be zero");
        }
        if handle != record.handle_id {
            anyhow::bail!(
                "registry key {handle} does not match record handle {}",
                record.handle_id
            );
        }
        if handle >= registry.next_handle {
            anyhow::bail!(
                "registry next_handle {} must be greater than server handle {handle}",
                registry.next_handle
            );
        }
    }

    Ok(())
}

fn validate_root_path(root: &Path) -> Result<()> {
    if root.as_os_str().is_empty() {
        anyhow::bail!("registry root must not be empty");
    }
    if !root.is_absolute() {
        anyhow::bail!("registry root must be absolute: {}", root.display());
    }
    Ok(())
}

fn reject_unsafe_existing_root(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                anyhow::bail!("registry root must not be a symlink: {}", root.display());
            }
            if !metadata.is_dir() {
                anyhow::bail!("registry root must be a directory: {}", root.display());
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect registry root {}", root.display())),
    }
}

fn initialize_registry_root(root: &Path) -> Result<()> {
    validate_root_path(root)?;
    reject_unsafe_existing_root(root)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(root)
            .with_context(|| format!("failed to create registry root {}", root.display()))?;
    }
    #[cfg(windows)]
    {
        if !root.exists() {
            if let Some(parent) = root.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create registry root parent {}", parent.display())
                })?;
            }
            match windows_private::create_private_directory(root) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create private registry root {}", root.display())
                    });
                }
            }
        }
    }
    #[cfg(all(not(unix), not(windows)))]
    fs::create_dir_all(root)
        .with_context(|| format!("failed to create registry root {}", root.display()))?;

    // Check again after creation so a final-component symlink or non-directory
    // introduced during the create is never accepted on any platform.
    reject_unsafe_existing_root(root)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to set registry root permissions on {}",
                root.display()
            )
        })?;
    }

    #[cfg(windows)]
    {
        windows_private::enforce_private_path(root, true).with_context(|| {
            format!(
                "failed to enforce private Windows ACL on registry root {}",
                root.display()
            )
        })?;
    }

    Ok(())
}

fn resolve_registry_root() -> Result<PathBuf> {
    let root = match std::env::var_os("GILA_HOME") {
        Some(value) => {
            if value.is_empty() {
                anyhow::bail!("GILA_HOME must not be empty");
            }
            PathBuf::from(value)
        }
        None => directories::BaseDirs::new()
            .context("failed to determine the home directory for the registry")?
            .home_dir()
            .join(".gila"),
    };

    validate_root_path(&root)?;
    Ok(root)
}

fn open_lock_file(lock_path: &Path) -> Result<fs::File> {
    match fs::symlink_metadata(lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("registry lock file must not be a symlink")
        }
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!("registry lock path must be a regular file")
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect registry lock file"),
    }

    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let lock_file = options
        .open(lock_path)
        .context("failed to open registry lock file")?;

    #[cfg(windows)]
    {
        let secured = windows_private::enforce_private_path(lock_path, false)
            .context("failed to enforce private Windows ACL on registry lock file")?;
        validate_windows_file_identity(&lock_file, &secured)
            .context("registry lock file changed while securing it")?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        lock_file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("failed to repair registry lock file permissions")?;
    }

    Ok(lock_file)
}

fn acquire_exclusive_lock(lock_file: &fs::File, timeout: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        match fs4::FileExt::try_lock(lock_file) {
            Ok(()) => return Ok(()),
            Err(fs4::TryLockError::WouldBlock) if started.elapsed() < timeout => {
                let remaining = timeout.saturating_sub(started.elapsed());
                thread::sleep(REGISTRY_LOCK_RETRY.min(remaining));
            }
            Err(fs4::TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "timed out after {} seconds waiting for the registry lock",
                    timeout.as_secs()
                )
            }
            Err(fs4::TryLockError::Error(error)) => {
                return Err(error).context("failed to acquire registry lock")
            }
        }
    }
}

fn atomic_write_registry_with<F>(path: &Path, registry: &RegistryFile, writer: F) -> Result<()>
where
    F: FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
{
    validate_registry(registry)?;
    let encoded = serde_json::to_vec(registry).context("failed to serialize registry")?;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    #[cfg(windows)]
    let mut written_identity = None;
    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write_with_options(
            |file| {
                writer(file, &encoded)?;
                #[cfg(windows)]
                {
                    written_identity = Some(windows_private::file_identity(file)?);
                }
                Ok::<(), io::Error>(())
            },
            options,
        )
        .context("failed to write registry atomically")?;

    #[cfg(windows)]
    {
        let secured = windows_private::enforce_private_path(path, false)
            .context("failed to enforce private Windows ACL on registry file")?;
        if written_identity != Some(windows_private::file_identity(&secured)?) {
            anyhow::bail!("registry file was replaced during atomic commit");
        }
    }
    Ok(())
}

fn atomic_write_registry(path: &Path, registry: &RegistryFile) -> Result<()> {
    atomic_write_registry_with(path, registry, |file, encoded| file.write_all(encoded))
}

impl RegistryStore {
    pub fn new() -> Result<Self> {
        Self::from_root(resolve_registry_root()?)
    }

    #[cfg(test)]
    fn for_test(root: PathBuf) -> Result<Self> {
        Self::from_root(root)
    }

    fn from_root(root: PathBuf) -> Result<Self> {
        initialize_registry_root(&root)?;
        Ok(Self { root })
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("servers.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("servers.lock")
    }

    pub fn snapshot(&self) -> Result<RegistryFile> {
        Ok(self.begin_transaction()?.data.clone())
    }

    pub fn begin_transaction(&self) -> Result<RegistryTransaction> {
        self.begin_transaction_with_timeout(REGISTRY_LOCK_TIMEOUT)
    }

    fn begin_transaction_with_timeout(&self, timeout: Duration) -> Result<RegistryTransaction> {
        let lock_path = self.lock_path();
        let reg_path = self.registry_path();
        let lock_file = open_lock_file(&lock_path)?;
        acquire_exclusive_lock(&lock_file, timeout)?;

        let outcome = self.load_registry(&reg_path)?;
        let data = match outcome {
            LoadOutcome::Current(data) | LoadOutcome::Missing(data) => data,
            LoadOutcome::Migrated(data) => {
                // Migration is a committed mutation. Persist it before exposing
                // the transaction, while the same exclusive lock is still held.
                atomic_write_registry(&reg_path, &data)?;
                data
            }
        };

        Ok(RegistryTransaction {
            data,
            lock_file,
            reg_path,
            allocated_handles: HashSet::new(),
            dirty: false,
        })
    }

    fn load_registry(&self, path: &Path) -> Result<LoadOutcome> {
        #[cfg(windows)]
        let read_result = (|| -> io::Result<Vec<u8>> {
            let mut file = windows_private::enforce_private_path(path, false)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        })();
        #[cfg(not(windows))]
        let read_result = fs::read(path);

        let bytes = match read_result {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadOutcome::Missing(empty_registry()));
            }
            Err(e) => return Err(e).context("failed to read registry"),
        };

        match serde_json::from_slice::<OnDiskRegistry>(&bytes)
            .context("malformed registry: expected schema v1 or the legacy server array")?
        {
            OnDiskRegistry::Current(registry) => {
                let registry = registry.into_registry()?;
                validate_registry(&registry)?;
                Ok(LoadOutcome::Current(registry))
            }
            OnDiskRegistry::Legacy(legacy) => {
                let mut servers = BTreeMap::new();
                let mut seen = HashSet::new();

                for legacy_record in legacy {
                    let LegacyServerRecord {
                        handle_id,
                        url,
                        port,
                        token,
                        pid,
                        start_time_unix,
                    } = legacy_record;

                    if handle_id == 0 {
                        anyhow::bail!("legacy registry server handle must not be zero");
                    }
                    if !seen.insert(handle_id) {
                        anyhow::bail!("duplicate legacy registry handle: {handle_id}");
                    }

                    let mut instance_bytes = [0u8; 16];
                    use rand::RngCore;
                    rand::thread_rng().fill_bytes(&mut instance_bytes);

                    let registered_at_unix_ms = start_time_unix
                        .unwrap_or(0)
                        .checked_mul(1_000)
                        .context("legacy start_time_unix overflows milliseconds")?;

                    let rec = ServerRecord {
                        handle_id,
                        instance_id: instance_bytes,
                        url,
                        port,
                        token,
                        pid,
                        registered_at_unix_ms,
                        log_path: None,
                        identity_bound: false,
                        runtime_path: None,
                    };
                    servers.insert(handle_id, rec);
                }

                let next_handle = match servers.keys().next_back().copied() {
                    Some(maximum) => maximum
                        .checked_add(1)
                        .context("legacy registry handle overflow")?,
                    None => 1,
                };
                let registry = RegistryFile {
                    schema_version: REGISTRY_SCHEMA_VERSION,
                    // Loading legacy data and replacing it with v1 is the first
                    // committed mutation in the versioned registry.
                    revision: 1,
                    next_handle,
                    servers,
                };
                validate_registry(&registry)?;
                Ok(LoadOutcome::Migrated(registry))
            }
        }
    }
}

impl RegistryTransaction {
    pub fn snapshot(&self) -> &RegistryFile {
        &self.data
    }

    pub fn allocate_handle(&mut self) -> Result<u64> {
        let handle = self.data.next_handle;
        let next_handle = handle.checked_add(1).context("registry handle overflow")?;
        let revision = self
            .data
            .revision
            .checked_add(1)
            .context("registry revision overflow")?;

        self.data.next_handle = next_handle;
        self.data.revision = revision;
        self.allocated_handles.insert(handle);
        self.dirty = true;
        Ok(handle)
    }

    pub fn insert(&mut self, record: ServerRecord) -> Result<()> {
        let handle = record.handle_id;
        if handle == 0 {
            anyhow::bail!("Cannot insert record with zero handle");
        }
        if self.data.servers.contains_key(&handle) {
            anyhow::bail!("record with handle {handle} already exists");
        }
        if !self.allocated_handles.contains(&handle) {
            anyhow::bail!("record handle {handle} was not allocated by this transaction");
        }

        let revision = self
            .data
            .revision
            .checked_add(1)
            .context("registry revision overflow")?;
        self.data.servers.insert(handle, record);
        self.data.revision = revision;
        self.allocated_handles.remove(&handle);
        self.dirty = true;
        Ok(())
    }

    fn ensure_socket_available(&self, socket_addr: SocketAddr) -> Result<()> {
        for record in self.data.servers.values() {
            match validate_stored_record(record) {
                Ok(endpoint) if endpoint.socket_addr == socket_addr => {
                    anyhow::bail!(
                        "Jupyter socket {socket_addr} is already registered as handle {}",
                        record.handle_id
                    )
                }
                Ok(_) => {}
                Err(error) if record.port == socket_addr.port() => {
                    anyhow::bail!(
                        "cannot register Jupyter socket {socket_addr}: handle {} uses the same port but has invalid endpoint metadata: {error}",
                        record.handle_id
                    )
                }
                Err(_) => {}
            }
        }
        Ok(())
    }

    pub fn compare_and_delete(&mut self, handle: u64, instance_id: [u8; 16]) -> Result<bool> {
        let matches = self
            .data
            .servers
            .get(&handle)
            .is_some_and(|record| record.instance_id == instance_id);
        if !matches {
            return Ok(false);
        }

        let revision = self
            .data
            .revision
            .checked_add(1)
            .context("registry revision overflow")?;
        self.data.servers.remove(&handle);
        self.data.revision = revision;
        self.dirty = true;
        Ok(true)
    }

    pub fn commit(self) -> Result<()> {
        self.commit_with_writer(|file, encoded| file.write_all(encoded))
    }

    fn commit_with_writer<F>(self, writer: F) -> Result<()>
    where
        F: FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
    {
        let Self {
            data,
            lock_file,
            reg_path,
            allocated_handles: _,
            dirty,
        } = self;

        let result = if dirty {
            atomic_write_registry_with(&reg_path, &data, writer)
        } else {
            Ok(())
        };

        // Keep the exact handle that acquired the lock alive until the atomic
        // replacement (or its failure) has completed.
        drop(lock_file);
        result
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store(temp: &TempDir) -> RegistryStore {
        RegistryStore::for_test(temp.path().to_path_buf()).expect("store")
    }

    fn legacy_record(handle_id: u64, port: u16) -> serde_json::Value {
        serde_json::json!({
            "handle_id": handle_id,
            "url": format!("http://127.0.0.1:{port}"),
            "port": port,
            "token": format!("token-{handle_id}"),
            "pid": 1234,
            "start_time_unix": 2
        })
    }

    fn server_record(handle_id: u64, instance_id: [u8; 16]) -> ServerRecord {
        ServerRecord {
            handle_id,
            instance_id,
            url: "http://127.0.0.1:8888".to_string(),
            port: 8888,
            token: "token".to_string(),
            pid: Some(1234),
            registered_at_unix_ms: 2_000,
            log_path: Some("server.log".to_string()),
            identity_bound: false,
            runtime_path: None,
        }
    }

    fn assert_rejected_without_rewrite(store: &RegistryStore, original: &[u8]) {
        fs::write(store.registry_path(), original).expect("write invalid registry");
        assert!(store.begin_transaction().is_err());
        assert_eq!(
            fs::read(store.registry_path()).expect("read invalid registry"),
            original
        );
    }

    #[test]
    fn missing_registry_starts_empty_without_creating_data_file() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let tx = store.begin_transaction().expect("transaction");
        assert_eq!(tx.snapshot(), &empty_registry());
        assert!(!store.registry_path().exists());
    }

    #[test]
    fn actual_legacy_object_array_migrates_and_rewrites_immediately() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let legacy_json = serde_json::to_vec(&vec![legacy_record(1, 8888), legacy_record(2, 8889)])
            .expect("json");
        fs::write(store.registry_path(), legacy_json).expect("write legacy");

        let tx = store.begin_transaction().expect("transaction");
        assert_eq!(tx.snapshot().schema_version, REGISTRY_SCHEMA_VERSION);
        assert_eq!(tx.snapshot().revision, 1);
        assert_eq!(tx.snapshot().next_handle, 3);
        assert_eq!(tx.snapshot().servers.len(), 2);
        assert_eq!(tx.snapshot().servers[&1].registered_at_unix_ms, 2_000);
        assert!(!tx.snapshot().servers[&1].identity_bound);
        assert_eq!(tx.snapshot().servers[&1].runtime_path, None);

        // The migration must already be durable even though this transaction
        // is still holding the lock and has not been committed by the caller.
        let rewritten: RegistryFile = serde_json::from_slice(
            &fs::read(store.registry_path()).expect("read migrated registry"),
        )
        .expect("schema v1 registry");
        assert_eq!(rewritten, *tx.snapshot());
    }

    #[test]
    fn prior_schema_v1_records_deserialize_as_unbound() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let prior = serde_json::json!({
            "schema_version": 1,
            "revision": 2,
            "next_handle": 2,
            "servers": {
                "1": {
                    "handle_id": 1,
                    "instance_id": "01010101010101010101010101010101",
                    "url": "http://127.0.0.1:8888/",
                    "port": 8888,
                    "token": "legacy-token",
                    "pid": 123,
                    "registered_at_unix_ms": 2,
                    "log_path": null
                }
            }
        });
        fs::write(store.registry_path(), serde_json::to_vec(&prior).unwrap()).unwrap();
        let snapshot = store.snapshot().expect("prior schema-v1 snapshot");
        assert!(!snapshot.servers[&1].identity_bound);
        assert_eq!(snapshot.servers[&1].runtime_path, None);
    }

    #[test]
    fn malformed_registry_is_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        assert_rejected_without_rewrite(&store, b"{ broken json");
    }

    #[test]
    fn unsupported_schema_is_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let unsupported = serde_json::to_vec(&RegistryFile {
            schema_version: 2,
            revision: 0,
            next_handle: 1,
            servers: BTreeMap::new(),
        })
        .expect("json");
        assert_rejected_without_rewrite(&store, &unsupported);
    }

    #[test]
    fn zero_legacy_handle_is_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let zero = serde_json::to_vec(&vec![legacy_record(0, 8888)]).expect("json");
        assert_rejected_without_rewrite(&store, &zero);
    }

    #[test]
    fn duplicate_legacy_handles_are_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let duplicate = serde_json::to_vec(&vec![legacy_record(1, 8888), legacy_record(1, 8889)])
            .expect("json");
        assert_rejected_without_rewrite(&store, &duplicate);
    }

    #[test]
    fn legacy_handle_overflow_is_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let overflow = serde_json::to_vec(&vec![legacy_record(u64::MAX, 8888)]).expect("json");
        assert_rejected_without_rewrite(&store, &overflow);
    }

    #[test]
    fn duplicate_current_server_keys_are_preserved_byte_for_byte() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let record = serde_json::to_string(&server_record(1, [1; 16])).expect("record json");
        let duplicate = format!(
            "{{\"schema_version\":1,\"revision\":0,\"next_handle\":2,\"servers\":{{\"1\":{record},\"1\":{record}}}}}"
        );
        assert_rejected_without_rewrite(&store, duplicate.as_bytes());
    }

    #[test]
    fn handles_remain_monotonic_across_delete_and_reopen() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);

        let mut tx = store.begin_transaction().expect("tx1");
        let h1 = tx.allocate_handle().expect("h1");
        assert_eq!(h1, 1);
        tx.insert(server_record(h1, [1; 16])).expect("insert");
        tx.commit().expect("commit1");

        let mut delete_tx = store.begin_transaction().expect("delete tx");
        assert!(delete_tx.compare_and_delete(h1, [1; 16]).expect("delete"));
        delete_tx.commit().expect("delete commit");

        let mut reopen_tx = store.begin_transaction().expect("reopen tx");
        assert!(reopen_tx.snapshot().servers.is_empty());
        assert_eq!(reopen_tx.snapshot().revision, 3);
        assert!(reopen_tx.insert(server_record(h1, [2; 16])).is_err());
        let h2 = reopen_tx.allocate_handle().expect("h2");
        assert_eq!(h2, 2);
        reopen_tx.commit().expect("commit h2");

        let snapshot = store.snapshot().expect("snapshot");
        assert_eq!(snapshot.next_handle, 3);
        assert_eq!(snapshot.revision, 4);
    }

    #[test]
    fn instance_mismatch_preserves_the_record_and_exact_file() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);

        let mut tx = store.begin_transaction().expect("tx1");
        let h = tx.allocate_handle().expect("h");
        tx.insert(server_record(h, [1; 16])).expect("insert");
        tx.commit().expect("commit1");
        let before = fs::read(store.registry_path()).expect("read before mismatch");

        let mut tx2 = store.begin_transaction().expect("tx2");
        let deleted = tx2
            .compare_and_delete(h, [2; 16])
            .expect("compare_and_delete");
        assert!(!deleted);
        assert!(tx2.snapshot().servers.contains_key(&h));
        tx2.commit().expect("no-op commit");
        assert_eq!(
            fs::read(store.registry_path()).expect("read after mismatch"),
            before
        );
    }

    #[test]
    fn instance_id_serialization_is_lowercase_and_roundtrips_exactly() {
        let instance_id = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0xfe, 0xff,
        ];
        let record = server_record(1, instance_id);
        let encoded = serde_json::to_string(&record).expect("serialize record");
        assert!(encoded.contains("000102030405060708090a0b0c0dfeff"));
        let decoded: ServerRecord = serde_json::from_str(&encoded).expect("deserialize record");
        assert_eq!(decoded.instance_id, instance_id);
    }

    #[test]
    fn failed_atomic_commit_preserves_the_prior_valid_registry() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);

        let mut initial = store.begin_transaction().expect("initial tx");
        let handle = initial.allocate_handle().expect("handle");
        initial
            .insert(server_record(handle, [3; 16]))
            .expect("insert");
        initial.commit().expect("initial commit");
        let before = fs::read(store.registry_path()).expect("read prior registry");

        let mut failing = store.begin_transaction().expect("failing tx");
        failing.allocate_handle().expect("allocate mutation");
        let error = failing
            .commit_with_writer(|file, _encoded| {
                file.write_all(b"partial replacement")?;
                Err(io::Error::other("forced commit failure"))
            })
            .expect_err("forced writer failure must propagate");
        assert!(error
            .to_string()
            .contains("failed to write registry atomically"));
        assert_eq!(
            fs::read(store.registry_path()).expect("read preserved registry"),
            before
        );
    }

    #[test]
    fn a_second_transaction_cannot_bypass_the_exclusive_lock() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let first = store.begin_transaction().expect("first transaction");

        let error = store
            .begin_transaction_with_timeout(Duration::from_millis(50))
            .expect_err("second transaction must not acquire the held lock");
        assert!(error.to_string().contains("timed out"));

        drop(first);
        store
            .begin_transaction_with_timeout(Duration::from_millis(50))
            .expect("dropping the first transaction releases the lock");
    }

    #[test]
    #[cfg(unix)]
    fn unix_permissions_are_repaired_and_created_securely() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().expect("temp dir");
        let root = tmp.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o755)).expect("loosen root");

        let store = test_store(&tmp);
        let root_perms = fs::metadata(root).expect("root meta").permissions();
        assert_eq!(root_perms.mode() & 0o777, 0o700);

        fs::write(store.lock_path(), b"").expect("create permissive lock file");
        fs::set_permissions(store.lock_path(), fs::Permissions::from_mode(0o666))
            .expect("loosen lock file");

        let mut tx = store.begin_transaction().expect("tx");
        tx.allocate_handle().expect("h");
        tx.commit().expect("commit");

        let reg_perms = fs::metadata(store.registry_path())
            .expect("reg meta")
            .permissions();
        assert_eq!(reg_perms.mode() & 0o777, 0o600);

        let lock_perms = fs::metadata(store.lock_path())
            .expect("lock meta")
            .permissions();
        assert_eq!(lock_perms.mode() & 0o777, 0o600);
    }

    #[test]
    fn explicit_test_root_must_be_absolute_and_a_directory() {
        assert!(RegistryStore::for_test(PathBuf::new()).is_err());
        assert!(RegistryStore::for_test(PathBuf::from("relative")).is_err());

        let tmp = TempDir::new().expect("temp dir");
        let file = tmp.path().join("not-a-directory");
        fs::write(&file, b"file").expect("write file");
        assert!(RegistryStore::for_test(file).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn explicit_test_root_rejects_a_final_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");
        let target = tmp.path().join("target");
        fs::create_dir(&target).expect("target directory");
        let link = tmp.path().join("registry-link");
        symlink(&target, &link).expect("directory symlink");
        assert!(RegistryStore::for_test(link).is_err());
    }
}

/// `gila jupyter …` subcommands. The whole enum (and the `Jupyter` arm of the
/// top-level `Command`) is compiled out unless the `jupyter` feature is on.
#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
pub enum JupyterCmd {
    /// Execute a Jupyter notebook (.ipynb) in place via `jupyter nbconvert`.
    Execute {
        /// Path to the notebook file (.ipynb).
        notebook_path: PathBuf,
        /// Working directory for execution (default: the notebook's parent dir).
        #[arg(long)]
        working_dir: Option<String>,
        /// Per-cell execution timeout in seconds (default: 300).
        #[arg(long)]
        timeout: Option<u64>,
        /// Kernel name (default: python3).
        #[arg(long)]
        kernel: Option<String>,
        /// Do not save outputs into the notebook (default: outputs are saved).
        #[arg(long)]
        no_save_outputs: bool,
    },
    /// Start a durable Jupyter notebook server bound to a typed loopback IP.
    Start {
        /// Working directory for the server (default: current directory).
        #[arg(long)]
        working_dir: Option<String>,
        /// Port to run the server on (default: 8888).
        #[arg(long)]
        port: Option<u16>,
        /// Bind address — must be a typed loopback IP (for example 127.0.0.1 or ::1).
        #[arg(long)]
        host: Option<String>,
        /// Auth token. Headless use requires this, --password, or
        /// --password-hash; --open-browser may generate a private token.
        #[arg(long)]
        token: Option<String>,
        /// Plaintext password; hashed with argon2 before being passed to jupyter.
        #[arg(long)]
        password: Option<String>,
        /// Already-hashed `argon2:$argon2id$…` PHC string (takes precedence over
        /// `--password`).
        #[arg(long)]
        password_hash: Option<String>,
        /// Open the operator's browser on start. This permits Gila to generate
        /// initial authentication when no explicit credential was supplied.
        #[arg(long)]
        open_browser: bool,
        /// Extra flags. Only exact --ServerApp.default_url spellings are allowed.
        #[arg(long, value_delimiter = ' ')]
        extra: Option<Vec<String>>,
    },
    /// Stop a Jupyter server by its handle id (from `gila jupyter start`).
    Stop {
        /// Opaque handle id returned by `gila jupyter start`.
        handle_id: u64,
    },
    /// Query a Jupyter server's status + kernels by handle id.
    Status {
        /// Opaque handle id returned by `gila jupyter start`.
        handle_id: u64,
    },
    /// List all durable Jupyter registrations, including unreachable servers.
    List,
    /// Bootstrap: modernize pixi.toml from [project] to [workspace] syntax.
    Bootstrap {
        /// Auto-confirm the update (preview-only by default).
        #[arg(long)]
        confirm: bool,
        /// Working directory (default: current directory).
        #[arg(long)]
        working_dir: Option<String>,
    },
}

// ---- Durable Logs & Process Management (B2a) ----------------------------

pub type OwnedChild = Box<dyn process_wrap::std::ChildWrapper>;

#[cfg(unix)]
// `process-wrap` exposes `killpg(2)` failures as `io::Error`; ESRCH is the
// platform errno indicating that the process group no longer exists.
const ESRCH_RAW_OS_ERROR: i32 = 3;

fn process_tree_already_exited(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(ESRCH_RAW_OS_ERROR)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

fn transient_process_cleanup_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

const PROCESS_CLEANUP_ATTEMPTS: usize = 3;
const PROCESS_CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(20);

/// Spawn a child in an explicitly managed process group (Unix) or job object
/// (Windows). The selected wrappers do not kill on drop/handle close; callers
/// must retain the returned ownership and explicitly terminate it when needed.
pub fn spawn_owned(command: Command) -> io::Result<OwnedChild> {
    let mut command = process_wrap::std::CommandWrap::from(command);

    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());

    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);

    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "owned process trees require a Unix process group or Windows job object",
    ));

    #[cfg(any(unix, windows))]
    command.spawn()
}

#[derive(Debug)]
pub struct StartupGuard {
    child: Option<OwnedChild>,
    termination_dispatched: bool,
}

impl StartupGuard {
    /// Spawn and arm in one operation so there is no unguarded post-spawn gap.
    pub fn spawn(command: Command) -> io::Result<Self> {
        spawn_owned(command).map(Self::armed)
    }

    pub fn armed(child: OwnedChild) -> Self {
        Self {
            child: Some(child),
            termination_dispatched: false,
        }
    }

    pub fn id(&self) -> u32 {
        self.child
            .as_ref()
            .expect("child always present until disarmed")
            .id()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .expect("child always present until disarmed")
            .try_wait()
    }

    pub fn into_child(mut self) -> OwnedChild {
        self.child
            .take()
            .expect("child always present until disarmed")
    }

    pub fn disarm(self) -> OwnedChild {
        self.into_child()
    }

    /// Terminate and reap the complete owned process tree.
    ///
    /// Termination dispatches through the outer `ProcessGroupChild` /
    /// `JobObjectChild`, and the subsequent wait observes the complete tree
    /// exiting. Unix `ESRCH` means that tree has already exited, so it proceeds
    /// directly to reaping. Transient dispatch errors receive bounded retries;
    /// all failures retain the exact owned child so `Drop` can make one final
    /// cleanup attempt instead of losing the only process-tree handle.
    pub fn rollback(mut self) -> io::Result<()> {
        self.terminate_and_wait()
    }

    fn terminate_and_wait(&mut self) -> io::Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        if !self.termination_dispatched {
            let child = self.child.as_mut().expect("checked child presence");
            let mut attempt = 0usize;
            loop {
                attempt += 1;
                match child.start_kill() {
                    Ok(()) => break,
                    Err(error) if process_tree_already_exited(&error) => break,
                    Err(error)
                        if transient_process_cleanup_error(&error)
                            && attempt < PROCESS_CLEANUP_ATTEMPTS =>
                    {
                        thread::sleep(PROCESS_CLEANUP_RETRY_DELAY);
                    }
                    Err(error) => return Err(error),
                }
            }
            self.termination_dispatched = true;
        }
        self.child
            .as_mut()
            .expect("checked child presence")
            .wait()?;
        self.child.take();
        Ok(())
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        // Explicit post-spawn error paths call `rollback` so cleanup errors can
        // be returned to the caller. Drop remains only the last-resort safety
        // net for unwind / early-return paths that cannot report another error.
        if let Err(error) = self.terminate_and_wait() {
            eprintln!(
                "warning: final Jupyter process-tree cleanup retry failed; operator cleanup may be required: {error}"
            );
        }
    }
}

/// Maximum operator-visible diagnostic text returned from a failed start.
pub const LOG_DIAGNOSTIC_TAIL_CAP: usize = 8 * 1024;
/// Authentication values are bounded before any hashing, artifact creation, or
/// process spawn. A fully percent-encoded representation is at most three
/// bytes per input byte, so this also gives the log redactor a finite overlap.
const AUTH_SECRET_INPUT_CAP: usize = 4 * 1024;
const LOG_REDACTION_OVERLAP_CAP: usize = AUTH_SECRET_INPUT_CAP * 3;
/// Retain enough bytes before the visible tail to recognize a maximum-sized
/// secret that straddles the final 8 KiB boundary. Redaction happens before the
/// visible tail is truncated.
const LOG_DIAGNOSTIC_RETAIN_CAP: usize = LOG_DIAGNOSTIC_TAIL_CAP + LOG_REDACTION_OVERLAP_CAP;
pub const LOG_PARTIAL_LINE_CAP: usize = 8 * 1024;
const START_LOG_CREATE_ATTEMPTS: usize = 16;
const LOG_SCAN_CHUNK_SIZE: usize = 4 * 1024;
/// One readiness poll cannot be monopolized by a noisy child. Together these
/// caps bound both file work and the returned `Vec<String>` allocation.
const LOG_SCAN_POLL_BYTE_CAP: usize = 64 * 1024;
const LOG_SCAN_POLL_LINE_CAP: usize = 1_024;
const TRUNCATED_LOG_LINE_PREFIX: &str = "[...truncated...] ";

struct DurableLog {
    logs_dir: PathBuf,
}

#[derive(Debug)]
pub struct LogHandles {
    pub stdout_append: fs::File,
    pub stderr_append: fs::File,
    pub read_handle: fs::File,
    pub log_path: String,
}

impl DurableLog {
    fn new(store: &RegistryStore) -> Result<Self> {
        // Revalidate the root at point of use; the store never exposes it to
        // callers, and the two new path components are each checked without
        // following a pre-existing final-component symlink.
        initialize_registry_root(&store.root)?;
        let jupyter_dir = store.root.join("jupyter");
        ensure_private_directory(&jupyter_dir, "jupyter log parent")?;
        let logs_dir = jupyter_dir.join("logs");
        ensure_private_directory(&logs_dir, "jupyter logs")?;

        Ok(Self { logs_dir })
    }

    fn create_handles(&self) -> Result<LogHandles> {
        for _ in 0..START_LOG_CREATE_ATTEMPTS {
            let name = random_start_log_name();
            let absolute_path = self.logs_dir.join(&name);
            let created_file = match create_secure_log(&absolute_path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create durable log {}", absolute_path.display())
                    });
                }
            };
            let created_metadata = created_file
                .metadata()
                .context("failed to inspect newly created durable log")?;

            let stdout_append = open_existing_log(
                &absolute_path,
                &created_metadata,
                &created_file,
                ExistingLogAccess::Append,
            )
            .context("failed to open independent durable stdout log handle")?;
            let stderr_append = open_existing_log(
                &absolute_path,
                &created_metadata,
                &created_file,
                ExistingLogAccess::Append,
            )
            .context("failed to open independent durable stderr log handle")?;
            let read_handle = open_existing_log(
                &absolute_path,
                &created_metadata,
                &created_file,
                ExistingLogAccess::Read,
            )
            .context("failed to open independent durable log read handle")?;
            drop(created_file);

            return Ok(LogHandles {
                stdout_append,
                stderr_append,
                read_handle,
                log_path: format!("jupyter/logs/{name}"),
            });
        }

        anyhow::bail!(
            "failed to allocate a unique durable start log after {START_LOG_CREATE_ATTEMPTS} attempts"
        )
    }
}

impl RegistryStore {
    /// Create the durable log handles for one server-start attempt without
    /// exposing the trusted registry root to callers.
    pub fn create_start_log(&self) -> Result<LogHandles> {
        DurableLog::new(self)?.create_handles()
    }
}

fn ensure_private_directory(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("{description} directory must not be a symlink")
        }
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!("{description} path must be a directory")
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match create_private_directory(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to create {description} directory {}",
                            path.display()
                        )
                    });
                }
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect {description} directory {}",
                    path.display()
                )
            });
        }
    }

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to re-inspect {description} directory"))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("{description} directory must not be a symlink");
    }
    if !metadata.is_dir() {
        anyhow::bail!("{description} path must be a directory");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to repair {description} directory permissions on {}",
                path.display()
            )
        })?;
    }

    #[cfg(windows)]
    {
        windows_private::enforce_private_path(path, true).with_context(|| {
            format!(
                "failed to enforce private Windows ACL on {description} directory {}",
                path.display()
            )
        })?;
    }

    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }

    #[cfg(windows)]
    {
        windows_private::create_private_directory(path)
    }

    #[cfg(all(not(unix), not(windows)))]
    fs::create_dir(path)
}

fn random_start_log_name() -> String {
    use rand::RngCore;

    let mut random_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut random_bytes);
    format!("start-{:032x}.log", u128::from_be_bytes(random_bytes))
}

fn create_secure_log(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let file = options.open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    #[cfg(windows)]
    {
        let secured = windows_private::enforce_private_path(path, false)?;
        validate_windows_file_identity(&file, &secured)?;
    }

    Ok(file)
}

#[derive(Clone, Copy)]
enum ExistingLogAccess {
    Append,
    Read,
}

fn open_existing_log(
    path: &Path,
    created_metadata: &fs::Metadata,
    created_file: &fs::File,
    access: ExistingLogAccess,
) -> io::Result<fs::File> {
    validate_log_path(path, created_metadata)?;

    let mut options = fs::OpenOptions::new();
    match access {
        ExistingLogAccess::Append => {
            options.write(true).append(true);
        }
        ExistingLogAccess::Read => {
            options.read(true);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    validate_opened_log(path, created_metadata, created_file, &file)?;
    Ok(file)
}

fn validate_log_path(path: &Path, created_metadata: &fs::Metadata) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "durable log path is not a regular non-symlink file",
        ));
    }

    validate_log_identity(created_metadata, &path_metadata)
}

fn validate_opened_log(
    path: &Path,
    created_metadata: &fs::Metadata,
    created_file: &fs::File,
    file: &fs::File,
) -> io::Result<()> {
    #[cfg(not(windows))]
    let _ = created_file;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened durable log handle is not a regular file",
        ));
    }
    validate_log_identity(created_metadata, &opened_metadata)?;

    #[cfg(windows)]
    {
        windows_private::reject_reparse_or_wrong_type(file, false)?;
        validate_windows_file_identity(created_file, file)?;
        let final_handle = windows_private::open_path_for_security(path, false)?;
        validate_windows_file_identity(created_file, &final_handle)?;
    }

    // A final path check catches replacement between the pre-open inspection
    // and open. On Unix, inode/device equality also proves the opened handle is
    // the exact create_new file rather than a followed replacement symlink.
    validate_log_path(path, created_metadata)
}

#[cfg(windows)]
fn validate_windows_file_identity(expected: &fs::File, candidate: &fs::File) -> io::Result<()> {
    if windows_private::file_identity(expected)? != windows_private::file_identity(candidate)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file was replaced while opening independent handles",
        ));
    }
    Ok(())
}

fn validate_open_file_identity(expected: &fs::File, candidate: &fs::File) -> io::Result<()> {
    validate_file_identity(&expected.metadata()?, &candidate.metadata()?)?;
    #[cfg(windows)]
    validate_windows_file_identity(expected, candidate)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OwnedFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    windows: windows_private::FileIdentity,
}

#[cfg(unix)]
fn owned_file_identity(file: &fs::File) -> io::Result<OwnedFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(OwnedFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn owned_file_identity(file: &fs::File) -> io::Result<OwnedFileIdentity> {
    Ok(OwnedFileIdentity {
        windows: windows_private::file_identity(file)?,
    })
}

#[cfg(all(not(unix), not(windows)))]
fn owned_file_identity(_file: &fs::File) -> io::Result<OwnedFileIdentity> {
    Ok(OwnedFileIdentity {})
}

fn validate_owned_file_identity(
    expected: &OwnedFileIdentity,
    candidate: &fs::File,
) -> io::Result<()> {
    if *expected != owned_file_identity(candidate)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file is not the retained created object",
        ));
    }
    Ok(())
}

fn validate_log_identity(
    created_metadata: &fs::Metadata,
    candidate_metadata: &fs::Metadata,
) -> io::Result<()> {
    validate_file_identity(created_metadata, candidate_metadata)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if candidate_metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "durable log permissions changed while opening handles",
            ));
        }
    }

    #[cfg(not(unix))]
    let _ = (created_metadata, candidate_metadata);

    Ok(())
}

fn validate_file_identity(
    expected_metadata: &fs::Metadata,
    candidate_metadata: &fs::Metadata,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if expected_metadata.dev() != candidate_metadata.dev()
            || expected_metadata.ino() != candidate_metadata.ino()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file was replaced while opening independent handles",
            ));
        }
    }

    #[cfg(not(unix))]
    let _ = (expected_metadata, candidate_metadata);

    Ok(())
}

#[derive(Debug)]
pub struct IncrementalLogScanner {
    file: fs::File,
    offset: u64,
    partial_line: Vec<u8>,
    partial_line_truncated: bool,
    diagnostic_tail: Vec<u8>,
}

impl IncrementalLogScanner {
    pub fn new(file: fs::File) -> Self {
        Self {
            file,
            offset: 0,
            partial_line: Vec::new(),
            partial_line_truncated: false,
            diagnostic_tail: Vec::new(),
        }
    }

    /// Read only bytes appended since the prior poll and return newly completed
    /// lines. A shrink is treated as a truncation and starts a fresh stream.
    pub fn poll_lines(&mut self) -> io::Result<Vec<String>> {
        let file_len = self.file.metadata()?.len();
        if file_len < self.offset {
            self.offset = 0;
            self.partial_line.clear();
            self.partial_line_truncated = false;
            self.diagnostic_tail.clear();
        }

        self.file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes_available = file_len.saturating_sub(self.offset);
        let mut bytes_processed = 0usize;
        let mut chunk = [0u8; LOG_SCAN_CHUNK_SIZE];
        let mut lines = Vec::new();
        while bytes_available != 0
            && bytes_processed < LOG_SCAN_POLL_BYTE_CAP
            && lines.len() < LOG_SCAN_POLL_LINE_CAP
        {
            let poll_remaining = LOG_SCAN_POLL_BYTE_CAP - bytes_processed;
            let read_size = usize::try_from(
                bytes_available.min(LOG_SCAN_CHUNK_SIZE.min(poll_remaining) as u64),
            )
            .unwrap_or(LOG_SCAN_CHUNK_SIZE.min(poll_remaining));
            let bytes_read = self.file.read(&mut chunk[..read_size])?;
            if bytes_read == 0 {
                break;
            }

            let appended = &chunk[..bytes_read];
            let consumed =
                self.consume_appended_bytes(appended, &mut lines, LOG_SCAN_POLL_LINE_CAP);
            self.retain_diagnostic_tail(&appended[..consumed]);
            self.offset = self
                .offset
                .saturating_add(u64::try_from(consumed).unwrap_or(u64::MAX));
            bytes_available = bytes_available.saturating_sub(consumed as u64);
            bytes_processed = bytes_processed.saturating_add(consumed);
            if consumed < bytes_read {
                break;
            }
        }

        Ok(lines)
    }

    pub fn diagnostic_tail(&self) -> &[u8] {
        &self.diagnostic_tail
    }

    fn consume_appended_bytes(
        &mut self,
        appended: &[u8],
        lines: &mut Vec<String>,
        line_budget: usize,
    ) -> usize {
        let mut start = 0;
        for (index, byte) in appended.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }

            self.retain_partial_suffix(&appended[start..index]);
            let mut line = &self.partial_line[..];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line);
            if self.partial_line_truncated {
                lines.push(format!("{TRUNCATED_LOG_LINE_PREFIX}{line}"));
            } else {
                lines.push(line.into_owned());
            }
            self.partial_line.clear();
            self.partial_line_truncated = false;
            start = index + 1;
            if lines.len() >= line_budget {
                return start;
            }
        }

        self.retain_partial_suffix(&appended[start..]);
        appended.len()
    }

    fn retain_partial_suffix(&mut self, appended: &[u8]) {
        if appended.len() >= LOG_PARTIAL_LINE_CAP {
            self.partial_line.clear();
            self.partial_line
                .extend_from_slice(&appended[appended.len() - LOG_PARTIAL_LINE_CAP..]);
            self.partial_line_truncated = true;
            return;
        }

        let overflow = self
            .partial_line
            .len()
            .saturating_add(appended.len())
            .saturating_sub(LOG_PARTIAL_LINE_CAP);
        if overflow != 0 {
            self.partial_line.drain(..overflow);
            self.partial_line_truncated = true;
        }
        self.partial_line.extend_from_slice(appended);
    }

    fn retain_diagnostic_tail(&mut self, appended: &[u8]) {
        if appended.len() >= LOG_DIAGNOSTIC_RETAIN_CAP {
            self.diagnostic_tail.clear();
            self.diagnostic_tail
                .extend_from_slice(&appended[appended.len() - LOG_DIAGNOSTIC_RETAIN_CAP..]);
            return;
        }

        let overflow = self
            .diagnostic_tail
            .len()
            .saturating_add(appended.len())
            .saturating_sub(LOG_DIAGNOSTIC_RETAIN_CAP);
        if overflow != 0 {
            self.diagnostic_tail.drain(..overflow);
        }
        self.diagnostic_tail.extend_from_slice(appended);
    }
}

#[cfg(test)]
mod durable_log_tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store(tmp: &TempDir) -> RegistryStore {
        RegistryStore::for_test(tmp.path().join("registry")).expect("test registry")
    }

    fn absolute_log_path(store: &RegistryStore, handles: &LogHandles) -> PathBuf {
        store.root.join(Path::new(&handles.log_path))
    }

    #[test]
    fn start_log_names_are_unique_lowercase_hex() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let first = store.create_start_log().expect("first log");
        let second = store.create_start_log().expect("second log");

        assert_ne!(first.log_path, second.log_path);
        for handles in [&first, &second] {
            let id = handles
                .log_path
                .strip_prefix("jupyter/logs/start-")
                .and_then(|value| value.strip_suffix(".log"))
                .expect("trusted relative start-log path");
            assert_eq!(id.len(), 32);
            assert!(id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            assert!(absolute_log_path(&store, handles).is_file());
        }
    }

    #[test]
    #[cfg(unix)]
    fn symlink_components_are_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");
        let first_store = test_store(&tmp);
        let target = tmp.path().join("symlink-target");
        fs::create_dir(&target).expect("symlink target");
        symlink(&target, first_store.root.join("jupyter")).expect("jupyter symlink");
        assert!(first_store.create_start_log().is_err());

        let second_root = tmp.path().join("second-registry");
        let second_store = RegistryStore::for_test(second_root.clone()).expect("second registry");
        fs::create_dir(second_root.join("jupyter")).expect("jupyter directory");
        symlink(&target, second_root.join("jupyter/logs")).expect("logs symlink");
        assert!(second_store.create_start_log().is_err());
    }

    #[test]
    fn non_directory_components_are_rejected() {
        let tmp = TempDir::new().expect("temp dir");
        let first_store = test_store(&tmp);
        fs::write(first_store.root.join("jupyter"), b"not a directory").expect("jupyter file");
        assert!(first_store.create_start_log().is_err());

        let second_root = tmp.path().join("second-registry");
        let second_store = RegistryStore::for_test(second_root.clone()).expect("second registry");
        fs::create_dir(second_root.join("jupyter")).expect("jupyter directory");
        fs::write(second_root.join("jupyter/logs"), b"not a directory").expect("logs file");
        assert!(second_store.create_start_log().is_err());
    }

    #[test]
    #[cfg(unix)]
    fn unix_directories_and_logs_have_private_modes() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let handles = store.create_start_log().expect("start log");
        let directories = [store.root.join("jupyter"), store.root.join("jupyter/logs")];
        for directory in &directories {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
                .expect("loosen directory mode");
        }
        store.create_start_log().expect("repair directory modes");

        for directory in directories {
            let mode = fs::metadata(directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }

        let mode = fs::metadata(absolute_log_path(&store, &handles))
            .expect("log metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    #[cfg(windows)]
    fn windows_registry_log_and_instance_artifacts_have_exact_private_acls() {
        fn assert_private(path: &Path, is_directory: bool) {
            let handle = windows_private::open_path_for_security(path, is_directory)
                .unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
            windows_private::validate_private_handle(&handle, is_directory)
                .unwrap_or_else(|error| panic!("private ACL {}: {error}", path.display()));
        }

        let tmp = TempDir::new().expect("temp dir");
        // This root is created with ordinary inherited permissions first.  The
        // store must tighten a caller-supplied GILA_HOME instead of trusting
        // the user-profile ACL it inherited.
        let root = tmp.path().join("custom-gila-home");
        fs::create_dir(&root).expect("permissive custom root");
        let store = RegistryStore::for_test(root.clone()).expect("repair custom root ACL");
        let mut transaction = store.begin_transaction().expect("registry transaction");
        transaction.allocate_handle().expect("dirty registry");
        transaction.commit().expect("atomic registry commit");
        let handles = store.create_start_log().expect("durable log");

        for directory in [
            root.clone(),
            root.join("jupyter"),
            root.join("jupyter/logs"),
        ] {
            assert_private(&directory, true);
        }
        for file in [
            root.join("servers.lock"),
            root.join("servers.json"),
            absolute_log_path(&store, &handles),
        ] {
            assert_private(&file, false);
        }

        let instance_id = [0xabu8; 16];
        let token = "windows-private-token";
        let base_path = instance_id_to_base_path(&instance_id);
        let artifacts =
            create_instance_artifacts(&store, &instance_id, token, &base_path, Some("hash"))
                .expect("instance artifacts");
        for directory in [&artifacts.instance_dir, &artifacts.runtime_dir] {
            assert_private(directory, true);
        }
        for file in [
            &artifacts.token_file,
            &artifacts.config_file,
            &artifacts.instance_dir.join(INSTANCE_OWNER_MARKER_NAME),
        ] {
            assert_private(file, false);
        }

        let runtime_file = artifacts.runtime_dir.join("jpserver-123.json");
        fs::write(
            &runtime_file,
            serde_json::json!({
                "base_url": base_path.clone(),
                "pid": 123,
                "port": 32123,
                "token": token,
                "url": format!("http://127.0.0.1:32123{base_path}"),
            })
            .to_string(),
        )
        .expect("runtime metadata");
        assert!(matches!(
            discover_runtime_endpoint(
                &artifacts.runtime_dir,
                token,
                &base_path,
                "127.0.0.1".parse().unwrap(),
            )
            .expect("runtime discovery"),
            RuntimeDiscovery::Ready(_, 123)
        ));
        assert_private(&runtime_file, false);
        artifacts.cleanup_all().expect("artifact cleanup");
    }

    #[test]
    #[cfg(windows)]
    fn windows_file_identity_rejects_replacement_and_reparse_points() {
        use std::os::windows::fs::symlink_file;

        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let logs = store.root.join("jupyter/logs");
        ensure_private_directory(&store.root.join("jupyter"), "jupyter").unwrap();
        ensure_private_directory(&logs, "logs").unwrap();

        let path = logs.join("identity.log");
        let original = create_secure_log(&path).expect("original log");
        let original_metadata = original.metadata().expect("original metadata");
        let moved = logs.join("identity-moved.log");
        fs::rename(&path, &moved).expect("rename live log");
        let replacement = create_secure_log(&path).expect("replacement log");
        assert!(open_existing_log(
            &path,
            &original_metadata,
            &original,
            ExistingLogAccess::Read,
        )
        .is_err());
        assert!(validate_windows_file_identity(&original, &replacement).is_err());

        let link = logs.join("runtime-link.json");
        match symlink_file(&moved, &link) {
            Ok(()) => assert!(windows_private::open_path_for_security(&link, false).is_err()),
            Err(error) if error.raw_os_error() == Some(1314) => {
                // Creating symlinks requires Developer Mode or SeCreateSymbolicLinkPrivilege.
            }
            Err(error) => panic!("unexpected symlink creation error: {error}"),
        }
    }

    #[test]
    fn scanner_reads_appends_from_independent_file_descriptions() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");

        let stderr_offset = handles
            .stderr_append
            .stream_position()
            .expect("stderr position");
        let reader_offset = handles
            .read_handle
            .stream_position()
            .expect("reader position");

        handles
            .stdout_append
            .write_all(b"stdout\n")
            .expect("stdout write");
        assert_eq!(
            handles
                .stderr_append
                .stream_position()
                .expect("stderr position after stdout write"),
            stderr_offset,
            "stdout writes must not move stderr's independent file offset"
        );
        assert_eq!(
            handles
                .read_handle
                .stream_position()
                .expect("reader position after stdout write"),
            reader_offset,
            "stdout writes must not move the scanner's independent file offset"
        );
        handles
            .stderr_append
            .write_all(b"stderr\n")
            .expect("stderr write");
        assert_eq!(
            handles
                .read_handle
                .stream_position()
                .expect("reader position after stderr write"),
            reader_offset,
            "stderr writes must not move the scanner's independent file offset"
        );

        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert_eq!(scanner.poll_lines().expect("scan"), ["stdout", "stderr"]);

        handles
            .stdout_append
            .write_all(b"later\n")
            .expect("later append");
        assert_eq!(scanner.poll_lines().expect("later scan"), ["later"]);
    }

    #[test]
    fn scanner_preserves_partial_lines_across_polls() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");

        handles
            .stdout_append
            .write_all(b"first\npar")
            .expect("first append");
        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert_eq!(scanner.poll_lines().expect("first poll"), ["first"]);
        assert!(scanner.poll_lines().expect("unchanged poll").is_empty());

        handles
            .stderr_append
            .write_all(b"tial\r\nlast")
            .expect("second append");
        assert_eq!(scanner.poll_lines().expect("second poll"), ["partial"]);

        handles
            .stdout_append
            .write_all(b"\n")
            .expect("final append");
        assert_eq!(scanner.poll_lines().expect("final poll"), ["last"]);
    }

    #[test]
    fn scanner_many_short_lines_is_bounded_per_poll_without_losing_progress() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let line_count = LOG_SCAN_POLL_LINE_CAP * 5 + 17;
        for _ in 0..line_count {
            handles.stdout_append.write_all(b"x\n").expect("line");
        }

        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        let mut observed = 0usize;
        let mut polls = 0usize;
        while observed < line_count {
            let lines = scanner.poll_lines().expect("bounded scan");
            assert!(!lines.is_empty(), "scanner stopped making progress");
            assert!(lines.len() <= LOG_SCAN_POLL_LINE_CAP);
            assert!(lines.iter().all(|line| line == "x"));
            observed += lines.len();
            polls += 1;
        }
        assert_eq!(observed, line_count);
        assert!(polls > 1, "line cap was not exercised");
        assert!(scanner.poll_lines().expect("drained scan").is_empty());
        assert!(scanner.diagnostic_tail().len() <= LOG_DIAGNOSTIC_RETAIN_CAP);
    }

    #[test]
    fn scanner_unterminated_bytes_are_bounded_per_poll_without_losing_progress() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let bytes = vec![b'z'; LOG_SCAN_POLL_BYTE_CAP * 2 + 17];
        handles
            .stdout_append
            .write_all(&bytes)
            .expect("noisy append");

        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert!(scanner.poll_lines().expect("first bounded poll").is_empty());
        assert_eq!(scanner.offset, LOG_SCAN_POLL_BYTE_CAP as u64);
        assert!(scanner
            .poll_lines()
            .expect("second bounded poll")
            .is_empty());
        assert_eq!(scanner.offset, (LOG_SCAN_POLL_BYTE_CAP * 2) as u64);
        assert!(scanner.poll_lines().expect("final bounded poll").is_empty());
        assert_eq!(scanner.offset, bytes.len() as u64);
        assert!(scanner.poll_lines().expect("drained poll").is_empty());
        assert_eq!(scanner.partial_line.len(), LOG_PARTIAL_LINE_CAP);
        assert!(scanner.partial_line.iter().all(|byte| *byte == b'z'));
    }

    #[test]
    fn scanner_tail_is_bounded_and_truncation_resets_state() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let absolute_path = absolute_log_path(&store, &handles);
        let oversized = vec![b'x'; LOG_DIAGNOSTIC_RETAIN_CAP + 257];

        handles
            .stdout_append
            .write_all(&oversized)
            .expect("oversized append");
        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert!(scanner.poll_lines().expect("oversized poll").is_empty());
        assert_eq!(scanner.diagnostic_tail().len(), LOG_DIAGNOSTIC_RETAIN_CAP);
        assert_eq!(
            scanner.diagnostic_tail(),
            &oversized[oversized.len() - LOG_DIAGNOSTIC_RETAIN_CAP..]
        );

        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&absolute_path)
            .expect("open for truncate")
            .write_all(b"reset\n")
            .expect("write replacement");
        assert_eq!(scanner.poll_lines().expect("post-truncate poll"), ["reset"]);
        assert_eq!(scanner.diagnostic_tail(), b"reset\n");
    }

    #[test]
    fn scanner_partial_line_is_bounded_across_multiple_polls() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let first = vec![b'a'; LOG_PARTIAL_LINE_CAP - 7];
        let second = vec![b'b'; LOG_PARTIAL_LINE_CAP + 31];

        handles
            .stdout_append
            .write_all(&first)
            .expect("first unterminated append");
        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        assert!(scanner.poll_lines().expect("first poll").is_empty());
        assert_eq!(scanner.partial_line.len(), first.len());
        assert!(!scanner.partial_line_truncated);

        handles
            .stderr_append
            .write_all(&second)
            .expect("second unterminated append");
        assert!(scanner.poll_lines().expect("second poll").is_empty());
        assert_eq!(scanner.partial_line.len(), LOG_PARTIAL_LINE_CAP);
        assert!(scanner.partial_line_truncated);
        assert!(scanner.partial_line.iter().all(|byte| *byte == b'b'));
        assert_eq!(scanner.diagnostic_tail().len(), first.len() + second.len());

        handles
            .stdout_append
            .write_all(b"suffix")
            .expect("third unterminated append");
        assert!(scanner.poll_lines().expect("third poll").is_empty());
        assert_eq!(scanner.partial_line.len(), LOG_PARTIAL_LINE_CAP);
        assert!(scanner.partial_line.ends_with(b"suffix"));

        handles
            .stderr_append
            .write_all(b"\n")
            .expect("terminate oversized line");
        let lines = scanner.poll_lines().expect("terminating poll");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(TRUNCATED_LOG_LINE_PREFIX));
        assert!(lines[0].ends_with("suffix"));
        assert!(lines[0].len() <= TRUNCATED_LOG_LINE_PREFIX.len() + LOG_PARTIAL_LINE_CAP);
        assert!(scanner.partial_line.is_empty());
        assert!(!scanner.partial_line_truncated);
    }

    #[test]
    fn diagnostic_tail_redacts_fixed_raw_form_standard_and_hex_case_variants() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let secrets = ["tok en+/Az", "pass word+/By"];
        // This corpus is intentionally fixed rather than generated through the
        // helper under test. It covers raw, form '+', standard '%20', mixed
        // percent-hex case, and fully encoded representations.
        let variants = [
            "tok en+/Az",
            "tok+en%2B%2FAz",
            "tok%20en%2b%2fAz",
            "%74%6F%6b%20%65%6E%2B%2f%41%7a",
            "pass word+/By",
            "pass+word%2B%2FBy",
            "pass%20word%2b%2FBy",
            "%70%61%73%73%20%77%6f%72%64%2B%2f%42%79",
        ];
        let redactor = DiagnosticSecretRedactor::new(secrets);
        writeln!(
            handles.stdout_append,
            "safe-prefix {} safe-suffix",
            variants.join("\n")
        )
        .expect("write diagnostic variants");
        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        scanner.poll_lines().expect("scan diagnostics");
        let diagnostic = redacted_log_tail(&scanner, &redactor);
        assert!(diagnostic.contains("safe-prefix"));
        assert!(diagnostic.contains("safe-suffix"));
        assert!(diagnostic.contains("<redacted>"));
        for variant in variants {
            assert!(
                !diagnostic.contains(variant),
                "diagnostic leaked secret representation {variant:?}: {diagnostic:?}"
            );
        }
    }

    #[test]
    fn diagnostic_tail_redacts_secret_before_final_visible_tail_truncation() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let secret = "UNIQUE-BOUNDARY-LEFT+/%-UNIQUE-BOUNDARY-RIGHT";
        let split = secret.len() / 2;
        handles
            .stdout_append
            .write_all(secret.as_bytes())
            .expect("boundary secret");
        let suffix = vec![b'x'; LOG_DIAGNOSTIC_TAIL_CAP - (secret.len() - split)];
        handles.stdout_append.write_all(&suffix).expect("suffix");

        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        while scanner.offset < scanner.file.metadata().expect("log metadata").len() {
            scanner.poll_lines().expect("scan diagnostic");
        }
        let diagnostic = redacted_log_tail(&scanner, &DiagnosticSecretRedactor::new([secret]));
        assert!(diagnostic.len() <= LOG_DIAGNOSTIC_TAIL_CAP);
        assert!(diagnostic.contains("<redacted>"));
        assert!(!diagnostic.contains(&secret[..split]));
        assert!(!diagnostic.contains(&secret[split..]));
    }

    #[test]
    fn diagnostic_overlap_covers_fully_percent_encoded_boundary_secret() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let secret = "A+/%".repeat(32);
        let encoded = fully_percent_encoded_secret(&secret);
        assert_eq!(encoded.len(), secret.len() * 3);
        let split = encoded.len() / 2;
        handles
            .stdout_append
            .write_all(encoded.as_bytes())
            .expect("encoded secret");
        handles
            .stdout_append
            .write_all(&vec![
                b'y';
                LOG_DIAGNOSTIC_TAIL_CAP - (encoded.len() - split)
            ])
            .expect("suffix");

        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        scanner.poll_lines().expect("scan diagnostic");
        let diagnostic =
            redacted_log_tail(&scanner, &DiagnosticSecretRedactor::new([secret.as_str()]));
        assert!(diagnostic.contains("<redacted>"));
        assert!(!diagnostic.contains(&encoded[..split]));
        assert!(!diagnostic.contains(&encoded[split..]));
        assert!(diagnostic.len() <= LOG_DIAGNOSTIC_TAIL_CAP);
    }

    #[test]
    fn dense_percent_secret_redaction_is_single_pass_and_bounded() {
        let dense = "%".repeat(LOG_DIAGNOSTIC_RETAIN_CAP);
        let redacted = DiagnosticSecretRedactor::new(["%"]).redact(&dense);
        assert_eq!(redacted, "<redacted>");
    }

    #[test]
    fn redaction_shrinkage_never_pulls_overlap_bytes_into_visible_tail() {
        let tmp = TempDir::new().expect("temp dir");
        let store = test_store(&tmp);
        let mut handles = store.create_start_log().expect("start log");
        let overlap_secret = "B".repeat(4_096);
        let visible_secret = "A".repeat(4_096);
        handles
            .stdout_append
            .write_all(overlap_secret.as_bytes())
            .expect("overlap secret");
        for _ in 0..4 {
            handles
                .stdout_append
                .write_all(visible_secret.as_bytes())
                .expect("visible secret");
        }
        handles
            .stdout_append
            .write_all(&vec![b'x'; 2_048])
            .expect("visible suffix");

        let mut scanner = IncrementalLogScanner::new(handles.read_handle);
        while scanner.offset < scanner.file.metadata().expect("log metadata").len() {
            scanner.poll_lines().expect("scan diagnostic");
        }
        let redactor =
            DiagnosticSecretRedactor::new([overlap_secret.as_str(), visible_secret.as_str()]);
        let diagnostic = redacted_log_tail(&scanner, &redactor);
        assert!(!diagnostic.contains('A'));
        assert!(!diagnostic.contains('B'));
        assert!(diagnostic.ends_with(&"x".repeat(2_048)));
        assert!(diagnostic.len() <= LOG_DIAGNOSTIC_TAIL_CAP);
    }
}

#[cfg(test)]
mod startup_guard_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use tempfile::TempDir;

    const FIXTURE_ROLE_ENV: &str = "GILA_B2A_FIXTURE_ROLE";
    const FIXTURE_MARKER_ENV: &str = "GILA_B2A_FIXTURE_MARKER";
    const FIXTURE_PLATFORM_ENV: &str = "GILA_B2A_FIXTURE_PLATFORM";

    #[derive(Debug, Clone, Copy)]
    enum InjectedTerminationFailure {
        Kind(io::ErrorKind),
        #[cfg(unix)]
        RawOs(i32),
    }

    #[derive(Debug)]
    struct FaultInjectingChild {
        termination_results: VecDeque<Option<InjectedTerminationFailure>>,
        wait_results: VecDeque<Option<io::ErrorKind>>,
        termination_calls: Arc<AtomicUsize>,
        wait_calls: Arc<AtomicUsize>,
    }

    impl process_wrap::std::ChildWrapper for FaultInjectingChild {
        fn inner(&self) -> &dyn process_wrap::std::ChildWrapper {
            self
        }

        fn inner_mut(&mut self) -> &mut dyn process_wrap::std::ChildWrapper {
            self
        }

        fn into_inner(self: Box<Self>) -> Box<dyn process_wrap::std::ChildWrapper> {
            self
        }

        fn start_kill(&mut self) -> io::Result<()> {
            self.termination_calls.fetch_add(1, Ordering::SeqCst);
            match self.termination_results.pop_front().flatten() {
                Some(InjectedTerminationFailure::Kind(kind)) => {
                    Err(io::Error::new(kind, "injected termination failure"))
                }
                #[cfg(unix)]
                Some(InjectedTerminationFailure::RawOs(code)) => {
                    Err(io::Error::from_raw_os_error(code))
                }
                None => Ok(()),
            }
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            self.wait_calls.fetch_add(1, Ordering::SeqCst);
            match self.wait_results.pop_front().flatten() {
                Some(kind) => Err(io::Error::new(kind, "injected wait failure")),
                None => Ok(successful_exit_status()),
            }
        }
    }

    #[cfg(unix)]
    fn successful_exit_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    fn successful_exit_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    fn fault_injecting_guard(
        termination_results: impl IntoIterator<Item = Option<InjectedTerminationFailure>>,
        wait_results: impl IntoIterator<Item = Option<io::ErrorKind>>,
    ) -> (StartupGuard, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let termination_calls = Arc::new(AtomicUsize::new(0));
        let wait_calls = Arc::new(AtomicUsize::new(0));
        let child = FaultInjectingChild {
            termination_results: termination_results.into_iter().collect(),
            wait_results: wait_results.into_iter().collect(),
            termination_calls: Arc::clone(&termination_calls),
            wait_calls: Arc::clone(&wait_calls),
        };
        (
            StartupGuard::armed(Box::new(child)),
            termination_calls,
            wait_calls,
        )
    }

    fn base_fixture_command(test_filter: &str, role: &str, marker: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg(test_filter)
            .arg("--ignored")
            .arg("--test-threads=1")
            .env(FIXTURE_ROLE_ENV, role)
            .env(FIXTURE_MARKER_ENV, marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[cfg(unix)]
    fn fixture_command(test_filter: &str, role: &str, marker: &Path) -> Command {
        let mut command = base_fixture_command(test_filter, role, marker);
        command.env(FIXTURE_PLATFORM_ENV, "unix");
        command
    }

    #[cfg(windows)]
    fn fixture_command(test_filter: &str, role: &str, marker: &Path) -> Command {
        let mut command = base_fixture_command(test_filter, role, marker);
        command.env(FIXTURE_PLATFORM_ENV, "windows");
        command
    }

    fn assert_expected_fixture_platform() {
        #[cfg(unix)]
        assert_eq!(std::env::var(FIXTURE_PLATFORM_ENV).as_deref(), Ok("unix"));
        #[cfg(windows)]
        assert_eq!(
            std::env::var(FIXTURE_PLATFORM_ENV).as_deref(),
            Ok("windows")
        );
    }

    fn wait_for_marker(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.is_file() {
            assert!(Instant::now() < deadline, "fixture did not become ready");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_listener(path: &Path) -> SocketAddr {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(contents) = fs::read_to_string(path) {
                if let Ok(address) = contents.parse() {
                    return address;
                }
            }
            assert!(
                Instant::now() < deadline,
                "descendant listener did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "spawned as a process-management fixture"]
    fn long_lived_fixture() {
        if std::env::var(FIXTURE_ROLE_ENV).as_deref() != Ok("long-lived") {
            return;
        }
        assert_expected_fixture_platform();
        let marker = PathBuf::from(std::env::var_os(FIXTURE_MARKER_ENV).expect("marker path"));
        fs::write(marker, b"ready").expect("write ready marker");
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    #[ignore = "spawned as a process-management fixture"]
    fn descendant_parent_fixture() {
        if std::env::var(FIXTURE_ROLE_ENV).as_deref() != Ok("descendant-parent") {
            return;
        }
        assert_expected_fixture_platform();
        let marker = PathBuf::from(std::env::var_os(FIXTURE_MARKER_ENV).expect("marker path"));
        let mut descendant = fixture_command(
            "startup_guard_tests::descendant_listener_fixture",
            "descendant-listener",
            &marker,
        )
        .spawn()
        .expect("spawn descendant listener fixture");
        let _ = descendant.wait();
    }

    #[test]
    #[ignore = "spawned as a process-management fixture"]
    fn descendant_listener_fixture() {
        if std::env::var(FIXTURE_ROLE_ENV).as_deref() != Ok("descendant-listener") {
            return;
        }
        assert_expected_fixture_platform();
        let marker = PathBuf::from(std::env::var_os(FIXTURE_MARKER_ENV).expect("marker path"));
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind descendant listener");
        fs::write(
            marker,
            listener.local_addr().expect("listener address").to_string(),
        )
        .expect("write listener marker");
        for connection in listener.incoming() {
            drop(connection.expect("accept connection"));
        }
    }

    #[test]
    fn disarm_leaves_a_live_child_owned_by_the_caller() {
        let tmp = TempDir::new().expect("temp dir");
        let marker = tmp.path().join("ready");
        let command = fixture_command(
            "startup_guard_tests::long_lived_fixture",
            "long-lived",
            &marker,
        );
        let guard = StartupGuard::spawn(command).expect("spawn guarded child");
        wait_for_marker(&marker);

        let mut child = guard.disarm();
        assert!(
            child.try_wait().expect("check live child").is_none(),
            "disarming must not terminate the live child"
        );

        child.start_kill().expect("terminate disarmed child tree");
        let status = child.wait().expect("reap disarmed child");
        assert!(
            !status.success(),
            "fixture should exit by forced termination"
        );
        assert!(child.try_wait().expect("check reaped child").is_some());
    }

    #[test]
    fn rollback_retains_handle_and_drop_retries_after_termination_failure() {
        let (guard, termination_calls, wait_calls) = fault_injecting_guard(
            [
                Some(InjectedTerminationFailure::Kind(
                    io::ErrorKind::PermissionDenied,
                )),
                None,
            ],
            [None],
        );

        let error = guard.rollback().expect_err("termination must fail");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(termination_calls.load(Ordering::SeqCst), 2);
        assert_eq!(wait_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rollback_retries_transient_termination_then_reaps() {
        let (guard, termination_calls, wait_calls) = fault_injecting_guard(
            [
                Some(InjectedTerminationFailure::Kind(io::ErrorKind::WouldBlock)),
                None,
            ],
            [None],
        );

        guard.rollback().expect("transient retry must recover");

        assert_eq!(termination_calls.load(Ordering::SeqCst), 2);
        assert_eq!(wait_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rollback_surfaces_persistent_termination_failure_after_final_drop_retry() {
        let (guard, termination_calls, wait_calls) = fault_injecting_guard(
            [
                Some(InjectedTerminationFailure::Kind(
                    io::ErrorKind::PermissionDenied,
                )),
                Some(InjectedTerminationFailure::Kind(
                    io::ErrorKind::PermissionDenied,
                )),
            ],
            std::iter::empty::<Option<io::ErrorKind>>(),
        );

        let error = guard.rollback().expect_err("persistent error must surface");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(termination_calls.load(Ordering::SeqCst), 2);
        assert_eq!(wait_calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rollback_reaps_after_unix_esrch_reports_an_already_exited_tree() {
        let (guard, termination_calls, wait_calls) = fault_injecting_guard(
            [
                Some(InjectedTerminationFailure::RawOs(ESRCH_RAW_OS_ERROR)),
                None,
            ],
            [Some(io::ErrorKind::BrokenPipe), None],
        );

        let error = guard
            .rollback()
            .expect_err("the injected reap failure must be surfaced");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(termination_calls.load(Ordering::SeqCst), 1);
        assert_eq!(wait_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rollback_surfaces_wait_failure_after_successful_termination() {
        let (guard, termination_calls, wait_calls) =
            fault_injecting_guard([None, None], [Some(io::ErrorKind::BrokenPipe), None]);

        let error = guard.rollback().expect_err("wait must fail");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(termination_calls.load(Ordering::SeqCst), 1);
        assert_eq!(wait_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn dropping_guard_kills_and_reaps_the_descendant_process_tree() {
        let tmp = TempDir::new().expect("temp dir");
        let marker = tmp.path().join("listener-address");
        let command = fixture_command(
            "startup_guard_tests::descendant_parent_fixture",
            "descendant-parent",
            &marker,
        );
        let guard = StartupGuard::spawn(command).expect("spawn guarded process tree");
        let listener_address = wait_for_listener(&marker);
        TcpStream::connect_timeout(&listener_address, Duration::from_secs(1))
            .expect("descendant listener must be live before guard cleanup");

        drop(guard);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match TcpStream::connect_timeout(&listener_address, Duration::from_millis(100)) {
                Err(_) => break,
                Ok(connection) => drop(connection),
            }
            assert!(
                Instant::now() < deadline,
                "descendant listener survived process-tree cleanup"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

// ---- notebook execution -------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterExecuteParams {
    /// Path to the notebook file (.ipynb)
    pub notebook_path: String,
    /// Optional working directory to execute the notebook in.
    /// If not provided, uses the notebook's parent directory.
    pub working_dir: Option<String>,
    /// Per-cell execution timeout in seconds, passed to nbconvert's
    /// `ExecutePreprocessor.timeout`. A cell that exceeds this is interrupted
    /// and marks the notebook as failed. Default: 300.
    pub timeout_seconds: Option<u64>,
    /// Whether to save the executed notebook with outputs (default: true)
    pub save_outputs: Option<bool>,
    /// Kernel name to use (default: python3)
    pub kernel_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterExecuteResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Path to the executed notebook
    pub notebook_path: String,
    /// Number of code cells executed (markdown/raw cells are not counted)
    pub cells_executed: usize,
    /// Number of code cells whose execution produced an error
    pub cells_failed: usize,
    /// Execution time in seconds
    pub execution_time_seconds: f64,
    /// Error message if any
    pub error: Option<String>,
    /// Cell outputs summary
    pub cell_outputs: Vec<CellOutputSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellOutputSummary {
    pub cell_index: usize,
    pub cell_type: String,
    pub success: bool,
    pub output_count: usize,
    pub error: Option<String>,
}

/// Resolve a notebook path to an absolute canonical path.
/// Absolute paths are returned as-is.
/// Relative paths are resolved against the working directory.
fn resolve_notebook_path(notebook: &str, working_dir: Option<&str>) -> Result<PathBuf> {
    let notebook_input = PathBuf::from(notebook);
    let resolved = if notebook_input.is_absolute() {
        notebook_input
    } else {
        let work = working_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        work.join(&notebook_input)
    };

    resolved.canonicalize().with_context(|| {
        format!(
            "Failed to resolve notebook path: {} (working_dir: {:?})",
            notebook, working_dir
        )
    })
}

/// Execute a Jupyter notebook using nbconvert.
pub fn execute_notebook(params: JupyterExecuteParams) -> Result<JupyterExecuteResult> {
    let start_time = std::time::Instant::now();

    // Resolve notebook path using production resolver
    let notebook_path =
        resolve_notebook_path(&params.notebook_path, params.working_dir.as_deref())?;

    // Determine working_dir for execution: notebook's parent if not explicitly specified.
    let working_dir = match params.working_dir.as_ref().map(PathBuf::from) {
        Some(d) => d,
        None => notebook_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    if !notebook_path.exists() {
        anyhow::bail!("Notebook not found: {}", notebook_path.display());
    }

    let timeout = params.timeout_seconds.unwrap_or(300);
    let save_outputs = params.save_outputs.unwrap_or(true);
    let kernel_name = params.kernel_name.unwrap_or_else(|| "python3".to_string());

    // Build the nbconvert command (env-scrubbed through the shared helper)
    let mut cmd = jupyter_cmd();
    cmd.arg("nbconvert")
        .arg("--execute")
        .arg("--to")
        .arg("notebook")
        .arg("--inplace")
        .arg("--ExecutePreprocessor.kernel_name")
        .arg(&kernel_name)
        .arg("--ExecutePreprocessor.timeout")
        .arg(timeout.to_string())
        .arg(&notebook_path) // Pass RESOLVED path to nbconvert
        .current_dir(&working_dir);

    if !save_outputs {
        cmd.arg("--no-output");
    }

    let output = cmd
        .output()
        .context("Failed to execute jupyter nbconvert. Is jupyter installed?")?;

    let execution_time = start_time.elapsed().as_secs_f64();

    let success = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse outputs from the executed notebook. nbconvert `--inplace` writes
    // partial outputs (up to and including the failing cell) even on error, so
    // parse best-effort to surface which cell failed. On a non-success run
    // where the file is unreadable, fall back to an empty list.
    let cell_outputs = match parse_notebook_outputs(&notebook_path) {
        Ok(o) => o,
        Err(_) if !success => vec![],
        Err(e) => return Err(e),
    };

    // Count only executed code cells; markdown/raw cells are not "executed".
    let cells_executed = cell_outputs
        .iter()
        .filter(|c| c.cell_type == "code")
        .count();
    let cells_failed = cell_outputs
        .iter()
        .filter(|c| c.cell_type == "code" && !c.success)
        .count();

    Ok(JupyterExecuteResult {
        success,
        notebook_path: params.notebook_path,
        cells_executed,
        cells_failed,
        execution_time_seconds: execution_time,
        error: if success {
            None
        } else {
            Some(format!("stdout: {stdout}\nstderr: {stderr}"))
        },
        cell_outputs,
    })
}

/// Parse cell outputs from an executed notebook.
fn parse_notebook_outputs(notebook_path: &Path) -> Result<Vec<CellOutputSummary>> {
    use nbformat::{parse_notebook, v4, Notebook};
    use std::fs;

    let content = fs::read_to_string(notebook_path).context("Failed to read notebook")?;
    let nb = parse_notebook(&content).context("Failed to parse notebook")?;

    let cells = match nb {
        Notebook::V4(nb) => nb.cells,
        Notebook::Legacy(nb) => {
            // Upgrade legacy notebook to v4
            let upgraded = nbformat::upgrade_legacy_notebook(nb)?;
            upgraded.cells
        }
    };

    let mut summaries = Vec::new();

    for (idx, cell) in cells.iter().enumerate() {
        let cell_type = match cell {
            v4::Cell::Code { .. } => "code",
            v4::Cell::Markdown { .. } => "markdown",
            v4::Cell::Raw { .. } => "raw",
        };

        let mut success = true;
        let mut output_count = 0;
        let mut error = None;

        if let v4::Cell::Code { outputs, .. } = cell {
            output_count = outputs.len();
            for output in outputs {
                if let v4::Output::Error(v4::ErrorOutput { ename, evalue, .. }) = output {
                    success = false;
                    error = Some(format!("{ename}: {evalue}"));
                    break;
                }
            }
        }

        summaries.push(CellOutputSummary {
            cell_index: idx,
            cell_type: cell_type.to_string(),
            success,
            output_count,
            error,
        });
    }

    Ok(summaries)
}

// ---- server management --------------------------------------------------

/// Parameters for starting a Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerParams {
    /// Working directory for the server (default: current directory)
    pub working_dir: Option<String>,
    /// Port to run the server on (default: 8888)
    pub port: Option<u16>,
    /// Bind address. Defaults to `127.0.0.1`. MUST be a typed loopback IP —
    /// non-loopback hosts are rejected so the server is reachable only from the
    /// operator's own machine.
    pub host: Option<String>,
    /// Token for authentication. Headless starts require an explicit token,
    /// password, or password hash. Browser starts may generate a private token.
    pub token: Option<String>,
    /// Already-hashed password for authentication, in the form Jupyter's
    /// `--NotebookApp.password` expects (`argon2:$argon2id$…` PHC string).
    /// Passed through verbatim. Takes precedence over `password`.
    pub password_hash: Option<String>,
    /// Plaintext password for authentication. The tool hashes this with
    /// argon2 (matching Jupyter's `argon2:` scheme) and passes the resulting
    /// hash to `--NotebookApp.password`. Used only when `password_hash` is
    /// absent. Storing/transporting a plaintext password is discouraged;
    /// prefer `password_hash` for persistent configs.
    pub password: Option<String>,
    /// Whether to open a browser on startup (default: false). `Some(true)`
    /// omits `--no-browser` so Jupyter opens the operator's browser; anything
    /// else passes `--no-browser`.
    pub open_browser: Option<bool>,
    /// Additional Jupyter arguments. The production surface is an exact
    /// allowlist containing only `--ServerApp.default_url` (split or `=`
    /// spelling); Gila consumes it instead of forwarding it. Every positional,
    /// alias, abbreviation, subcommand, and other option is rejected before
    /// spawn so Traitlets cannot reinterpret it around Gila's controls.
    pub extra_args: Option<Vec<String>>,
}

/// Result of starting a Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerResult {
    /// Whether server started successfully
    pub success: bool,
    /// Opaque handle for later `stop_server` / `get_server_status` calls.
    pub handle_id: Option<u64>,
    /// Server URL (e.g. http://127.0.0.1:8888)
    pub url: Option<String>,
    /// Server process ID (informational; operations use `handle_id`)
    pub pid: Option<u32>,
    /// Port the server is running on
    pub port: Option<u16>,
    /// Compatibility field for older in-process callers. Authentication
    /// material is intentionally never serialized or returned by the current
    /// lifecycle; it lives only in the private registry.
    #[serde(skip)]
    pub token: Option<String>,
    /// Private durable log path, relative to `GILA_HOME`.
    pub log_path: Option<String>,
    /// Error message if any
    pub error: Option<String>,
}

/// Network-observed state of a durable registry entry.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JupyterServerState {
    Running,
    Unreachable,
    NotFound,
}

/// Status of a Jupyter server, queried by handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerStatus {
    /// Compatibility convenience: true only for a verified 2xx kernels API
    /// response containing valid kernels JSON.
    pub running: bool,
    pub state: JupyterServerState,
    /// The handle this status refers to
    pub handle_id: u64,
    /// Registered server URL, including an optional Jupyter base path.
    pub url: Option<String>,
    /// Registered port.
    pub port: Option<u16>,
    /// Private durable log path, relative to `GILA_HOME`.
    pub log_path: Option<String>,
    /// List of running kernels
    pub kernels: Vec<KernelInfo>,
    /// Bounded probe/validation diagnostic when the record is unreachable.
    pub error: Option<String>,
}

/// Information about a running kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelInfo {
    pub id: String,
    pub name: String,
    pub last_activity: String,
    pub execution_state: String,
    pub connections: usize,
}

/// Parsed endpoint from Jupyter server startup output
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JupyterEndpoint {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub token: String,
}

/// Summary of an active Jupyter server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSummary {
    pub handle_id: u64,
    pub url: String,
    pub port: u16,
    pub running: bool,
    pub state: JupyterServerState,
    pub log_path: Option<String>,
    pub error: Option<String>,
}

/// Result of listing Jupyter servers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterListResult {
    pub servers: Vec<ServerSummary>,
}

fn is_loopback(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn typed_loopback_host(parsed: &url::Url) -> Result<IpAddr> {
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => Ok(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => Ok(IpAddr::V6(ip)),
        Some(url::Host::Ipv4(ip)) => anyhow::bail!("URL host {ip} is not loopback"),
        Some(url::Host::Ipv6(ip)) => anyhow::bail!("URL host {ip} is not loopback"),
        Some(url::Host::Domain(host)) => {
            anyhow::bail!("URL host must be a typed loopback IP address, got {host}")
        }
        None => anyhow::bail!("URL is missing a host"),
    }
}

fn explicit_port(input: &str) -> Option<u16> {
    let authority_start = input.find("://")?.checked_add(3)?;
    let authority_end = input[authority_start..]
        .find(['/', '?', '#'])
        .map_or(input.len(), |offset| authority_start + offset);
    let authority = &input[authority_start..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }

    let encoded_port = if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']')?;
        rest.get(close + 1..)?.strip_prefix(':')?
    } else {
        authority.rsplit_once(':')?.1
    };
    if encoded_port.is_empty() || !encoded_port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    encoded_port.parse().ok()
}

const CONTROLLED_DEFAULT_URL: &str = "/tree";
const CANONICAL_DEFAULT_URL_FLAG: &str = "--ServerApp.default_url";

fn normalize_expected_default_url(value: &str) -> Result<String> {
    let value = value.trim();
    if !value.starts_with('/') || value.starts_with("//") {
        anyhow::bail!("Jupyter default_url must be an absolute URL path beginning with one '/'");
    }
    if value.contains(['?', '#'])
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        anyhow::bail!("Jupyter default_url must not contain a query, fragment, or whitespace");
    }
    let normalized = value.trim_end_matches('/');
    Ok(if normalized.is_empty() {
        "/".to_string()
    } else {
        normalized.to_string()
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedExtraArgs {
    default_url: Option<String>,
    forwarded: Vec<String>,
}

/// Parse the deliberately tiny Jupyter extension surface. Traitlets accepts
/// abbreviations, positionals/subcommands, and migrates deprecated aliases
/// after parsing; therefore a denylist or "Gila flags come last" is not a
/// security boundary. Only the one documented default URL spelling is public.
fn parse_extra_args(extra_args: &[String]) -> Result<ParsedExtraArgs> {
    let mut default_url = None;
    #[cfg(feature = "jupyter-test-fixture")]
    let mut forwarded = Vec::new();
    #[cfg(not(feature = "jupyter-test-fixture"))]
    let forwarded = Vec::new();
    let mut index = 0;

    while index < extra_args.len() {
        let argument = &extra_args[index];
        let default_value = if argument == CANONICAL_DEFAULT_URL_FLAG {
            index += 1;
            Some(
                extra_args
                    .get(index)
                    .with_context(|| format!("{CANONICAL_DEFAULT_URL_FLAG} requires a value"))?
                    .as_str(),
            )
        } else {
            argument.strip_prefix(&format!("{CANONICAL_DEFAULT_URL_FLAG}="))
        };
        if let Some(value) = default_value {
            if default_url.is_some() {
                anyhow::bail!("extra_args may configure Jupyter default_url at most once");
            }
            default_url = Some(normalize_expected_default_url(value)?);
            index += 1;
            continue;
        }

        #[cfg(feature = "jupyter-test-fixture")]
        {
            const BOOLEAN_FLAGS: &[&str] =
                &["--fixture-spoof-candidates", "--fixture-invalid-only"];
            const VALUE_FLAGS: &[&str] = &["--fixture-descendant-port", "--fixture-break-registry"];

            if BOOLEAN_FLAGS.contains(&argument.as_str()) {
                forwarded.push(argument.clone());
                index += 1;
                continue;
            }
            let mut fixture_value = None;
            let mut fixture_flag = None;
            for flag in VALUE_FLAGS {
                if argument == flag {
                    index += 1;
                    fixture_value = Some(
                        extra_args
                            .get(index)
                            .with_context(|| format!("{flag} requires a value"))?
                            .clone(),
                    );
                    fixture_flag = Some((*flag).to_string());
                    break;
                }
                if let Some(value) = argument.strip_prefix(&format!("{flag}=")) {
                    if value.is_empty() {
                        anyhow::bail!("{flag} requires a non-empty value");
                    }
                    fixture_value = Some(value.to_string());
                    fixture_flag = Some((*flag).to_string());
                    break;
                }
            }
            if let (Some(flag), Some(value)) = (fixture_flag, fixture_value) {
                forwarded.push(flag);
                forwarded.push(value);
                index += 1;
                continue;
            }
        }

        anyhow::bail!(
            "Jupyter argument not in the exact allowlist: {argument}; only \
             {CANONICAL_DEFAULT_URL_FLAG} is supported"
        );
    }

    Ok(ParsedExtraArgs {
        default_url,
        forwarded,
    })
}

#[cfg(test)]
fn normalized_base_path<'a>(path: &'a str, expected_default_url: &str) -> Result<&'a str> {
    let without_trailing_slash = path.trim_end_matches('/');
    if expected_default_url == "/" {
        return Ok(without_trailing_slash);
    }
    // The suffix is either Gila's controlled `/tree` route or the single
    // caller-supplied default_url validated before spawn. Strip exactly that
    // known suffix, preserving a base path that happens to end the same way.
    without_trailing_slash
        .strip_suffix(expected_default_url)
        .context("endpoint URL does not end in the expected Jupyter default route")
}

#[cfg(test)]
fn format_base_url(
    parsed: &url::Url,
    ip: IpAddr,
    port: u16,
    expected_default_url: &str,
) -> Result<String> {
    let host = match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    Ok(format!(
        "{}://{}:{}{}",
        parsed.scheme(),
        host,
        port,
        normalized_base_path(parsed.path(), expected_default_url)?
    ))
}

#[cfg(test)]
fn parse_endpoint_candidate(
    candidate: &str,
    expected_token: &str,
    expected_default_url: &str,
) -> Result<JupyterEndpoint> {
    if expected_token.is_empty() {
        anyhow::bail!("expected Jupyter token must not be empty");
    }

    let parsed = url::Url::parse(candidate).context("failed to parse endpoint URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("endpoint scheme must be http or https");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("endpoint URL must not contain user info");
    }
    let port = explicit_port(candidate).context("endpoint URL must contain an explicit port")?;
    if port == 0 {
        anyhow::bail!("endpoint URL port must not be zero");
    }
    let ip = typed_loopback_host(&parsed)?;
    let tokens: Vec<String> = parsed
        .query_pairs()
        .filter(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
        .collect();
    if tokens.len() != 1 || tokens[0].is_empty() || tokens[0] != expected_token {
        anyhow::bail!("endpoint token does not exactly match the expected token");
    }

    Ok(JupyterEndpoint {
        url: format_base_url(&parsed, ip, port, expected_default_url)?,
        host: ip.to_string(),
        port,
        token: tokens.into_iter().next().expect("exactly one token"),
    })
}

#[cfg(test)]
fn endpoint_candidate_strings(output: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    let mut remaining = output;
    while !remaining.is_empty() {
        let http = remaining.find("http://");
        let https = remaining.find("https://");
        let Some(start) = (match (http, https) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(start), None) | (None, Some(start)) => Some(start),
            (None, None) => None,
        }) else {
            break;
        };
        let candidate = &remaining[start..];
        let end = candidate
            .char_indices()
            .find(|(index, ch)| {
                *index != 0
                    && (ch.is_ascii_whitespace()
                        || ch.is_control()
                        || matches!(ch, '\"' | '\'' | '<' | '>' | ')' | ','))
            })
            .map_or(candidate.len(), |(index, _)| index);
        candidates.push(&candidate[..end]);
        let advance = start.saturating_add(end.max(1));
        remaining = &remaining[advance..];
    }
    candidates
}

/// Scan every URL candidate and keep only endpoints that satisfy Gila's exact
/// typed-loopback, explicit-port, and token boundary. Rejected candidates do
/// not prevent a later legitimate announcement from being accepted.
#[cfg(test)]
fn parse_jupyter_endpoints(
    output: &str,
    expected_token: &str,
    expected_default_url: &str,
) -> Vec<JupyterEndpoint> {
    endpoint_candidate_strings(output)
        .into_iter()
        .filter_map(|candidate| {
            parse_endpoint_candidate(candidate, expected_token, expected_default_url).ok()
        })
        .collect()
}

#[derive(Debug)]
struct ValidatedStoredEndpoint {
    base_url: url::Url,
    socket_addr: SocketAddr,
}

fn validate_stored_record(record: &ServerRecord) -> Result<ValidatedStoredEndpoint> {
    if record.token.is_empty() {
        anyhow::bail!("registered Jupyter token is empty");
    }
    if record.token.len() > AUTH_SECRET_INPUT_CAP {
        anyhow::bail!("registered Jupyter token exceeds the {AUTH_SECRET_INPUT_CAP}-byte limit");
    }
    let parsed = url::Url::parse(&record.url).context("registered Jupyter URL is invalid")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("registered Jupyter URL must use http or https");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("registered Jupyter URL must not contain user info");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("registered Jupyter base URL must not contain a query or fragment");
    }
    let port = explicit_port(&record.url)
        .context("registered Jupyter URL must contain an explicit port")?;
    if port == 0 || record.port == 0 {
        anyhow::bail!("registered Jupyter port must not be zero");
    }
    if port != record.port {
        anyhow::bail!(
            "registered Jupyter URL port {port} does not match record port {}",
            record.port
        );
    }
    let ip = typed_loopback_host(&parsed)?;
    if record.identity_bound {
        let expected_base_path = instance_id_to_base_path(&record.instance_id);
        if parsed.path() != expected_base_path {
            anyhow::bail!(
                "registered Jupyter base path does not exactly match its instance identity"
            );
        }
        let expected_runtime = format!(
            "jupyter/instances/{}/runtime",
            instance_id_to_hex(&record.instance_id)
        );
        if record.runtime_path.as_deref() != Some(expected_runtime.as_str()) {
            anyhow::bail!("registered Jupyter runtime path does not match its instance identity");
        }
    }
    Ok(ValidatedStoredEndpoint {
        base_url: parsed,
        socket_addr: SocketAddr::new(ip, port),
    })
}

fn validate_registered_runtime(
    store: &RegistryStore,
    record: &ServerRecord,
    endpoint: &ValidatedStoredEndpoint,
) -> Result<()> {
    if !record.identity_bound {
        return Ok(());
    }
    let instance_hex = instance_id_to_hex(&record.instance_id);
    let runtime_dir = store
        .root
        .join("jupyter")
        .join("instances")
        .join(&instance_hex)
        .join("runtime");
    let expected_relative = format!("jupyter/instances/{instance_hex}/runtime");
    if record.runtime_path.as_deref() != Some(expected_relative.as_str()) {
        anyhow::bail!("registered runtime path is not bound to the instance identity");
    }
    let expected_ip = endpoint.socket_addr.ip();
    match discover_runtime_endpoint(
        &runtime_dir,
        &record.token,
        &instance_id_to_base_path(&record.instance_id),
        expected_ip,
    )? {
        RuntimeDiscovery::Pending => anyhow::bail!("registered Jupyter runtime file is missing"),
        RuntimeDiscovery::Ready(runtime, _runtime_pid) => {
            if runtime.url != record.url || runtime.port != record.port {
                anyhow::bail!("registered Jupyter runtime endpoint no longer matches its record");
            }
            Ok(())
        }
    }
}

fn api_url(base_url: &url::Url, endpoint: &str) -> url::Url {
    let mut url = base_url.clone();
    let path = format!(
        "{}/{}",
        base_url.path().trim_end_matches('/'),
        endpoint.trim_start_matches('/')
    );
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    url
}

/// Detect environment manager in a directory and return appropriate launcher.
///
/// Searches for pixi.toml, pyproject.toml (uv), requirements.txt (venv),
/// environment.yml (conda), or .venv directory. Returns a command wrapper
/// that will activate the environment before running jupyter.
///
/// The wrapper is constructed so that we can still append `notebook --port XXX` etc.
fn detect_and_wrap_jupyter_cmd(working_dir: &Path) -> (String, Vec<String>) {
    // Check for pixi.toml
    if working_dir.join("pixi.toml").exists() {
        return (
            "pixi".to_string(),
            vec!["run".to_string(), "jupyter".to_string()],
        );
    }

    // Check for uv (pyproject.toml with [tool.uv])
    if let Ok(content) = fs::read_to_string(working_dir.join("pyproject.toml")) {
        if content.contains("[tool.uv]") {
            return (
                "uv".to_string(),
                vec!["run".to_string(), "jupyter".to_string()],
            );
        }
    }

    // Check for conda environment.yml
    if working_dir.join("environment.yml").exists() {
        return (
            "conda".to_string(),
            vec![
                "run".to_string(),
                "--file".to_string(),
                working_dir
                    .join("environment.yml")
                    .to_string_lossy()
                    .to_string(),
                "jupyter".to_string(),
            ],
        );
    }

    // Check for .venv directory with platform-specific executable
    if working_dir.join(".venv").exists() {
        #[cfg(unix)]
        let venv_exe = working_dir.join(".venv/bin/jupyter");
        #[cfg(windows)]
        let venv_exe = working_dir.join(".venv/Scripts/jupyter.exe");

        if venv_exe.exists() {
            return (venv_exe.to_string_lossy().to_string(), vec![]);
        }
    }

    // Check for requirements.txt (assume venv exists or will be created)
    if working_dir.join("requirements.txt").exists() {
        #[cfg(unix)]
        let venv_exe = working_dir.join(".venv/bin/jupyter");
        #[cfg(windows)]
        let venv_exe = working_dir.join(".venv/Scripts/jupyter.exe");

        if venv_exe.exists() {
            return (venv_exe.to_string_lossy().to_string(), vec![]);
        }
    }

    // Fallback: plain jupyter (with minimal environment)
    ("jupyter".to_string(), vec![])
}

/// Build the base `jupyter` command with the inherited environment scrubbed
/// and only a minimal, safe allowlist passed back through.
fn jupyter_cmd() -> Command {
    let mut cmd = Command::new("jupyter");
    // Scrub the whole inherited environment, then pass back ONLY the minimal
    // allowlist jupyter needs to locate its binary, write config, and render.
    // No gila control-plane env reaches the child.
    cmd.env_clear();
    copy_safe_child_environment(&mut cmd);
    cmd
}

/// Hash a plaintext password into the `argon2:$argon2id$…` PHC string that
/// Jupyter's `--NotebookApp.password` expects and `notebook.auth.passwd_check`
/// verifies. Matches Jupyter's `argon2:` prefix scheme (the `argon2-cffi`
/// `PasswordHasher` produces the suffix after the colon); the argon2id
/// parameters are the crate defaults, which verify cleanly because
/// `passwd_check` reads them back from the encoded string.
fn hash_password(plaintext: &str) -> Result<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    let argon = argon2::Argon2::default();
    let hash = argon
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("Failed to hash server password: {e}"))?;
    Ok(format!("argon2:{hash}"))
}

fn instance_id_to_base_path(instance_id: &[u8; 16]) -> String {
    format!("/__gila/{}/", instance_id_to_hex(instance_id))
}

fn instance_id_to_hex(instance_id: &[u8; 16]) -> String {
    instance_id
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

#[derive(Debug)]
struct InstanceArtifacts {
    instance_id: [u8; 16],
    instance_dir: PathBuf,
    instance_identity: OwnedFileIdentity,
    runtime_dir: PathBuf,
    token_file: PathBuf,
    token_handle: std::sync::Mutex<Option<fs::File>>,
    config_file: PathBuf,
    config_handle: std::sync::Mutex<Option<fs::File>>,
    runtime_relative: String,
}

const INSTANCE_OWNER_MARKER_NAME: &str = ".gila-instance-owner";

fn instance_owner_marker(instance_id: &[u8; 16]) -> String {
    format!(
        "gila-jupyter-instance-v1:{}\n",
        instance_id_to_hex(instance_id)
    )
}

fn create_secure_file(path: &Path, contents: &[u8], description: &str) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {description} {}", path.display()))?;
    #[cfg(windows)]
    {
        let secured = windows_private::enforce_private_path(path, false)
            .with_context(|| format!("failed to secure {description} {}", path.display()))?;
        validate_windows_file_identity(&file, &secured)
            .with_context(|| format!("{description} changed while securing it"))?;
    }
    file.write_all(contents)
        .with_context(|| format!("failed to write {description} {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {description} {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
            anyhow::bail!("{description} was not created with mode 0600");
        }
    }
    Ok(file)
}

fn protected_jupyter_config(base_path: &str, password_hash: Option<&str>) -> Result<Vec<u8>> {
    let encoded_base = serde_json::to_string(base_path)?;
    let mut config = format!(
        "# Generated by Gila; contains no token.\n\
c.ServerApp.base_url = {encoded_base}\n\
c.NotebookApp.base_url = {encoded_base}\n\
c.JupyterNotebookApp.base_url = {encoded_base}\n\
c.ServerApp.custom_display_url = {encoded_base}\n\
c.NotebookApp.custom_display_url = {encoded_base}\n\
c.JupyterNotebookApp.custom_display_url = {encoded_base}\n"
    );
    if let Some(password_hash) = password_hash {
        let encoded_hash = serde_json::to_string(password_hash)?;
        config.push_str(&format!(
            "c.NotebookApp.password = {encoded_hash}\n\
c.PasswordIdentityProvider.hashed_password = {encoded_hash}\n"
        ));
    }
    Ok(config.into_bytes())
}

fn create_instance_artifacts(
    store: &RegistryStore,
    instance_id: &[u8; 16],
    token: &str,
    base_path: &str,
    password_hash: Option<&str>,
) -> Result<InstanceArtifacts> {
    initialize_registry_root(&store.root)?;
    let jupyter_dir = store.root.join("jupyter");
    ensure_private_directory(&jupyter_dir, "Jupyter state")?;
    let instances_dir = jupyter_dir.join("instances");
    ensure_private_directory(&instances_dir, "Jupyter instances")?;

    let instance_hex = instance_id_to_hex(instance_id);
    let instance_dir = instances_dir.join(&instance_hex);
    match create_private_directory(&instance_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            anyhow::bail!("Jupyter instance directory already exists for random identity")
        }
        Err(error) => return Err(error).context("failed to create private instance directory"),
    }
    ensure_private_directory(&instance_dir, "Jupyter instance")?;
    #[cfg(windows)]
    let instance_handle = windows_private::open_path_for_security(&instance_dir, true)
        .context("failed to retain private Jupyter instance identity")?;
    #[cfg(not(windows))]
    let instance_handle = fs::File::open(&instance_dir)
        .context("failed to retain private Jupyter instance identity")?;
    let instance_identity = owned_file_identity(&instance_handle)
        .context("failed to capture private Jupyter instance identity")?;

    let owner_marker = instance_dir.join(INSTANCE_OWNER_MARKER_NAME);
    let runtime_dir = instance_dir.join("runtime");
    let token_file = instance_dir.join("token");
    let config_file = instance_dir.join("jupyter_config.py");
    let creation = (|| -> Result<(fs::File, fs::File)> {
        create_secure_file(
            &owner_marker,
            instance_owner_marker(instance_id).as_bytes(),
            "Jupyter instance ownership marker",
        )?;
        create_private_directory(&runtime_dir)
            .context("failed to create private runtime directory")?;
        ensure_private_directory(&runtime_dir, "Jupyter runtime")?;
        let token_handle = create_secure_file(&token_file, token.as_bytes(), "Jupyter token file")?;
        let config = protected_jupyter_config(base_path, password_hash)?;
        let config_handle = create_secure_file(&config_file, &config, "Jupyter config file")?;
        Ok((token_handle, config_handle))
    })();
    let (token_handle, config_handle) = match creation {
        Ok(handles) => handles,
        Err(error) => {
            let cleanup = remove_private_tree(&instance_dir, instance_id, Some(&instance_identity));
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "{error:#}; partial instance cleanup also failed: {cleanup_error:#}"
                )),
            };
        }
    };

    Ok(InstanceArtifacts {
        instance_id: *instance_id,
        instance_dir,
        instance_identity,
        runtime_dir,
        token_file,
        token_handle: std::sync::Mutex::new(Some(token_handle)),
        config_file,
        config_handle: std::sync::Mutex::new(Some(config_handle)),
        runtime_relative: format!("jupyter/instances/{instance_hex}/runtime"),
    })
}

#[cfg(unix)]
fn random_cleanup_quarantine(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("cleanup path has no parent directory")?;
    use rand::RngCore;
    use std::os::unix::fs::DirBuilderExt;
    for _ in 0..16 {
        let mut random = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut random);
        let container = parent.join(format!(
            ".gila-cleanup-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&container) {
            Ok(()) => return Ok(container.join("owned")),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("failed to create cleanup quarantine"),
        }
    }
    anyhow::bail!("failed to allocate a unique cleanup quarantine path")
}

#[cfg(unix)]
fn remove_empty_quarantine_container(quarantine: &Path) -> Result<()> {
    let container = quarantine
        .parent()
        .context("cleanup quarantine has no container")?;
    fs::remove_dir(container).with_context(|| {
        format!(
            "failed to remove empty cleanup quarantine {}",
            container.display()
        )
    })
}

#[cfg(unix)]
fn restore_quarantined_replacement(
    original: &Path,
    quarantine: &Path,
    description: &str,
) -> Result<()> {
    match fs::symlink_metadata(original) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::rename(quarantine, original)
                .with_context(|| format!("failed to restore preserved {description}"))?;
            remove_empty_quarantine_container(quarantine)
        }
        Ok(_) => anyhow::bail!(
            "{description} changed concurrently; unknown replacement preserved at {}",
            quarantine.display()
        ),
        Err(error) => Err(error).with_context(|| {
            format!("failed to inspect original path while preserving {description}")
        }),
    }
}

/// Move the exact opened Unix object to an unpredictable same-directory name,
/// then re-verify its device/inode before any recursive or unlink operation.
/// If the public path was swapped after validation, the unknown replacement is
/// restored (or retained under the quarantine name) and is never deleted.
#[cfg(unix)]
fn quarantine_verified_unix_path(
    path: &Path,
    expected: &OwnedFileIdentity,
    is_directory: bool,
    description: &str,
) -> Result<PathBuf> {
    let quarantine = random_cleanup_quarantine(path)?;
    fs::rename(path, &quarantine)
        .with_context(|| format!("failed to quarantine {description} before cleanup"))?;

    let verification = (|| -> Result<()> {
        let metadata = fs::symlink_metadata(&quarantine)
            .with_context(|| format!("failed to inspect quarantined {description}"))?;
        if metadata.file_type().is_symlink()
            || (is_directory && !metadata.is_dir())
            || (!is_directory && !metadata.is_file())
        {
            anyhow::bail!("quarantined {description} has an unsafe replacement type");
        }
        let retained = fs::File::open(&quarantine)
            .with_context(|| format!("failed to retain quarantined {description}"))?;
        validate_owned_file_identity(expected, &retained)
            .with_context(|| format!("{description} changed before quarantine"))?;
        let final_metadata = fs::symlink_metadata(&quarantine)
            .with_context(|| format!("failed to re-inspect quarantined {description}"))?;
        if final_metadata.file_type().is_symlink()
            || (is_directory && !final_metadata.is_dir())
            || (!is_directory && !final_metadata.is_file())
        {
            anyhow::bail!("quarantined {description} changed into an unsafe replacement type");
        }
        let final_handle = fs::File::open(&quarantine)
            .with_context(|| format!("failed to re-open quarantined {description}"))?;
        validate_owned_file_identity(expected, &final_handle)
            .with_context(|| format!("{description} changed after quarantine"))
    })();
    if let Err(error) = verification {
        return match restore_quarantined_replacement(path, &quarantine, description) {
            Ok(()) => Err(error),
            Err(restore) => Err(anyhow::anyhow!("{error:#}; {restore:#}")),
        };
    }
    Ok(quarantine)
}

fn erase_owned_secret(expected: &fs::File, description: &str) -> Result<()> {
    expected
        .set_len(0)
        .with_context(|| format!("failed to erase exact {description}"))?;
    expected
        .sync_all()
        .with_context(|| format!("failed to sync erased {description}"))
}

fn remove_owned_secret_file(path: &Path, expected: &fs::File, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
            "{description} was replaced by a symlink; replacement preserved: {}",
            path.display()
        ),
        Ok(metadata) if metadata.is_file() => {
            #[cfg(windows)]
            let opened = windows_private::open_path_for_delete(path, false).with_context(|| {
                format!("{description} is a reparse point or unsafe replacement; preserved")
            })?;
            #[cfg(not(windows))]
            let opened = fs::File::open(path)
                .with_context(|| format!("failed to open {description} for safe cleanup"))?;
            validate_file_identity(&metadata, &opened.metadata()?)
                .with_context(|| format!("{description} changed during cleanup; preserved"))?;
            validate_open_file_identity(expected, &opened)
                .with_context(|| format!("{description} is not the created object; preserved"))?;
            let final_metadata = fs::symlink_metadata(path)
                .with_context(|| format!("failed to re-inspect {description} during cleanup"))?;
            if final_metadata.file_type().is_symlink() || !final_metadata.is_file() {
                anyhow::bail!("{description} changed into an unsafe replacement; preserved");
            }
            validate_file_identity(&opened.metadata()?, &final_metadata)
                .with_context(|| format!("{description} changed during cleanup; preserved"))?;
            #[cfg(windows)]
            {
                let final_handle = windows_private::open_path_for_security(path, false)
                    .with_context(|| format!("{description} became a reparse point; preserved"))?;
                validate_windows_file_identity(&opened, &final_handle)
                    .with_context(|| format!("{description} changed during cleanup; preserved"))?;
                windows_private::delete_open_path(&opened).with_context(|| {
                    format!(
                        "failed to remove {description} {} by handle",
                        path.display()
                    )
                })
            }
            #[cfg(unix)]
            {
                let expected_identity = owned_file_identity(expected)
                    .with_context(|| format!("failed to identify exact {description}"))?;
                let quarantine =
                    quarantine_verified_unix_path(path, &expected_identity, false, description)?;
                fs::remove_file(&quarantine).with_context(|| {
                    format!(
                        "failed to remove quarantined {description} {}",
                        quarantine.display()
                    )
                })?;
                remove_empty_quarantine_container(&quarantine)
            }
            #[cfg(all(not(unix), not(windows)))]
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {description} {}", path.display()))
        }
        Ok(_) => anyhow::bail!(
            "{description} cleanup path is not a file: {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => anyhow::bail!(
            "{description} disappeared before removal; its exact contents were erased"
        ),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {description}")),
    }
}

#[cfg(not(windows))]
fn validate_instance_owner_marker(path: &Path, instance_id: &[u8; 16]) -> Result<()> {
    let expected_name = instance_id_to_hex(instance_id);
    if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        anyhow::bail!("private instance cleanup path is not named for its instance identity");
    }
    let path_metadata = fs::symlink_metadata(path)
        .context("failed to inspect private instance tree before ownership validation")?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_dir() {
        anyhow::bail!("private instance cleanup path is a symlink or non-directory; preserved");
    }
    let directory_handle =
        fs::File::open(path).context("failed to retain private instance directory identity")?;
    validate_file_identity(&path_metadata, &directory_handle.metadata()?)
        .context("private instance directory changed while validating ownership")?;

    let marker_path = path.join(INSTANCE_OWNER_MARKER_NAME);
    let marker_metadata = fs::symlink_metadata(&marker_path)
        .context("private instance ownership marker is missing; replacement preserved")?;
    if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
        anyhow::bail!(
            "private instance ownership marker is not a regular file; replacement preserved"
        );
    }
    if marker_metadata.len() > 128 {
        anyhow::bail!("private instance ownership marker is oversized; replacement preserved");
    }
    let mut marker =
        fs::File::open(&marker_path).context("failed to open private instance ownership marker")?;
    validate_file_identity(&marker_metadata, &marker.metadata()?)
        .context("private instance ownership marker changed while opening")?;
    let mut contents = String::new();
    (&mut marker)
        .take(129)
        .read_to_string(&mut contents)
        .context("failed to read private instance ownership marker")?;
    if contents != instance_owner_marker(instance_id) {
        anyhow::bail!("private instance ownership marker does not match; replacement preserved");
    }
    let final_path_metadata = fs::symlink_metadata(path)
        .context("private instance directory disappeared during ownership validation")?;
    validate_file_identity(&directory_handle.metadata()?, &final_path_metadata)
        .context("private instance directory changed during ownership validation")?;
    Ok(())
}

#[cfg(not(windows))]
fn validate_owned_instance_tree_contents(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path).context("failed to inspect owned instance contents")? {
        let entry = entry.context("failed to inspect owned instance entry")?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .context("owned instance entry name is not valid UTF-8")?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)
            .with_context(|| format!("failed to inspect owned instance entry {name}"))?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("owned instance entry {name} is a replacement symlink; tree preserved");
        }
        match name {
            INSTANCE_OWNER_MARKER_NAME | "token" | "jupyter_config.py" => {
                if !metadata.is_file() {
                    anyhow::bail!(
                        "owned instance entry {name} is not an expected regular file; tree preserved"
                    );
                }
                #[cfg(windows)]
                windows_private::open_path_for_security(&entry_path, false).with_context(|| {
                    format!("owned instance entry {name} is a reparse point; tree preserved")
                })?;
            }
            "runtime" => {
                if !metadata.is_dir() {
                    anyhow::bail!("owned runtime entry is not a directory; tree preserved");
                }
                #[cfg(windows)]
                windows_private::open_path_for_security(&entry_path, true)
                    .context("owned runtime entry is a reparse point; tree preserved")?;
                let mut runtime_entries = 0usize;
                for runtime_entry in
                    fs::read_dir(&entry_path).context("failed to inspect owned runtime contents")?
                {
                    runtime_entries += 1;
                    if runtime_entries > 4_096 {
                        anyhow::bail!("owned runtime cleanup exceeds the 4096-entry safety limit");
                    }
                    let runtime_entry =
                        runtime_entry.context("failed to inspect owned runtime cleanup entry")?;
                    let runtime_path = runtime_entry.path();
                    let runtime_metadata = fs::symlink_metadata(&runtime_path)
                        .context("failed to inspect owned runtime cleanup path")?;
                    if runtime_metadata.file_type().is_symlink() || !runtime_metadata.is_file() {
                        anyhow::bail!(
                            "owned runtime contains a symlink, reparse point, or non-file replacement; tree preserved"
                        );
                    }
                    #[cfg(windows)]
                    windows_private::open_path_for_security(&runtime_path, false)
                        .context("owned runtime contains a reparse point; tree preserved")?;
                }
            }
            _ => anyhow::bail!("owned instance contains unrecognized entry {name}; tree preserved"),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn pin_windows_instance_ancestors(path: &Path) -> Result<Vec<fs::File>> {
    let instances = path
        .parent()
        .context("private instance path has no instances parent")?;
    let jupyter = instances
        .parent()
        .context("private instance path has no Jupyter parent")?;
    let root = jupyter
        .parent()
        .context("private instance path has no registry root")?;
    if instances.file_name().and_then(|name| name.to_str()) != Some("instances")
        || jupyter.file_name().and_then(|name| name.to_str()) != Some("jupyter")
    {
        anyhow::bail!("private instance path is outside the managed Jupyter hierarchy");
    }

    let mut pins = Vec::with_capacity(3);
    // Pin from the managed root downward. Once a parent is pinned without
    // delete sharing, opening the next descendant by path cannot be redirected
    // through a renamed/replaced managed ancestor.
    for (ancestor, description) in [
        (root, "registry root"),
        (jupyter, "Jupyter state directory"),
        (instances, "Jupyter instances directory"),
    ] {
        let metadata = fs::symlink_metadata(ancestor)
            .with_context(|| format!("failed to inspect managed {description}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("managed {description} is a symlink or non-directory; tree preserved");
        }
        let handle = windows_private::pin_directory_for_cleanup(ancestor)
            .with_context(|| format!("failed to pin managed {description}; tree preserved"))?;
        validate_file_identity(&metadata, &handle.metadata()?)
            .with_context(|| format!("managed {description} changed while pinning"))?;
        pins.push(handle);
    }
    Ok(pins)
}

fn remove_private_tree(
    path: &Path,
    instance_id: &[u8; 16],
    expected_root: Option<&OwnedFileIdentity>,
) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
            "private instance directory was replaced by a symlink; replacement preserved"
        ),
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!("private instance cleanup path is not a directory; replacement preserved")
        }
        Ok(_metadata) => {
            #[cfg(windows)]
            let _ancestor_pins = pin_windows_instance_ancestors(path)?;
            #[cfg(windows)]
            let directory_handle = {
                let handle = windows_private::open_path_for_delete(path, true).context(
                    "private instance cleanup path is a reparse point or unsafe directory",
                )?;
                validate_file_identity(&_metadata, &handle.metadata()?)
                    .context("private instance directory changed before cleanup")?;
                if let Some(expected) = expected_root {
                    validate_owned_file_identity(expected, &handle).context(
                        "private instance directory is not the created object; replacement preserved",
                    )?;
                }
                handle
            };
            #[cfg(windows)]
            {
                let cleanup = (|| -> Result<()> {
                    let owned = open_private_tree_windows(path, instance_id, directory_handle)?;
                    remove_private_tree_windows(owned)
                })();
                match cleanup {
                    Ok(()) => Ok(()),
                    Err(_error)
                        if expected_root.is_none()
                            && matches!(
                                fs::symlink_metadata(path),
                                Err(ref missing) if missing.kind() == io::ErrorKind::NotFound
                            ) =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            #[cfg(not(windows))]
            {
                let cleanup = (|| -> Result<()> {
                    let expected = match expected_root.copied() {
                        Some(expected) => expected,
                        None => {
                            let opened = fs::File::open(path)
                                .context("failed to retain private instance directory identity")?;
                            owned_file_identity(&opened)
                                .context("failed to identify private instance directory")?
                        }
                    };
                    validate_owned_file_identity(
                        &expected,
                        &fs::File::open(path).context(
                            "failed to verify current private instance directory identity",
                        )?,
                    )
                    .context(
                        "private instance directory is not the created object; replacement preserved",
                    )?;
                    validate_instance_owner_marker(path, instance_id)?;
                    validate_owned_instance_tree_contents(path)?;
                    #[cfg(unix)]
                    let cleanup_path = quarantine_verified_unix_path(
                        path,
                        &expected,
                        true,
                        "private instance tree",
                    )?;
                    #[cfg(not(unix))]
                    let cleanup_path = path.to_path_buf();
                    match fs::remove_dir_all(&cleanup_path) {
                        Ok(()) => {
                            #[cfg(unix)]
                            remove_empty_quarantine_container(&cleanup_path)?;
                            Ok(())
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            #[cfg(unix)]
                            remove_empty_quarantine_container(&cleanup_path)?;
                            Ok(())
                        }
                        Err(error) => Err(error).with_context(|| {
                            format!(
                                "failed to remove private instance tree {}",
                                cleanup_path.display()
                            )
                        }),
                    }
                })();
                match cleanup {
                    Ok(()) => Ok(()),
                    Err(_error)
                        if expected_root.is_none()
                            && matches!(
                                fs::symlink_metadata(path),
                                Err(ref missing) if missing.kind() == io::ErrorKind::NotFound
                            ) =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to inspect private instance tree"),
    }
}

#[cfg(windows)]
struct WindowsOwnedInstanceTree {
    root: fs::File,
    runtime: Option<fs::File>,
    runtime_files: Vec<fs::File>,
    top_level_files: Vec<fs::File>,
}

#[cfg(windows)]
fn open_windows_cleanup_file(path: &Path, description: &str) -> Result<fs::File> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {description}; tree preserved"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("{description} is a symlink, reparse point, or non-file; tree preserved");
    }
    let handle = windows_private::open_path_for_delete(path, false)
        .with_context(|| format!("failed to pin {description}; tree preserved"))?;
    let current = windows_private::open_path_for_security(path, false)
        .with_context(|| format!("failed to verify {description}; tree preserved"))?;
    validate_windows_file_identity(&handle, &current)
        .with_context(|| format!("{description} changed while pinning; tree preserved"))?;
    Ok(handle)
}

#[cfg(windows)]
fn open_private_tree_windows(
    path: &Path,
    instance_id: &[u8; 16],
    root: fs::File,
) -> Result<WindowsOwnedInstanceTree> {
    windows_private::validate_private_handle(&root, true)
        .context("private instance directory ACL/owner is unsafe; tree preserved")?;
    let mut runtime = None;
    let mut runtime_files = Vec::new();
    let mut top_level_files = Vec::new();
    let mut marker_seen = false;

    for entry in fs::read_dir(path).context("failed to inspect owned instance contents")? {
        let entry = entry.context("failed to inspect owned instance entry")?;
        let name = entry
            .file_name()
            .to_str()
            .context("owned instance entry name is not valid UTF-8")?
            .to_string();
        let entry_path = entry.path();
        match name.as_str() {
            INSTANCE_OWNER_MARKER_NAME | "token" | "jupyter_config.py" => {
                let mut handle = open_windows_cleanup_file(
                    &entry_path,
                    &format!("owned instance entry {name}"),
                )?;
                windows_private::validate_private_handle(&handle, false).with_context(|| {
                    format!("owned instance entry {name} ACL/owner is unsafe; tree preserved")
                })?;
                if name == INSTANCE_OWNER_MARKER_NAME {
                    if handle.metadata()?.len() > 128 {
                        anyhow::bail!(
                            "private instance ownership marker is oversized; replacement preserved"
                        );
                    }
                    let mut contents = String::new();
                    (&mut handle)
                        .take(129)
                        .read_to_string(&mut contents)
                        .context("failed to read private instance ownership marker")?;
                    if contents != instance_owner_marker(instance_id) {
                        anyhow::bail!(
                            "private instance ownership marker does not match; replacement preserved"
                        );
                    }
                    marker_seen = true;
                }
                top_level_files.push(handle);
            }
            "runtime" => {
                let metadata = fs::symlink_metadata(&entry_path)
                    .context("failed to inspect owned runtime entry")?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    anyhow::bail!("owned runtime entry is unsafe; tree preserved");
                }
                let handle = windows_private::open_path_for_delete(&entry_path, true)
                    .context("failed to pin owned runtime; tree preserved")?;
                windows_private::validate_private_handle(&handle, true)
                    .context("owned runtime ACL/owner is unsafe; tree preserved")?;
                let current = windows_private::open_path_for_security(&entry_path, true)
                    .context("failed to verify owned runtime; tree preserved")?;
                validate_windows_file_identity(&handle, &current)
                    .context("owned runtime changed while pinning; tree preserved")?;

                let mut entries = 0usize;
                for runtime_entry in
                    fs::read_dir(&entry_path).context("failed to inspect owned runtime contents")?
                {
                    entries += 1;
                    if entries > 4_096 {
                        anyhow::bail!("owned runtime cleanup exceeds the 4096-entry safety limit");
                    }
                    let runtime_entry =
                        runtime_entry.context("failed to inspect owned runtime cleanup entry")?;
                    runtime_files.push(open_windows_cleanup_file(
                        &runtime_entry.path(),
                        "Jupyter runtime file",
                    )?);
                }
                runtime = Some(handle);
            }
            _ => anyhow::bail!("owned instance contains unrecognized entry {name}; tree preserved"),
        }
    }
    if !marker_seen {
        anyhow::bail!("private instance ownership marker is missing; replacement preserved");
    }
    Ok(WindowsOwnedInstanceTree {
        root,
        runtime,
        runtime_files,
        top_level_files,
    })
}

#[cfg(windows)]
fn remove_private_tree_windows(mut owned: WindowsOwnedInstanceTree) -> Result<()> {
    for file in &owned.runtime_files {
        windows_private::delete_open_path(file)
            .context("failed to remove Jupyter runtime file by handle")?;
    }
    owned.runtime_files.clear();
    if let Some(runtime) = owned.runtime.take() {
        windows_private::delete_open_path(&runtime)
            .context("failed to remove owned runtime directory by handle")?;
        drop(runtime);
    }
    for file in &owned.top_level_files {
        windows_private::delete_open_path(file)
            .context("failed to remove owned instance file by handle")?;
    }
    owned.top_level_files.clear();
    windows_private::delete_open_path(&owned.root)
        .context("failed to remove private instance directory by handle")
}

impl InstanceArtifacts {
    fn take_secret_handle(
        slot: &std::sync::Mutex<Option<fs::File>>,
        description: &str,
    ) -> Result<Option<fs::File>> {
        slot.lock()
            .map_err(|_| anyhow::anyhow!("{description} handle lock was poisoned"))
            .map(|mut handle| handle.take())
    }

    fn cleanup_secrets(&self) -> Result<()> {
        // Erase both exact create_new objects before consulting any path. A
        // live child can rename its instance directory, but it cannot make us
        // return early while either retained secret still has contents.
        let mut errors = Vec::new();
        let token_handle = match Self::take_secret_handle(&self.token_handle, "Jupyter token file")
        {
            Ok(Some(handle)) => Some(handle),
            Ok(None) => {
                errors.push("Jupyter token file handle was already released".to_string());
                None
            }
            Err(error) => {
                errors.push(format!("{error:#}"));
                None
            }
        };
        let config_handle =
            match Self::take_secret_handle(&self.config_handle, "Jupyter config file") {
                Ok(Some(handle)) => Some(handle),
                Ok(None) => {
                    errors.push("Jupyter config file handle was already released".to_string());
                    None
                }
                Err(error) => {
                    errors.push(format!("{error:#}"));
                    None
                }
            };
        if let Some(handle) = &token_handle {
            if let Err(error) = erase_owned_secret(handle, "Jupyter token file") {
                errors.push(format!("{error:#}"));
            }
        }
        if let Some(handle) = &config_handle {
            if let Err(error) = erase_owned_secret(handle, "Jupyter config file") {
                errors.push(format!("{error:#}"));
            }
        }

        #[cfg(windows)]
        let pins = (|| -> Result<(Vec<fs::File>, fs::File)> {
            let ancestors = pin_windows_instance_ancestors(&self.instance_dir)?;
            let instance = windows_private::pin_directory_for_cleanup(&self.instance_dir)
                .context("failed to pin private instance directory during secret cleanup")?;
            validate_owned_file_identity(&self.instance_identity, &instance).context(
                "private instance directory changed before secret cleanup; replacement preserved",
            )?;
            Ok((ancestors, instance))
        })();
        #[cfg(windows)]
        if let Err(error) = &pins {
            errors.push(format!("{error:#}"));
        }
        #[cfg(windows)]
        let paths_are_pinned = pins.is_ok();
        #[cfg(not(windows))]
        let paths_are_pinned = true;

        if paths_are_pinned {
            if let Some(handle) = &token_handle {
                if let Err(error) =
                    remove_owned_secret_file(&self.token_file, handle, "Jupyter token file")
                {
                    errors.push(format!("{error:#}"));
                }
            }
            if let Some(handle) = &config_handle {
                if let Err(error) =
                    remove_owned_secret_file(&self.config_file, handle, "Jupyter config file")
                {
                    errors.push(format!("{error:#}"));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(errors.join("; ")))
        }
    }

    fn cleanup_all(&self) -> Result<()> {
        let mut errors = Vec::new();
        for (slot, description) in [
            (&self.token_handle, "Jupyter token file"),
            (&self.config_handle, "Jupyter config file"),
        ] {
            match Self::take_secret_handle(slot, description) {
                Ok(Some(handle)) => {
                    if let Err(error) = erase_owned_secret(&handle, description) {
                        errors.push(format!("{error:#}"));
                    }
                    // Close the retained handle before Windows checks whether
                    // the instance directory is empty and dispositions it.
                    drop(handle);
                }
                Ok(None) => {}
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        if let Err(error) = remove_private_tree(
            &self.instance_dir,
            &self.instance_id,
            Some(&self.instance_identity),
        ) {
            errors.push(format!("{error:#}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(errors.join("; ")))
        }
    }
}

#[cfg(test)]
mod instance_cleanup_tests {
    use super::*;
    use tempfile::TempDir;

    fn artifacts(tmp: &TempDir, id: [u8; 16]) -> InstanceArtifacts {
        let store = RegistryStore::for_test(tmp.path().join("registry")).expect("test registry");
        create_instance_artifacts(
            &store,
            &id,
            "cleanup-test-token",
            &instance_id_to_base_path(&id),
            None,
        )
        .expect("instance artifacts")
    }

    #[test]
    fn cleanup_requires_the_exact_owner_marker() {
        let tmp = TempDir::new().expect("temp dir");
        let id = [0x31; 16];
        let artifacts = artifacts(&tmp, id);
        fs::write(
            artifacts.instance_dir.join(INSTANCE_OWNER_MARKER_NAME),
            instance_owner_marker(&[0x32; 16]),
        )
        .expect("replace marker contents");

        let error = artifacts
            .cleanup_all()
            .expect_err("mismatched owner must survive");
        assert!(error.to_string().contains("does not match"));
        assert!(artifacts.instance_dir.exists());
        fs::write(
            artifacts.instance_dir.join(INSTANCE_OWNER_MARKER_NAME),
            instance_owner_marker(&id),
        )
        .expect("restore marker");
        artifacts.cleanup_all().expect("owned cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_a_replacement_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");
        let id = [0x41; 16];
        let artifacts = artifacts(&tmp, id);
        let retained = tmp.path().join("retained-owned-instance");
        fs::rename(&artifacts.instance_dir, &retained).expect("retain owner");
        let unrelated = tmp.path().join("unrelated");
        fs::create_dir(&unrelated).expect("unrelated directory");
        fs::write(unrelated.join("sentinel"), b"preserve").expect("sentinel");
        symlink(&unrelated, &artifacts.instance_dir).expect("replacement symlink");

        let error = artifacts.cleanup_all().expect_err("symlink must survive");
        assert!(error.to_string().contains("symlink"));
        assert!(fs::symlink_metadata(&artifacts.instance_dir)
            .expect("replacement metadata")
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(unrelated.join("sentinel")).unwrap(), b"preserve");

        fs::remove_file(&artifacts.instance_dir).expect("remove test symlink");
        fs::rename(&retained, &artifacts.instance_dir).expect("restore owner");
        artifacts.cleanup_all().expect("owned cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_an_ordinary_directory_replacing_the_created_instance() {
        let tmp = TempDir::new().expect("temp dir");
        let id = [0x44; 16];
        let artifacts = artifacts(&tmp, id);
        let retained = tmp.path().join("retained-created-instance");
        fs::rename(&artifacts.instance_dir, &retained).expect("retain created instance");
        create_private_directory(&artifacts.instance_dir).expect("replacement directory");
        fs::write(artifacts.instance_dir.join("sentinel"), b"preserve")
            .expect("replacement sentinel");

        let error = artifacts
            .cleanup_all()
            .expect_err("ordinary replacement must survive");
        assert!(error.to_string().contains("not the created object"));
        assert_eq!(
            fs::read(artifacts.instance_dir.join("sentinel")).unwrap(),
            b"preserve"
        );
        assert!(retained.is_dir(), "created instance must remain retained");

        fs::remove_dir_all(&artifacts.instance_dir).expect("remove test replacement");
        fs::rename(&retained, &artifacts.instance_dir).expect("restore created instance");
        artifacts.cleanup_all().expect("owned cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn unix_quarantine_restores_a_replacement_selected_after_identity_capture() {
        let tmp = TempDir::new().expect("temp dir");
        let original = tmp.path().join("owned-directory");
        fs::create_dir(&original).expect("owned directory");
        let expected = fs::File::open(&original).expect("retain owned identity");
        let expected_identity = owned_file_identity(&expected).expect("capture owned identity");
        let retained = tmp.path().join("retained-owned-directory");
        fs::rename(&original, &retained).expect("retain exact directory");
        fs::create_dir(&original).expect("replacement directory");
        fs::write(original.join("sentinel"), b"preserve").expect("replacement sentinel");

        let error = quarantine_verified_unix_path(
            &original,
            &expected_identity,
            true,
            "owned directory fixture",
        )
        .expect_err("replacement identity must be restored");
        assert!(error.to_string().contains("changed before quarantine"));
        assert_eq!(fs::read(original.join("sentinel")).unwrap(), b"preserve");
        assert!(retained.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_a_symlink_replacing_an_owned_secret_file() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");
        let id = [0x43; 16];
        let artifacts = artifacts(&tmp, id);
        fs::remove_file(&artifacts.token_file).expect("remove owned token");
        let unrelated = tmp.path().join("unrelated-token-target");
        fs::write(&unrelated, b"preserve").expect("unrelated target");
        symlink(&unrelated, &artifacts.token_file).expect("replacement token symlink");

        let error = artifacts
            .cleanup_all()
            .expect_err("child symlink must survive");
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read(&unrelated).unwrap(), b"preserve");
        assert!(fs::symlink_metadata(&artifacts.token_file)
            .expect("replacement metadata")
            .file_type()
            .is_symlink());

        fs::remove_file(&artifacts.token_file).expect("remove test symlink");
        create_secure_file(
            &artifacts.token_file,
            b"cleanup-test-token",
            "restored token fixture",
        )
        .expect("restore token");
        artifacts.cleanup_all().expect("owned cleanup");
    }

    #[test]
    fn secret_cleanup_erases_the_created_file_and_preserves_a_regular_replacement() {
        let tmp = TempDir::new().expect("temp dir");
        let id = [0x45; 16];
        let artifacts = artifacts(&tmp, id);
        let retained = tmp.path().join("renamed-created-token");
        fs::rename(&artifacts.token_file, &retained).expect("rename created token");
        create_secure_file(
            &artifacts.token_file,
            b"unrelated replacement",
            "replacement token fixture",
        )
        .expect("replacement token");

        let error = artifacts
            .cleanup_secrets()
            .expect_err("regular replacement must survive");
        assert!(error.to_string().contains("not the created object"));
        assert_eq!(fs::metadata(&retained).unwrap().len(), 0);
        assert_eq!(
            fs::read(&artifacts.token_file).unwrap(),
            b"unrelated replacement"
        );
        assert!(
            !artifacts.config_file.exists(),
            "the other exact secret should still be removed"
        );

        fs::remove_file(&artifacts.token_file).expect("remove test replacement");
        fs::remove_file(&retained).expect("remove erased retained token");
        artifacts.cleanup_all().expect("owned cleanup");
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_preserves_a_replacement_directory_reparse_point() {
        use std::os::windows::fs::symlink_dir;

        let tmp = TempDir::new().expect("temp dir");
        let id = [0x42; 16];
        let artifacts = artifacts(&tmp, id);
        let retained = tmp.path().join("retained-owned-instance");
        fs::rename(&artifacts.instance_dir, &retained).expect("retain owner");
        let unrelated = tmp.path().join("unrelated");
        fs::create_dir(&unrelated).expect("unrelated directory");
        fs::write(unrelated.join("sentinel"), b"preserve").expect("sentinel");
        match symlink_dir(&unrelated, &artifacts.instance_dir) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => {
                // Creating symlinks requires Developer Mode or
                // SeCreateSymbolicLinkPrivilege. The separate direct-handle
                // test below still covers safe exact-object deletion.
                fs::rename(&retained, &artifacts.instance_dir).expect("restore owner");
                artifacts.cleanup_all().expect("owned cleanup");
                return;
            }
            Err(error) => panic!("unexpected symlink creation error: {error}"),
        }

        let error = artifacts
            .cleanup_all()
            .expect_err("reparse point must survive");
        assert!(error.to_string().contains("symlink") || error.to_string().contains("reparse"));
        assert_eq!(fs::read(unrelated.join("sentinel")).unwrap(), b"preserve");

        fs::remove_dir(&artifacts.instance_dir).expect("remove test reparse point");
        fs::rename(&retained, &artifacts.instance_dir).expect("restore owner");
        artifacts.cleanup_all().expect("owned cleanup");
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_deletes_the_open_identity_and_blocks_path_replacement() {
        let tmp = TempDir::new().expect("temp dir");
        let path = tmp.path().join("owned-file");
        create_secure_file(&path, b"owned", "owned fixture").expect("secure file");
        let opened = windows_private::open_path_for_delete(&path, false).expect("delete handle");
        let replacement = tmp.path().join("replacement");
        create_secure_file(&replacement, b"replacement", "replacement fixture")
            .expect("replacement");
        let moved = tmp.path().join("moved-owned-file");
        assert!(
            fs::rename(&path, &moved).is_err(),
            "delete-exclusive handle must prevent a same-path replacement"
        );
        windows_private::delete_open_path(&opened).expect("delete exact handle");
        drop(opened);
        assert!(!path.exists());
        assert_eq!(fs::read(&replacement).unwrap(), b"replacement");
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_pins_every_managed_ancestor_against_replacement() {
        let tmp = TempDir::new().expect("temp dir");
        let id = [0x63; 16];
        let artifacts = artifacts(&tmp, id);
        let pins = pin_windows_instance_ancestors(&artifacts.instance_dir).expect("ancestor pins");
        assert_eq!(pins.len(), 3);
        let instances = artifacts.instance_dir.parent().unwrap();
        let moved = tmp.path().join("moved-instances");
        assert!(
            fs::rename(instances, &moved).is_err(),
            "pinned managed ancestor must not be renameable"
        );
        drop(pins);
        artifacts.cleanup_all().expect("owned cleanup");
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_open_rejects_an_unprivileged_directory_junction() {
        let tmp = TempDir::new().expect("temp dir");
        let target = tmp.path().join("junction-target");
        let junction = tmp.path().join("junction");
        fs::create_dir(&target).expect("junction target");
        let system_root = std::env::var_os("SYSTEMROOT").expect("SYSTEMROOT");
        let output = Command::new(PathBuf::from(system_root).join("System32/cmd.exe"))
            .arg("/D")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&junction)
            .arg(&target)
            .output()
            .expect("launch mklink junction");
        assert!(
            output.status.success(),
            "mklink /J failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let error = windows_private::open_path_for_security(&junction, true)
            .expect_err("directory junction must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir(&junction).expect("remove junction without traversal");
    }

    #[test]
    fn runtime_directory_entry_work_has_an_exact_cap() {
        let tmp = TempDir::new().expect("temp dir");
        let id = [0x51; 16];
        let artifacts = artifacts(&tmp, id);
        for index in 0..RUNTIME_DIRECTORY_ENTRY_CAP {
            create_secure_file(
                &artifacts.runtime_dir.join(format!("noise-{index:04}")),
                b"noise",
                "runtime noise fixture",
            )
            .expect("noise entry");
        }
        assert!(matches!(
            discover_runtime_endpoint(
                &artifacts.runtime_dir,
                "cleanup-test-token",
                &instance_id_to_base_path(&id),
                "127.0.0.1".parse().unwrap(),
            )
            .expect("exact entry cap"),
            RuntimeDiscovery::Pending
        ));
        create_secure_file(
            &artifacts.runtime_dir.join("noise-over-cap"),
            b"noise",
            "runtime noise fixture",
        )
        .expect("over-cap entry");
        let error = discover_runtime_endpoint(
            &artifacts.runtime_dir,
            "cleanup-test-token",
            &instance_id_to_base_path(&id),
            "127.0.0.1".parse().unwrap(),
        )
        .err()
        .expect("entry flood rejected");
        assert!(error.to_string().contains("entry work limit"));
        artifacts.cleanup_all().expect("owned cleanup");
    }
}

const RUNTIME_FILE_SIZE_CAP: u64 = 64 * 1024;
/// Bound hostile or broken runtime-directory work before inspecting metadata.
const RUNTIME_DIRECTORY_ENTRY_CAP: usize = 256;

#[derive(Debug, Deserialize)]
struct RuntimeServerFile {
    base_url: String,
    pid: u64,
    port: u64,
    token: String,
    url: String,
}

enum RuntimeDiscovery {
    Pending,
    Ready(JupyterEndpoint, u32),
}

fn runtime_file_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    (name.starts_with("jpserver-") || name.starts_with("nbserver-")) && name.ends_with(".json")
}

fn runtime_file_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let stem = name
        .strip_prefix("jpserver-")
        .or_else(|| name.strip_prefix("nbserver-"))?
        .strip_suffix(".json")?;
    stem.parse::<u32>().ok().filter(|pid| *pid != 0)
}

fn validate_private_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {description} directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("{description} must be a regular non-symlink directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o700 {
            anyhow::bail!("{description} directory must have mode 0700");
        }
    }
    #[cfg(windows)]
    {
        windows_private::enforce_private_path(path, true).with_context(|| {
            format!("failed to enforce private Windows ACL on {description} directory")
        })?;
    }
    Ok(())
}

fn discover_runtime_endpoint(
    runtime_dir: &Path,
    expected_token: &str,
    expected_base_path: &str,
    expected_ip: IpAddr,
) -> Result<RuntimeDiscovery> {
    validate_private_directory(runtime_dir, "Jupyter runtime")?;
    let mut candidates = Vec::new();
    let mut entries_seen = 0usize;
    for entry in
        fs::read_dir(runtime_dir).context("failed to read private Jupyter runtime directory")?
    {
        entries_seen += 1;
        if entries_seen > RUNTIME_DIRECTORY_ENTRY_CAP {
            anyhow::bail!(
                "Jupyter runtime directory exceeds the {RUNTIME_DIRECTORY_ENTRY_CAP}-entry work limit"
            );
        }
        let entry = entry.context("failed to inspect Jupyter runtime directory entry")?;
        if runtime_file_name(&entry.path()) {
            candidates.push(entry.path());
            if candidates.len() > 1 {
                anyhow::bail!(
                    "expected exactly one Jupyter runtime server file, found more than one"
                );
            }
        }
    }
    if candidates.is_empty() {
        return Ok(RuntimeDiscovery::Pending);
    }
    let path = &candidates[0];
    let filename_pid = runtime_file_pid(path)
        .context("Jupyter runtime server filename must contain a positive numeric pid")?;
    let path_metadata =
        fs::symlink_metadata(path).context("failed to inspect Jupyter runtime file")?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        anyhow::bail!("Jupyter runtime server file must be a regular non-symlink file");
    }
    if path_metadata.len() > RUNTIME_FILE_SIZE_CAP {
        anyhow::bail!("Jupyter runtime server file exceeds the 64 KiB limit");
    }
    #[cfg(windows)]
    let mut file = windows_private::enforce_private_path(path, false)
        .context("failed to securely open Jupyter runtime server file")?;
    #[cfg(not(windows))]
    let mut file = fs::OpenOptions::new()
        .read(true)
        .open(path)
        .context("failed to open Jupyter runtime server file")?;
    let opened_metadata = file
        .metadata()
        .context("failed to inspect opened runtime file")?;
    validate_file_identity(&path_metadata, &opened_metadata)
        .context("Jupyter runtime file changed while opening")?;
    let mut bytes = Vec::with_capacity(usize::try_from(path_metadata.len()).unwrap_or(0));
    (&mut file)
        .take(RUNTIME_FILE_SIZE_CAP + 1)
        .read_to_end(&mut bytes)
        .context("failed to read Jupyter runtime server file")?;
    let final_path_metadata =
        fs::symlink_metadata(path).context("failed to re-inspect Jupyter runtime file")?;
    if final_path_metadata.file_type().is_symlink() || !final_path_metadata.is_file() {
        anyhow::bail!("Jupyter runtime server file changed into a non-regular file");
    }
    validate_file_identity(&opened_metadata, &final_path_metadata)
        .context("Jupyter runtime file changed while reading")?;
    #[cfg(windows)]
    {
        let final_handle = windows_private::open_path_for_security(path, false)
            .context("failed to re-open Jupyter runtime server file without reparsing")?;
        validate_windows_file_identity(&file, &final_handle)
            .context("Jupyter runtime file changed while reading")?;
    }
    if bytes.len() as u64 > RUNTIME_FILE_SIZE_CAP {
        anyhow::bail!("Jupyter runtime server file grew beyond the 64 KiB limit");
    }
    let runtime: RuntimeServerFile = match serde_json::from_slice(&bytes) {
        Ok(runtime) => runtime,
        Err(error) if error.is_eof() => return Ok(RuntimeDiscovery::Pending),
        Err(error) => return Err(error).context("malformed Jupyter runtime server JSON"),
    };
    if runtime.token != expected_token {
        anyhow::bail!("Jupyter runtime token does not exactly match the expected token");
    }
    if runtime.base_url != expected_base_path {
        anyhow::bail!(
            "Jupyter runtime base_url mismatch: expected {expected_base_path}, got {}",
            runtime.base_url
        );
    }
    let port = u16::try_from(runtime.port).context("Jupyter runtime port is not a nonzero u16")?;
    if port == 0 {
        anyhow::bail!("Jupyter runtime port must not be zero");
    }
    let pid = u32::try_from(runtime.pid).context("Jupyter runtime pid is not a positive u32")?;
    if pid == 0 {
        anyhow::bail!("Jupyter runtime pid must not be zero");
    }
    if pid != filename_pid {
        anyhow::bail!("Jupyter runtime filename pid does not match its JSON pid field");
    }
    let parsed = url::Url::parse(&runtime.url).context("Jupyter runtime URL is invalid")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        anyhow::bail!("Jupyter runtime URL has an unsafe scheme or authority component");
    }
    let ip = typed_loopback_host(&parsed)?;
    if ip != expected_ip {
        anyhow::bail!("Jupyter runtime URL host does not match the controlled bind address");
    }
    let url_port =
        explicit_port(&runtime.url).context("Jupyter runtime URL needs an explicit port")?;
    if url_port != port {
        anyhow::bail!("Jupyter runtime URL port does not match its numeric port field");
    }
    if parsed.path() != expected_base_path {
        anyhow::bail!("Jupyter runtime URL does not use the exact instance base path");
    }

    Ok(RuntimeDiscovery::Ready(
        JupyterEndpoint {
            url: runtime.url,
            host: ip.to_string(),
            port,
            token: runtime.token,
        },
        pid,
    ))
}

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STARTUP_HTTP_TIMEOUT: Duration = Duration::from_millis(500);
const LIFECYCLE_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// A kernels response is diagnostic lifecycle data, not an unbounded notebook
/// payload. Eight list workers can therefore retain at most 512 KiB of response
/// bodies in aggregate before decoding.
const LIFECYCLE_JSON_BODY_CAP: usize = 64 * 1024;
const LIST_AGGREGATE_TIMEOUT: Duration = Duration::from_secs(3);
const LIST_PROBE_CONCURRENCY: usize = 8;
const LISTENER_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
const DUPLICATE_STOP_RACE_TIMEOUT: Duration = Duration::from_millis(750);

fn lifecycle_http_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(1)))
        // Registry URLs are typed loopback endpoints. Never route the
        // authentication token through HTTP(S)/ALL_PROXY environment state.
        .no_proxy()
        // A registered loopback endpoint must never redirect a privileged
        // authenticated request to a different origin.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build Jupyter HTTP client")
}

fn bounded_text(text: impl AsRef<str>, cap: usize) -> String {
    let text = text.as_ref();
    if text.len() <= cap {
        return text.to_string();
    }
    const PREFIX: &str = "[...truncated...] ";
    if cap <= PREFIX.len() {
        return PREFIX[..cap].to_string();
    }
    let suffix_cap = cap - PREFIX.len();
    let mut start = text.len() - suffix_cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{PREFIX}{}", &text[start..])
}

fn form_urlencoded_secret(secret: &str) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("secret", secret);
    serializer
        .finish()
        .strip_prefix("secret=")
        .unwrap_or_default()
        .to_string()
}

fn percent_hex_case(value: &str, uppercase: bool) -> String {
    let mut encoded = value.as_bytes().to_vec();
    let mut index = 0;
    while index + 2 < encoded.len() {
        if encoded[index] == b'%'
            && encoded[index + 1].is_ascii_hexdigit()
            && encoded[index + 2].is_ascii_hexdigit()
        {
            encoded[index + 1] = if uppercase {
                encoded[index + 1].to_ascii_uppercase()
            } else {
                encoded[index + 1].to_ascii_lowercase()
            };
            encoded[index + 2] = if uppercase {
                encoded[index + 2].to_ascii_uppercase()
            } else {
                encoded[index + 2].to_ascii_lowercase()
            };
            index += 3;
        } else {
            index += 1;
        }
    }
    String::from_utf8(encoded).expect("URL encodings are ASCII")
}

fn fully_percent_encoded_secret(secret: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(secret.len().saturating_mul(3));
    for byte in secret.as_bytes() {
        let _ = write!(encoded, "%{byte:02X}");
    }
    encoded
}

fn standard_percent_encoded_secret(secret: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(secret.len().saturating_mul(3));
    for byte in secret.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn secret_representations(secret: &str) -> Vec<String> {
    if secret.is_empty() {
        return Vec::new();
    }
    let standard = standard_percent_encoded_secret(secret);
    let form = form_urlencoded_secret(secret);
    let fully_encoded = fully_percent_encoded_secret(secret);
    let mut variants = HashSet::new();
    variants.insert(secret.to_string());
    for encoded in [standard, form, fully_encoded] {
        variants.insert(encoded.clone());
        variants.insert(percent_hex_case(&encoded, false));
        variants.insert(percent_hex_case(&encoded, true));
    }
    let mut variants: Vec<_> = variants
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect();
    variants.sort_by_key(|value| std::cmp::Reverse(value.len()));
    variants
}

#[derive(Debug)]
struct DiagnosticSecretRedactor {
    literal_patterns: Vec<String>,
    percent_patterns: Vec<String>,
}

impl DiagnosticSecretRedactor {
    fn new<'a>(secrets: impl IntoIterator<Item = &'a str>) -> Self {
        let mut literal_patterns = HashSet::new();
        let mut percent_patterns = HashSet::new();
        for secret in secrets {
            for pattern in secret_representations(secret) {
                if pattern.contains('%') {
                    // Only the two hexadecimal nibbles following '%' are
                    // case-insensitive. Normalizing those exact positions
                    // covers mixed encodings without weakening the case of
                    // ordinary secret bytes.
                    percent_patterns.insert(normalize_percent_hex_nibbles(&pattern));
                } else {
                    literal_patterns.insert(pattern);
                }
            }
        }
        let mut literal_patterns: Vec<_> = literal_patterns.into_iter().collect();
        literal_patterns.sort_by_key(|value| std::cmp::Reverse(value.len()));
        let mut percent_patterns: Vec<_> = percent_patterns.into_iter().collect();
        percent_patterns.sort_by_key(|value| std::cmp::Reverse(value.len()));
        Self {
            literal_patterns,
            percent_patterns,
        }
    }

    #[cfg(test)]
    fn redact(&self, text: &str) -> String {
        self.redact_from(text, 0)
    }

    /// Redact matches against the retained overlap, but emit only the raw
    /// suffix beginning at `visible_start`. Redaction shrinkage must never pull
    /// earlier raw bytes into the operator-visible tail.
    fn redact_from(&self, text: &str, visible_start: usize) -> String {
        let percent_normalized = normalize_percent_hex_nibbles(text);
        let mut matches = Vec::new();
        for pattern in &self.literal_patterns {
            matches.extend(
                text.match_indices(pattern.as_str())
                    .map(|(start, value)| (start, start + value.len())),
            );
        }
        for pattern in &self.percent_patterns {
            matches.extend(
                percent_normalized
                    .match_indices(pattern.as_str())
                    .map(|(start, value)| (start, start + value.len())),
            );
        }
        if matches.is_empty() {
            return text[visible_start..].to_string();
        }
        matches.sort_unstable_by_key(|(start, end)| (*start, std::cmp::Reverse(*end)));
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(matches.len());
        for (start, end) in matches {
            if let Some((_, prior_end)) = merged.last_mut() {
                if start <= *prior_end {
                    *prior_end = (*prior_end).max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }
        let mut redacted = String::with_capacity(text.len().saturating_sub(visible_start));
        let mut cursor = visible_start;
        for (start, end) in merged {
            if end <= visible_start {
                continue;
            }
            if start < visible_start {
                redacted.push_str("<redacted>");
                cursor = end;
                continue;
            }
            redacted.push_str(&text[cursor..start]);
            redacted.push_str("<redacted>");
            cursor = end;
        }
        redacted.push_str(&text[cursor..]);
        redacted
    }
}

fn normalize_percent_hex_nibbles(value: &str) -> String {
    let mut normalized = value.as_bytes().to_vec();
    let mut index = 0usize;
    while index + 2 < normalized.len() {
        if normalized[index] == b'%'
            && normalized[index + 1].is_ascii_hexdigit()
            && normalized[index + 2].is_ascii_hexdigit()
        {
            normalized[index + 1] = normalized[index + 1].to_ascii_lowercase();
            normalized[index + 2] = normalized[index + 2].to_ascii_lowercase();
            index += 3;
        } else {
            index += 1;
        }
    }
    String::from_utf8(normalized).expect("percent normalization preserves UTF-8")
}

fn redacted_log_tail(
    scanner: &IncrementalLogScanner,
    redactor: &DiagnosticSecretRedactor,
) -> String {
    let tail = String::from_utf8_lossy(scanner.diagnostic_tail());
    let mut visible_start = tail.len().saturating_sub(LOG_DIAGNOSTIC_TAIL_CAP);
    while !tail.is_char_boundary(visible_start) {
        visible_start += 1;
    }
    let redacted = redactor.redact_from(&tail, visible_start);
    bounded_text(redacted, LOG_DIAGNOSTIC_TAIL_CAP)
}

fn authorization_header(token: &str) -> Result<reqwest::header::HeaderValue> {
    if token.is_empty() {
        anyhow::bail!("Jupyter authentication token must not be empty");
    }
    if token.len() > AUTH_SECRET_INPUT_CAP {
        anyhow::bail!(
            "Jupyter authentication token exceeds the {AUTH_SECRET_INPUT_CAP}-byte limit"
        );
    }
    reqwest::header::HeaderValue::from_str(&format!("token {token}"))
        .context("Jupyter token contains bytes that are invalid in an HTTP authorization header")
}

fn validate_auth_input(value: &str, description: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("{description} must not be empty");
    }
    if value.len() > AUTH_SECRET_INPUT_CAP {
        anyhow::bail!("{description} exceeds the {AUTH_SECRET_INPUT_CAP}-byte limit");
    }
    Ok(())
}

fn decode_bounded_lifecycle_json<T>(
    mut response: reqwest::blocking::Response,
    description: &str,
) -> std::result::Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    if response
        .content_length()
        .is_some_and(|length| length > LIFECYCLE_JSON_BODY_CAP as u64)
    {
        return Err(format!(
            "{description} response exceeds the {LIFECYCLE_JSON_BODY_CAP}-byte lifecycle body limit"
        ));
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(LIFECYCLE_JSON_BODY_CAP),
    );
    response
        .by_ref()
        .take(LIFECYCLE_JSON_BODY_CAP as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| format!("failed to read {description} response: {error}"))?;
    if body.len() > LIFECYCLE_JSON_BODY_CAP {
        return Err(format!(
            "{description} response exceeds the {LIFECYCLE_JSON_BODY_CAP}-byte lifecycle body limit"
        ));
    }
    serde_json::from_slice(&body).map_err(|error| {
        format!(
            "{description} returned malformed JSON: {}",
            bounded_text(error.to_string(), 512)
        )
    })
}

fn failed_start(error: impl Into<String>, pid: u32, log_path: String) -> JupyterServerResult {
    JupyterServerResult {
        success: false,
        handle_id: None,
        url: None,
        pid: Some(pid),
        port: None,
        token: None,
        log_path: Some(log_path),
        error: Some(error.into()),
    }
}

fn rollback_start_with_artifacts(
    guard: StartupGuard,
    artifacts: &InstanceArtifacts,
    cause: anyhow::Error,
) -> anyhow::Error {
    let process_result = guard.rollback();
    let artifact_result = artifacts.cleanup_all();
    match (process_result, artifact_result) {
        (Ok(()), Ok(())) => cause,
        (Err(process), Ok(())) => anyhow::anyhow!(
            "{cause:#}; process-tree rollback also failed and may require operator cleanup: {process}"
        ),
        (Ok(()), Err(artifacts)) => {
            anyhow::anyhow!("{cause:#}; private runtime cleanup also failed: {artifacts:#}")
        }
        (Err(process), Err(artifacts)) => anyhow::anyhow!(
            "{cause:#}; process-tree rollback failed: {process}; private runtime cleanup failed: {artifacts:#}"
        ),
    }
}

fn failed_start_with_artifact_rollback(
    guard: StartupGuard,
    artifacts: &InstanceArtifacts,
    error: impl Into<String>,
    pid: u32,
    log_path: String,
) -> JupyterServerResult {
    let cause = anyhow::anyhow!(error.into());
    failed_start(
        rollback_start_with_artifacts(guard, artifacts, cause).to_string(),
        pid,
        log_path,
    )
}

fn startup_endpoint_ready(
    client: &reqwest::blocking::Client,
    endpoint: &JupyterEndpoint,
) -> std::result::Result<(), String> {
    let base_url = url::Url::parse(&endpoint.url)
        .map_err(|error| format!("validated startup URL could not be reopened: {error}"))?;
    let kernels_url = api_url(&base_url, "api/kernels");
    match client
        .get(kernels_url)
        .header(
            "Authorization",
            authorization_header(&endpoint.token).map_err(|error| error.to_string())?,
        )
        .send()
    {
        Ok(response) if response.status().is_success() => {
            decode_bounded_lifecycle_json::<Vec<KernelInfo>>(response, "readiness endpoint")
                .map(|_| ())
        }
        Ok(response) => Err(format!(
            "readiness endpoint returned HTTP {}",
            response.status()
        )),
        Err(error) if error.is_timeout() => Err("readiness request timed out".to_string()),
        Err(error) => Err(format!("readiness connection failed: {error}")),
    }
}

/// Start a durable Jupyter server.
///
/// Parameters, the registry, secure log handles, command, and HTTP client are
/// validated before spawn. After spawn, `StartupGuard` owns the complete tree
/// until a typed-loopback announcement with the exact expected token passes an
/// authenticated readiness probe and one allocate+insert registry transaction
/// commits atomically.
pub fn start_server(params: JupyterServerParams) -> Result<JupyterServerResult> {
    let JupyterServerParams {
        working_dir,
        port,
        host,
        token,
        password_hash,
        password,
        open_browser,
        extra_args,
    } = params;

    let working_dir = fs::canonicalize(
        working_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".")),
    )
    .context("failed to resolve Jupyter working directory")?;
    if !fs::metadata(&working_dir)
        .context("failed to inspect Jupyter working directory")?
        .is_dir()
    {
        anyhow::bail!(
            "Jupyter working directory is not a directory: {}",
            working_dir.display()
        );
    }

    let requested_port = port.unwrap_or(8888);
    let host = host.unwrap_or_else(|| "127.0.0.1".to_string());
    if !is_loopback(&host) {
        anyhow::bail!("refusing Jupyter host '{host}': use a typed IPv4 or IPv6 loopback address");
    }
    let expected_host_ip: IpAddr = host
        .parse()
        .context("failed to parse typed Jupyter loopback host")?;
    let extra_args = extra_args.unwrap_or_default();
    let parsed_extra = parse_extra_args(&extra_args).context("invalid extra Jupyter arguments")?;
    let expected_default_url = parsed_extra
        .default_url
        .unwrap_or_else(|| CONTROLLED_DEFAULT_URL.to_string());
    for (value, description) in [
        (token.as_deref(), "Jupyter authentication token"),
        (password.as_deref(), "Jupyter plaintext password"),
        (password_hash.as_deref(), "Jupyter password hash"),
    ] {
        if let Some(value) = value {
            validate_auth_input(value, description)?;
        }
    }
    if token.is_none()
        && password.is_none()
        && password_hash.is_none()
        && !matches!(open_browser, Some(true))
    {
        anyhow::bail!(
            "headless Jupyter start requires --token, --password, or --password-hash; use --open-browser to let Gila generate private browser authentication"
        );
    }
    let token = token.unwrap_or_else(|| {
        use rand::Rng;
        rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect()
    });
    // Reject control characters/header-invalid bytes before any child exists.
    let _authorization = authorization_header(&token)?;

    let mut instance_id = [0u8; 16];
    {
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut instance_id);
    }
    let instance_base_path = instance_id_to_base_path(&instance_id);

    let password_hash = match (password_hash, password.as_deref()) {
        (Some(hash), _) => Some(hash),
        (None, Some(plaintext)) => Some(hash_password(plaintext)?),
        (None, None) => None,
    };
    let diagnostic_redactor = DiagnosticSecretRedactor::new(
        [
            Some(token.as_str()),
            password.as_deref(),
            password_hash.as_deref(),
        ]
        .into_iter()
        .flatten(),
    );

    // Validate the current on-disk schema/corruption state before creating a
    // process. This read-only snapshot releases its lock immediately.
    let store = RegistryStore::new().context("failed to initialize Jupyter registry")?;
    let _validated_snapshot = store
        .snapshot()
        .context("failed to validate Jupyter registry before start")?;
    let http_client = lifecycle_http_client(STARTUP_HTTP_TIMEOUT)?;
    let LogHandles {
        stdout_append,
        stderr_append,
        read_handle,
        log_path,
    } = store.create_start_log()?;

    let artifacts = create_instance_artifacts(
        &store,
        &instance_id,
        &token,
        &instance_base_path,
        password_hash.as_deref(),
    )
    .context("failed to create private Jupyter instance state")?;

    use crate::gila_pixi;
    let mut command = if gila_pixi::has_pixi_manifest(&working_dir) {
        let mut command = Command::new("pixi");
        command
            .arg("run")
            .arg("--executable")
            .arg("jupyter")
            .arg("notebook");
        command
    } else {
        let (launcher, launcher_args) = detect_and_wrap_jupyter_cmd(&working_dir);
        let mut command = Command::new(launcher);
        command.args(launcher_args).arg("notebook");
        command
    };

    command.args(&parsed_extra.forwarded);
    if !matches!(open_browser, Some(true)) {
        command.arg("--no-browser");
    }
    command
        // This final Gila-controlled option selects the only config file. The
        // public extra-arg parser rejects config/base/runtime/display aliases.
        .arg("--config")
        .arg(&artifacts.config_file)
        .arg("--NotebookApp.default_url")
        .arg(&expected_default_url)
        .arg("--ServerApp.default_url")
        .arg(&expected_default_url)
        .arg("--JupyterNotebookApp.default_url")
        .arg(&expected_default_url)
        .arg("--port")
        .arg(requested_port.to_string())
        .arg("--ip")
        .arg(&host)
        .current_dir(&working_dir)
        .env_clear();
    copy_safe_child_environment(&mut command);
    command
        .env("JUPYTER_TOKEN_FILE", &artifacts.token_file)
        .env("JUPYTER_RUNTIME_DIR", &artifacts.runtime_dir);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_append))
        .stderr(Stdio::from(stderr_append));

    let mut guard = match StartupGuard::spawn(command) {
        Ok(guard) => guard,
        Err(error) => {
            let cause = anyhow::Error::new(error)
                .context("failed to start Jupyter server; is Jupyter installed?");
            return match artifacts.cleanup_all() {
                Ok(()) => Err(cause),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{cause:#}; private instance cleanup also failed: {cleanup:#}"
                )),
            };
        }
    };
    let pid = guard.id();
    let mut scanner = IncrementalLogScanner::new(read_handle);
    let mut last_probe_error: Option<String> = None;
    let deadline = Instant::now() + STARTUP_TIMEOUT;

    let (endpoint, runtime_pid) = loop {
        match scanner.poll_lines() {
            Ok(_) => {}
            Err(error) => {
                return Ok(failed_start_with_artifact_rollback(
                    guard,
                    &artifacts,
                    format!("failed to scan durable Jupyter log: {error}"),
                    pid,
                    log_path,
                ));
            }
        }
        match discover_runtime_endpoint(
            &artifacts.runtime_dir,
            &token,
            &instance_base_path,
            expected_host_ip,
        ) {
            Ok(RuntimeDiscovery::Ready(candidate, runtime_pid)) => {
                match startup_endpoint_ready(&http_client, &candidate) {
                    Ok(()) => {
                        break (candidate, runtime_pid);
                    }
                    Err(error) => last_probe_error = Some(error),
                }
            }
            Ok(RuntimeDiscovery::Pending) => {}
            Err(error) => {
                let diagnostic = redacted_log_tail(&scanner, &diagnostic_redactor);
                return Ok(failed_start_with_artifact_rollback(
                    guard,
                    &artifacts,
                    format!(
                        "rejected Jupyter runtime metadata: {error:#}; durable log tail:\n{diagnostic}"
                    ),
                    pid,
                    log_path,
                ));
            }
        }

        match guard.try_wait() {
            Ok(Some(status)) => {
                let diagnostic = redacted_log_tail(&scanner, &diagnostic_redactor);
                let probe = last_probe_error
                    .as_deref()
                    .unwrap_or("no valid endpoint candidate was announced");
                return Ok(failed_start_with_artifact_rollback(
                    guard,
                    &artifacts,
                    format!(
                        "Jupyter exited during startup with status {status}; last readiness result: {probe}; durable log tail:\n{diagnostic}"
                    ),
                    pid,
                    log_path,
                ));
            }
            Err(error) => {
                return Ok(failed_start_with_artifact_rollback(
                    guard,
                    &artifacts,
                    format!("failed to inspect Jupyter child during startup: {error}"),
                    pid,
                    log_path,
                ));
            }
            Ok(None) => {}
        }

        if Instant::now() >= deadline {
            let diagnostic = redacted_log_tail(&scanner, &diagnostic_redactor);
            let probe = last_probe_error
                .as_deref()
                .unwrap_or("no complete runtime server file was discovered");
            return Ok(failed_start_with_artifact_rollback(
                guard,
                &artifacts,
                format!(
                    "Jupyter did not become ready within {} seconds ({probe}); durable log tail:\n{diagnostic}",
                    STARTUP_TIMEOUT.as_secs()
                ),
                pid,
                log_path,
            ));
        }
        thread::sleep(STARTUP_POLL_INTERVAL);
    };

    let registration = (|| -> Result<u64> {
        let registered_at_unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before the Unix epoch")?
                .as_millis(),
        )
        .context("registration timestamp exceeds u64 milliseconds")?;

        // Allocation and insertion deliberately share this single short
        // transaction after readiness.
        let mut transaction = store
            .begin_transaction()
            .context("failed to open registry transaction for ready Jupyter server")?;
        let validated = validate_stored_record(&ServerRecord {
            handle_id: 1,
            instance_id,
            url: endpoint.url.clone(),
            port: endpoint.port,
            token: token.clone(),
            pid: Some(runtime_pid),
            registered_at_unix_ms,
            log_path: Some(log_path.clone()),
            identity_bound: true,
            runtime_path: Some(artifacts.runtime_relative.clone()),
        })?;
        transaction.ensure_socket_available(validated.socket_addr)?;
        let handle_id = transaction.allocate_handle()?;
        transaction.insert(ServerRecord {
            handle_id,
            instance_id,
            url: endpoint.url.clone(),
            port: endpoint.port,
            token: token.clone(),
            pid: Some(runtime_pid),
            registered_at_unix_ms,
            log_path: Some(log_path.clone()),
            identity_bound: true,
            runtime_path: Some(artifacts.runtime_relative.clone()),
        })?;
        transaction
            .commit()
            .context("ready Jupyter server could not be persisted; startup was rolled back")?;
        Ok(handle_id)
    })();
    let handle_id = match registration {
        Ok(handle_id) => handle_id,
        Err(error) => {
            return Err(rollback_start_with_artifacts(guard, &artifacts, error));
        }
    };

    if let Err(secret_error) = artifacts.cleanup_secrets() {
        let rollback = guard.rollback();
        let runtime_cleanup = artifacts.cleanup_all();
        let registry_cleanup = (|| -> Result<()> {
            let mut transaction = store.begin_transaction()?;
            if transaction.compare_and_delete(handle_id, instance_id)? {
                transaction.commit()?;
            }
            Ok(())
        })();
        anyhow::bail!(
            "registered Jupyter server could not remove private startup secrets: {secret_error:#}; rollback={rollback:?}; runtime_cleanup={runtime_cleanup:?}; registry_cleanup={registry_cleanup:?}"
        );
    }

    // Dropping an unwrapped std child does not kill it. On Windows JobObject is
    // configured without kill-on-close; on Unix the process group remains.
    drop(guard.disarm());
    Ok(JupyterServerResult {
        success: true,
        handle_id: Some(handle_id),
        url: Some(endpoint.url),
        pid: Some(runtime_pid),
        port: Some(endpoint.port),
        token: None,
        log_path: Some(log_path),
        error: None,
    })
}

#[derive(Debug)]
enum ProbeOutcome {
    Running(Vec<KernelInfo>),
    Unreachable(String),
}

fn probe_registered_server(
    store: &RegistryStore,
    client: &reqwest::blocking::Client,
    record: &ServerRecord,
    timeout: Duration,
) -> ProbeOutcome {
    if !record.identity_bound {
        return ProbeOutcome::Unreachable(
            "legacy Jupyter record has no instance-bound identity; status was not probed and stored credentials were not sent"
                .to_string(),
        );
    }
    let endpoint = match validate_stored_record(record) {
        Ok(endpoint) => endpoint,
        Err(error) => return ProbeOutcome::Unreachable(format!("invalid registry entry: {error}")),
    };
    if let Err(error) = validate_registered_runtime(store, record, &endpoint) {
        return ProbeOutcome::Unreachable(format!(
            "registered runtime identity could not be verified: {error}"
        ));
    }
    let kernels_url = api_url(&endpoint.base_url, "api/kernels");
    let authorization = match authorization_header(&record.token) {
        Ok(value) => value,
        Err(error) => {
            return ProbeOutcome::Unreachable(format!("invalid registry entry: {error}"));
        }
    };
    let response = match client
        .get(kernels_url)
        .timeout(timeout)
        .header("Authorization", authorization)
        .send()
    {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return ProbeOutcome::Unreachable("kernels request timed out".to_string());
        }
        Err(error) => {
            return ProbeOutcome::Unreachable(format!("kernels connection failed: {error}"));
        }
    };
    if !response.status().is_success() {
        return ProbeOutcome::Unreachable(format!(
            "kernels endpoint returned HTTP {}",
            response.status()
        ));
    }
    match decode_bounded_lifecycle_json::<Vec<KernelInfo>>(response, "kernels endpoint") {
        Ok(kernels) => ProbeOutcome::Running(kernels),
        Err(error) => ProbeOutcome::Unreachable(error),
    }
}

fn status_for_record(
    store: &RegistryStore,
    client: &reqwest::blocking::Client,
    record: ServerRecord,
    timeout: Duration,
) -> JupyterServerStatus {
    match probe_registered_server(store, client, &record, timeout) {
        ProbeOutcome::Running(kernels) => JupyterServerStatus {
            running: true,
            state: JupyterServerState::Running,
            handle_id: record.handle_id,
            url: Some(record.url),
            port: Some(record.port),
            log_path: record.log_path,
            kernels,
            error: None,
        },
        ProbeOutcome::Unreachable(error) => JupyterServerStatus {
            running: false,
            state: JupyterServerState::Unreachable,
            handle_id: record.handle_id,
            url: Some(record.url),
            port: Some(record.port),
            log_path: record.log_path,
            kernels: Vec::new(),
            error: Some(bounded_text(error, 1_024)),
        },
    }
}

fn summary_from_status(status: JupyterServerStatus) -> ServerSummary {
    let JupyterServerStatus {
        running,
        state,
        handle_id,
        url,
        port,
        log_path,
        kernels: _,
        error,
    } = status;
    ServerSummary {
        handle_id,
        url: url.expect("record status always retains URL"),
        port: port.expect("record status always retains port"),
        running,
        state,
        log_path,
        error,
    }
}

fn deadline_summary(record: ServerRecord) -> ServerSummary {
    ServerSummary {
        handle_id: record.handle_id,
        url: record.url,
        port: record.port,
        running: false,
        state: JupyterServerState::Unreachable,
        log_path: record.log_path,
        error: Some("list aggregate deadline exceeded".to_string()),
    }
}

/// Snapshot one durable record under the registry lock, release the lock, and
/// then probe its authenticated kernels endpoint. Network/auth/JSON failures
/// are represented as `Unreachable`; they never delete the record.
pub fn get_server_status(handle_id: u64) -> Result<JupyterServerStatus> {
    let store = RegistryStore::new()?;
    let snapshot = store.snapshot()?;
    let Some(record) = snapshot.servers.get(&handle_id).cloned() else {
        return Ok(JupyterServerStatus {
            running: false,
            state: JupyterServerState::NotFound,
            handle_id,
            url: None,
            port: None,
            log_path: None,
            kernels: Vec::new(),
            error: None,
        });
    };
    let client = lifecycle_http_client(LIFECYCLE_HTTP_TIMEOUT)?;
    Ok(status_for_record(
        &store,
        &client,
        record,
        LIFECYCLE_HTTP_TIMEOUT,
    ))
}

/// List every durable registry entry. A failed probe is visible as
/// `Unreachable` and is never treated as permission to delete the entry.
pub fn list_servers() -> Result<JupyterListResult> {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    let store = RegistryStore::new()?;
    let records: Vec<_> = store.snapshot()?.servers.into_values().collect();
    if records.is_empty() {
        return Ok(JupyterListResult {
            servers: Vec::new(),
        });
    }
    let record_count = records.len();
    let deadline = Instant::now() + LIST_AGGREGATE_TIMEOUT;
    // Move the only per-probe credential copy into the queue. Workers convert
    // decoded kernel vectors into small summaries immediately, so at most the
    // active worker count can retain bounded lifecycle bodies.
    let work = Mutex::new(VecDeque::from_iter(records.into_iter().enumerate()));
    let results = Mutex::new(vec![None; record_count]);
    let worker_count = record_count.min(LIST_PROBE_CONCURRENCY);

    thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                let Ok(client) = lifecycle_http_client(LIST_AGGREGATE_TIMEOUT) else {
                    return;
                };
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return;
                    }
                    let item = work.lock().expect("list work mutex poisoned").pop_front();
                    let Some((index, record)) = item else {
                        return;
                    };
                    let status = status_for_record(&store, &client, record, remaining);
                    let summary = summary_from_status(status);
                    results.lock().expect("list result mutex poisoned")[index] = Some(summary);
                }
            });
        }
    });

    let mut statuses = results.into_inner().context("list result mutex poisoned")?;
    for (index, record) in work.into_inner().context("list work mutex poisoned")? {
        statuses[index] = Some(deadline_summary(record));
    }
    let servers = statuses
        .into_iter()
        .map(|status| status.expect("every list record is probed or deadline-summarized"))
        .collect();
    Ok(JupyterListResult { servers })
}

#[derive(Debug)]
enum ListenerState {
    Accepting,
    Refused,
    Ambiguous(String),
}

fn listener_state(socket_addr: SocketAddr) -> ListenerState {
    match TcpStream::connect_timeout(&socket_addr, LISTENER_CONNECT_TIMEOUT) {
        Ok(stream) => {
            drop(stream);
            ListenerState::Accepting
        }
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => ListenerState::Refused,
        Err(error) => ListenerState::Ambiguous(error.to_string()),
    }
}

fn cleanup_registered_runtime(store: &RegistryStore, record: &ServerRecord) -> Result<()> {
    if !record.identity_bound {
        return Ok(());
    }
    let instance_hex = instance_id_to_hex(&record.instance_id);
    let expected_relative = format!("jupyter/instances/{instance_hex}/runtime");
    if record.runtime_path.as_deref() != Some(expected_relative.as_str()) {
        anyhow::bail!("refusing to clean a runtime path not bound to the instance identity");
    }
    let instance_dir = store
        .root
        .join("jupyter")
        .join("instances")
        .join(instance_hex);
    remove_private_tree(&instance_dir, &record.instance_id, None)
}

/// Clean up a stopped instance and remove its registry record. A concurrent
/// stopper may win the cleanup race after this caller has already confirmed
/// listener refusal; once the exact record disappears, that is an idempotent
/// not-running result rather than an error from the losing filesystem cleanup.
fn cleanup_then_delete_registered_instance(
    store: &RegistryStore,
    record: &ServerRecord,
    shutdown_succeeded: bool,
) -> Result<bool> {
    match cleanup_registered_runtime(store, record) {
        Ok(()) => delete_registered_instance(
            store,
            record.handle_id,
            record.instance_id,
            shutdown_succeeded,
        ),
        Err(cleanup_error) => {
            let deadline = Instant::now() + DUPLICATE_STOP_RACE_TIMEOUT;
            loop {
                match store.snapshot()?.servers.get(&record.handle_id) {
                    None => return Ok(false),
                    Some(current) if current.instance_id != record.instance_id => {
                        return Err(cleanup_error).context(
                            "registered instance changed during concurrent cleanup; replacement preserved",
                        )
                    }
                    Some(_) if Instant::now() >= deadline => return Err(cleanup_error),
                    Some(_) => thread::sleep(STARTUP_POLL_INTERVAL),
                }
            }
        }
    }
}

fn delete_registered_instance(
    store: &RegistryStore,
    handle_id: u64,
    instance_id: [u8; 16],
    shutdown_succeeded: bool,
) -> Result<bool> {
    let partial_context = if shutdown_succeeded {
        format!("Jupyter shutdown succeeded, but registry cleanup for handle {handle_id} failed")
    } else {
        format!(
            "Jupyter was already stopped, but stale registry cleanup for handle {handle_id} failed"
        )
    };
    let mut transaction = store
        .begin_transaction()
        .with_context(|| partial_context.clone())?;
    match transaction.snapshot().servers.get(&handle_id) {
        None => return Ok(false),
        Some(record) if record.instance_id != instance_id => {
            anyhow::bail!(
                "Jupyter registry entry {handle_id} changed during shutdown; replacement was preserved"
            )
        }
        Some(_) => {}
    }
    if !transaction.compare_and_delete(handle_id, instance_id)? {
        anyhow::bail!("Jupyter registry entry changed during locked cleanup");
    }
    transaction.commit().with_context(|| partial_context)?;
    Ok(true)
}

fn confirm_listener_refused(socket_addr: SocketAddr) -> Result<()> {
    let deadline = Instant::now() + SHUTDOWN_CONFIRM_TIMEOUT;
    let mut last_ambiguous = None;
    loop {
        match listener_state(socket_addr) {
            ListenerState::Refused => return Ok(()),
            ListenerState::Accepting => {}
            ListenerState::Ambiguous(error) => last_ambiguous = Some(error),
        }
        if Instant::now() >= deadline {
            let diagnostic = last_ambiguous
                .map(|error| format!("; last listener error: {error}"))
                .unwrap_or_default();
            anyhow::bail!(
                "Jupyter shutdown response succeeded, but listener exit was not confirmed within {} seconds{diagnostic}",
                SHUTDOWN_CONFIRM_TIMEOUT.as_secs()
            );
        }
        thread::sleep(STARTUP_POLL_INTERVAL);
    }
}

fn listener_becomes_refused(socket_addr: SocketAddr, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    let mut last_ambiguous = None;
    loop {
        match listener_state(socket_addr) {
            ListenerState::Refused => return Ok(true),
            ListenerState::Accepting => {}
            ListenerState::Ambiguous(error) => last_ambiguous = Some(error),
        }
        if Instant::now() >= deadline {
            if let Some(error) = last_ambiguous {
                anyhow::bail!("listener state remained ambiguous: {error}");
            }
            return Ok(false);
        }
        thread::sleep(STARTUP_POLL_INTERVAL);
    }
}

/// Stop the exact durable server instance through its authenticated shutdown
/// API. Bare PIDs are never trusted or killed. Ambiguous network/auth/server
/// failures preserve the record; only definite listener refusal permits CAS
/// cleanup.
pub fn stop_server(handle_id: u64) -> Result<bool> {
    let store = RegistryStore::new()?;
    let snapshot = store.snapshot()?;
    let Some(record) = snapshot.servers.get(&handle_id).cloned() else {
        return Ok(false);
    };
    let endpoint = validate_stored_record(&record)?;

    match listener_state(endpoint.socket_addr) {
        ListenerState::Refused => {
            let _ = cleanup_then_delete_registered_instance(&store, &record, false).with_context(
                || {
                    format!(
                        "Jupyter handle {handle_id} is definitely stopped, but private runtime cleanup failed; registry entry preserved"
                    )
                },
            )?;
            return Ok(false);
        }
        ListenerState::Ambiguous(error) => {
            anyhow::bail!(
                "could not determine whether Jupyter handle {handle_id} is listening; registry entry preserved: {error}"
            );
        }
        ListenerState::Accepting => {}
    }

    if !record.identity_bound {
        anyhow::bail!(
            "Jupyter handle {handle_id} predates instance-bound shutdown; its accepting socket was preserved and no authenticated request was sent"
        );
    }
    validate_registered_runtime(&store, &record, &endpoint).with_context(|| {
        format!(
            "Jupyter handle {handle_id} runtime identity could not be verified; registry entry preserved and no authenticated request was sent"
        )
    })?;

    let client = lifecycle_http_client(LIFECYCLE_HTTP_TIMEOUT)?;
    match probe_registered_server(&store, &client, &record, LIFECYCLE_HTTP_TIMEOUT) {
        ProbeOutcome::Running(_) => {}
        ProbeOutcome::Unreachable(error) => {
            if listener_becomes_refused(endpoint.socket_addr, DUPLICATE_STOP_RACE_TIMEOUT)? {
                return cleanup_then_delete_registered_instance(&store, &record, false);
            }
            anyhow::bail!(
                "Jupyter handle {handle_id} failed its authenticated readiness probe; registry entry preserved and shutdown was not sent: {error}"
            )
        }
    }
    let shutdown_url = api_url(&endpoint.base_url, "api/shutdown");
    let response = match client
        .post(shutdown_url)
        .header("Authorization", authorization_header(&record.token)?)
        .send()
    {
        Ok(response) => response,
        Err(error) => {
            // A race with an independently stopped server is safe to clean only
            // after a fresh, definite refusal check.
            if matches!(listener_state(endpoint.socket_addr), ListenerState::Refused) {
                return cleanup_then_delete_registered_instance(&store, &record, false);
            }
            if error.is_timeout() {
                anyhow::bail!(
                    "Jupyter shutdown request timed out; registry entry {handle_id} preserved"
                );
            }
            anyhow::bail!(
                "Jupyter shutdown connection failed; registry entry {handle_id} preserved: {error}"
            );
        }
    };
    if !response.status().is_success() {
        anyhow::bail!(
            "Jupyter shutdown returned HTTP {}; registry entry {handle_id} preserved",
            response.status()
        );
    }

    confirm_listener_refused(endpoint.socket_addr)?;
    cleanup_then_delete_registered_instance(&store, &record, true).with_context(|| {
        format!(
            "Jupyter shutdown succeeded for handle {handle_id}, but private runtime cleanup failed; registry entry preserved"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_inputs_have_an_exact_pre_spawn_size_boundary() {
        let exact = "x".repeat(AUTH_SECRET_INPUT_CAP);
        let oversized = "x".repeat(AUTH_SECRET_INPUT_CAP + 1);
        for description in [
            "Jupyter authentication token",
            "Jupyter plaintext password",
            "Jupyter password hash",
        ] {
            validate_auth_input(&exact, description).expect("exact cap accepted");
            let error =
                validate_auth_input(&oversized, description).expect_err("cap plus one rejected");
            assert!(error.to_string().contains("exceeds"));
        }
    }

    #[test]
    fn list_summary_consumes_kernel_payload_and_preserves_record_fields() {
        let status = JupyterServerStatus {
            running: true,
            state: JupyterServerState::Running,
            handle_id: 17,
            url: Some("http://127.0.0.1:8123/__gila/example/".to_string()),
            port: Some(8123),
            log_path: Some("jupyter/logs/start-example.log".to_string()),
            kernels: vec![KernelInfo {
                id: "large-payload-is-not-retained-in-summary".repeat(256),
                name: "python3".to_string(),
                last_activity: "2026-08-24T00:00:00Z".to_string(),
                execution_state: "idle".to_string(),
                connections: 1,
            }],
            error: None,
        };

        let summary = summary_from_status(status);
        assert_eq!(summary.handle_id, 17);
        assert_eq!(summary.port, 8123);
        assert!(summary.running);
        assert_eq!(summary.state, JupyterServerState::Running);
        assert_eq!(
            summary.log_path.as_deref(),
            Some("jupyter/logs/start-example.log")
        );
    }

    #[test]
    fn test_jupyter_params_serialization() {
        let params = JupyterExecuteParams {
            notebook_path: "test.ipynb".to_string(),
            working_dir: Some("/tmp".to_string()),
            timeout_seconds: Some(60),
            save_outputs: Some(true),
            kernel_name: Some("python3".to_string()),
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("test.ipynb"));
        assert!(json.contains("/tmp"));
    }

    #[test]
    fn test_server_params_serialization() {
        let params = JupyterServerParams {
            working_dir: Some("/tmp".to_string()),
            port: Some(8888),
            host: Some("127.0.0.1".to_string()),
            token: Some("test-token".to_string()),
            password_hash: None,
            password: None,
            open_browser: Some(false),
            extra_args: None,
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("8888"));
        assert!(json.contains("test-token"));
    }

    /// Loopback boundary: only true loopback addresses are accepted.
    #[test]
    fn test_loopback_boundaries() {
        for ok in ["127.0.0.1", "127.42.7.9", "::1"] {
            assert!(is_loopback(ok), "expected '{ok}' to be loopback");
        }
        for bad in [
            "0.0.0.0",
            "::",
            "localhost",
            "example.com",
            "10.0.0.1",
            "192.168.1.1",
        ] {
            assert!(!is_loopback(bad), "expected '{bad}' to NOT be loopback");
        }
    }

    /// `start_server` must refuse a non-loopback host *before* spawning — so
    /// this test needs no jupyter install and leaves no process behind.
    #[test]
    fn test_start_server_rejects_non_loopback() {
        let err = start_server(JupyterServerParams {
            working_dir: None,
            port: None,
            host: Some("0.0.0.0".to_string()),
            token: None,
            password_hash: None,
            password: None,
            open_browser: None,
            extra_args: None,
        })
        .expect_err("should refuse non-loopback host with an error");
        let msg = err.to_string();
        assert!(
            msg.contains("loopback"),
            "error should explain the loopback requirement: {msg}"
        );
    }

    fn announced_url(host: &str, port: u16, path: &str, token: &str) -> String {
        let mut url =
            url::Url::parse(&format!("http://{host}:{port}{path}")).expect("test URL must parse");
        url.query_pairs_mut().append_pair("token", token);
        url.into()
    }

    #[test]
    fn endpoint_parser_accepts_typed_ipv4_and_ipv6_loopback() {
        let ipv4 = parse_endpoint_candidate(
            "http://127.9.8.7:8888/tree?token=expected",
            "expected",
            "/tree",
        )
        .expect("127/8 is loopback");
        assert_eq!(ipv4.host, "127.9.8.7");
        assert_eq!(ipv4.url, "http://127.9.8.7:8888");

        let ipv6 = parse_endpoint_candidate(
            "https://[::1]:9443/tree?token=expected",
            "expected",
            "/tree",
        )
        .expect("IPv6 loopback parses");
        assert_eq!(ipv6.host, "::1");
        assert_eq!(ipv6.url, "https://[::1]:9443");
    }

    #[test]
    fn endpoint_token_is_percent_decoded_and_compared_exactly() {
        let token = "punctuation +/%&=?#!";
        let announced = announced_url("127.0.0.1", 8888, "/tree", token);
        let endpoint =
            parse_endpoint_candidate(&announced, token, "/tree").expect("exact decoded token");
        assert_eq!(endpoint.token, token);
        assert!(parse_endpoint_candidate(&announced, "punctuation", "/tree").is_err());

        let duplicate = format!("{announced}&token={token}");
        assert!(parse_endpoint_candidate(&duplicate, token, "/tree").is_err());
    }

    #[test]
    fn endpoint_scanner_skips_spoofs_and_keeps_searching() {
        let output = concat!(
            "docs: https://example.com:443/lab?token=expected\n",
            "wrong: http://127.0.0.1:7777/lab?token=wrong\n",
            "empty: http://127.0.0.1:7777/lab?token=\n",
            "ready: http://127.0.0.1:7777/user/alice/tree?token=expected\n"
        );
        let endpoints = parse_jupyter_endpoints(output, "expected", "/tree");
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].url, "http://127.0.0.1:7777/user/alice");
    }

    #[test]
    fn base_path_is_preserved_for_api_urls() {
        let endpoint = parse_endpoint_candidate(
            "http://127.0.0.1:8888/user/alice/tree/?token=t",
            "t",
            "/tree",
        )
        .expect("base-path endpoint");
        assert_eq!(endpoint.url, "http://127.0.0.1:8888/user/alice");
        let base = url::Url::parse(&endpoint.url).unwrap();
        assert_eq!(
            api_url(&base, "api/kernels").as_str(),
            "http://127.0.0.1:8888/user/alice/api/kernels"
        );

        let non_ui = parse_endpoint_candidate(
            "http://127.0.0.1:8888/user/lab-notebook/tree?token=t",
            "t",
            "/tree",
        )
        .expect("non-UI base suffix");
        assert_eq!(non_ui.url, "http://127.0.0.1:8888/user/lab-notebook");

        let base_ending_in_lab =
            parse_endpoint_candidate("http://127.0.0.1:8888/user/lab/tree?token=t", "t", "/tree")
                .expect("controlled route after base ending in lab");
        assert_eq!(base_ending_in_lab.url, "http://127.0.0.1:8888/user/lab");

        let base_ending_in_tree =
            parse_endpoint_candidate("http://127.0.0.1:8888/user/tree/tree?token=t", "t", "/tree")
                .expect("controlled route after base ending in tree");
        assert_eq!(base_ending_in_tree.url, "http://127.0.0.1:8888/user/tree");

        let custom_route = parse_endpoint_candidate(
            "http://127.0.0.1:8888/user/tree/voila?token=t",
            "t",
            "/voila",
        )
        .expect("explicit custom default route");
        assert_eq!(custom_route.url, "http://127.0.0.1:8888/user/tree");

        assert_eq!(
            parse_extra_args(&["--ServerApp.default_url".to_string(), "/voila".to_string(),])
                .unwrap()
                .default_url
                .as_deref(),
            Some("/voila")
        );
        assert!(parse_extra_args(&[
            "--ServerApp.default_url=/voila".to_string(),
            "--NotebookApp.default_url=/tree".to_string(),
        ])
        .is_err());
    }

    fn validation_record(url: &str, port: u16, token: &str) -> ServerRecord {
        ServerRecord {
            handle_id: 1,
            instance_id: [7; 16],
            url: url.to_string(),
            port,
            token: token.to_string(),
            pid: Some(123),
            registered_at_unix_ms: 1,
            log_path: Some("jupyter/logs/start-test.log".to_string()),
            identity_bound: false,
            runtime_path: None,
        }
    }

    #[test]
    fn stored_record_validation_rejects_port_mismatch_and_untyped_host() {
        let mismatch = validation_record("http://127.0.0.1:8889/base", 8888, "token");
        assert!(validate_stored_record(&mismatch)
            .unwrap_err()
            .to_string()
            .contains("does not match"));

        let hostname = validation_record("http://localhost:8888/base", 8888, "token");
        assert!(validate_stored_record(&hostname)
            .unwrap_err()
            .to_string()
            .contains("typed loopback"));

        let empty_token = validation_record("http://127.0.0.1:8888/base", 8888, "");
        assert!(validate_stored_record(&empty_token).is_err());
    }

    #[test]
    fn start_result_serialization_never_exposes_token_compatibility_field() {
        let result = JupyterServerResult {
            success: true,
            handle_id: Some(1),
            url: Some("http://127.0.0.1:8888".to_string()),
            pid: Some(123),
            port: Some(8888),
            token: Some("must-not-serialize".to_string()),
            log_path: Some("jupyter/logs/start-test.log".to_string()),
            error: None,
        };
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(!encoded.contains("must-not-serialize"));
        assert!(!encoded.contains("token"));
    }

    /// `hash_password` must produce a `argon2:$argon2id$…` PHC string that
    /// Jupyter's `passwd_check` accepts, and it must verify round-trip.
    #[test]
    fn test_hash_password_produces_verifiable_argon2() {
        use argon2::{Argon2, PasswordHash, PasswordVerifier};
        let plaintext = "hunter2";
        let encoded = hash_password(plaintext).unwrap();
        assert!(
            encoded.starts_with("argon2:$argon2id$"),
            "expected argon2 PHC with jupyter prefix, got: {encoded}"
        );
        // Jupyter strips the `argon2:` prefix before verifying, so do the
        // same here to confirm the encoded suffix is a valid argon2 hash.
        let phc = &encoded["argon2:".len()..];
        let parsed = PasswordHash::new(phc).unwrap();
        Argon2::default()
            .verify_password(plaintext.as_bytes(), &parsed)
            .expect("hash must verify against the original plaintext");
    }

    /// P1 Fix: Unique base path derived from instance_id prevents stale socket reuse
    #[test]
    fn p1_unique_base_path_from_instance_id() {
        let instance_id1 = [1u8; 16];
        let instance_id2 = [2u8; 16];
        let path1 = instance_id_to_base_path(&instance_id1);
        let path2 = instance_id_to_base_path(&instance_id2);
        assert!(path1.starts_with("/__gila/"));
        assert_eq!(path1.len(), "/__gila//".len() + 32);
        assert_ne!(
            path1, path2,
            "different instance IDs must produce different paths"
        );
        assert!(
            path1.starts_with("/") && path1.ends_with("/"),
            "base path must be properly bounded"
        );
    }

    /// P1 Fix: Argument allowlist rejects bare `--` and dangerous flags
    #[test]
    fn p1_argument_allowlist_rejects_bare_double_dash() {
        let args = vec!["--no-browser".to_string(), "--".to_string()];
        let result = parse_extra_args(&args);
        assert!(result.is_err(), "bare `--` should be rejected");
        assert!(result.unwrap_err().to_string().contains("--"));
    }

    /// P1 Fix: Argument exact-match allowlist rejects dangerous flags
    #[test]
    fn p1_argument_allowlist_rejects_dangerous_flags() {
        let args = vec![
            "--no-browser".to_string(),
            "--token".to_string(),
            "secret-token".to_string(),
        ];
        let result = parse_extra_args(&args);
        assert!(
            result.is_err(),
            "dangerous flags like --token should be rejected by exact-match allowlist"
        );
        assert!(
            result.unwrap_err().to_string().contains("allowlist"),
            "error should mention allowlist rejection"
        );
    }

    #[test]
    fn extra_argument_surface_rejects_traitlets_aliases_abbreviations_and_positionals() {
        for rejected in [
            "--",
            "-h",
            "--help",
            "--help-all",
            "--version",
            "--show-config",
            "--show-config-json",
            "--generate-config",
            "notebook",
            "lab",
            "--ip=0.0.0.0",
            "--NotebookApp.ip=0.0.0.0",
            "--ServerApp.ip=0.0.0.0",
            "--ServerApp.i=0.0.0.0",
            "--NotebookApp.default_url=/lab",
            "--JupyterNotebookApp.default_url=/lab",
            "--ServerApp.base_url=/attacker",
            "--ServerApp.runtime_dir=/tmp/attacker",
            "--ServerApp.custom_display_url=http://attacker.invalid/",
            "--config=/tmp/attacker.py",
        ] {
            assert!(
                parse_extra_args(&[rejected.to_string()]).is_err(),
                "unexpectedly accepted {rejected}"
            );
        }
        let allowed = parse_extra_args(&["--ServerApp.default_url=/voila".to_string()])
            .expect("canonical default URL");
        assert_eq!(allowed.default_url.as_deref(), Some("/voila"));
        assert!(allowed.forwarded.is_empty());
    }

    /// P1 Fix: Hex conversion for instance_id
    #[test]
    fn p1_instance_id_hex_encoding() {
        let id = [
            0xabu8, 0xcd, 0xef, 0x12, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11,
        ];
        let hex = instance_id_to_hex(&id);
        assert_eq!(hex.len(), 32, "should be 32 hex chars for 16 bytes");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "hex must be lowercase"
        );
    }

    /// Validation must reject invalid registry records without deleting them.
    #[test]
    fn validation_preserves_malformed_records() {
        let malformed = validation_record("not-a-url", 8888, "token");
        let result = validate_stored_record(&malformed);
        assert!(
            result.is_err(),
            "malformed record should fail validation without deletion"
        );
    }
}
