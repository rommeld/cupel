use std::io;
use std::mem;
use std::ptr;

use libc::{c_int, pid_t};

const PROC_PIDUNIQIDENTIFIERINFO: c_int = 17;

#[repr(C)]
#[derive(Default)]
struct UniqueIdentifierInfo {
    executable_uuid: [u8; 16],
    unique_id: u64,
    parent_unique_id: u64,
    pid_version: i32,
    original_parent_pid_version: i32,
    reserved: [u64; 2],
}

const _: () = assert!(mem::size_of::<UniqueIdentifierInfo>() == 56);

#[repr(C)]
struct AuditToken {
    identity_prefix: [u32; 5],
    pid: u32,
    audit_session: u32,
    pid_version: u32,
}

type SignalWithAuditToken = unsafe extern "C" fn(*mut AuditToken, c_int) -> c_int;

/// A macOS PID and its kernel-assigned version, used to reject a recycled PID.
pub struct ProcessIdentity {
    pid: pid_t,
    version: i32,
}

impl ProcessIdentity {
    /// Read the current process version for a positive PID.
    pub fn read(pid: pid_t) -> io::Result<Self> {
        if pid <= 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid PID"));
        }

        let mut info = UniqueIdentifierInfo::default();
        let size = mem::size_of::<UniqueIdentifierInfo>() as c_int;
        // SAFETY: This flavor writes the 56-byte XNU proc_uniqidentifierinfo layout.
        // The initialized C-layout buffer is writable for exactly that size.
        let copied = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDUNIQIDENTIFIERINFO,
                0,
                ptr::from_mut(&mut info).cast(),
                size,
            )
        };
        match copied {
            count if count == size => Ok(Self {
                pid,
                version: info.pid_version,
            }),
            count if count <= 0 => Err(io::Error::last_os_error()),
            _ => Err(io::Error::other("unexpected macOS process identity size")),
        }
    }

    /// Send SIGKILL only if the kernel still recognizes this process identity.
    pub fn kill(&self) -> io::Result<()> {
        // SAFETY: RTLD_DEFAULT is a valid lookup scope and the symbol name is NUL-terminated.
        let symbol =
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"proc_signal_with_audittoken".as_ptr()) };
        if symbol.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "macOS process identity signaling is unavailable",
            ));
        }
        // SAFETY: libSystem exports this symbol with the signature in the SDK's libproc.h.
        let signal: SignalWithAuditToken = unsafe { mem::transmute(symbol) };
        let mut token = AuditToken {
            identity_prefix: [0; 5],
            pid: self.pid as u32,
            audit_session: 0,
            pid_version: self.version as u32,
        };
        // SAFETY: The writable token has the audit_token_t layout. XNU checks its PID and
        // version against the process table and checks signal permissions on the matching process.
        if unsafe { signal(&mut token, libc::SIGKILL) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::Command;

    use super::ProcessIdentity;

    #[test]
    fn rejects_a_stale_version_without_signaling_the_current_process() {
        let mut child = Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let identity = ProcessIdentity::read(i32::try_from(child.id()).unwrap()).unwrap();
        let stale = ProcessIdentity {
            version: identity.version.wrapping_add(1),
            ..identity
        };
        assert_eq!(stale.kill().unwrap_err().raw_os_error(), Some(libc::ESRCH));
        assert!(
            child.try_wait().unwrap().is_none(),
            "stale identity killed the child"
        );
        identity.kill().unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }
}
