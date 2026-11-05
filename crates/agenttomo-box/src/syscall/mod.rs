//! Linux syscall-stop mechanism. No file, network or proxy policy belongs here.

mod filter;
pub mod memory;
pub mod redirect;

use nix::{errno::Errno, libc, unistd::Pid};
use std::io;

pub use filter::install;
pub const SUPPORTED: bool = cfg!(target_arch = "x86_64");
pub const NATIVE_ARCH: u32 = 0xc000003e;

// Linux UAPI ptrace_syscall_info. The oversized union buffer allows the kernel
// to supply entry, exit and seccomp variants without architecture-specific regs.
#[repr(C)]
#[derive(Default)]
pub struct Stop {
    pub op: u8,
    padding: [u8; 3],
    pub arch: u32,
    pub instruction_pointer: u64,
    pub stack_pointer: u64,
    data: [u64; 8],
}

impl Stop {
    pub fn read(tid: Pid) -> io::Result<Self> {
        let mut stop = Self::default();
        let count = unsafe {
            libc::ptrace(
                0x420e_u32,
                tid.as_raw(),
                std::mem::size_of::<Self>(),
                &mut stop as *mut Self,
            )
        };
        Errno::result(count)?;
        if count < 24 {
            return Err(io::Error::other("short PTRACE_GET_SYSCALL_INFO response"));
        }
        Ok(stop)
    }

    pub fn result(&self) -> i64 {
        self.data[0] as i64
    }
}

pub struct Call {
    pub tid: Pid,
    pub task_id: u64,
    pub number: i64,
    pub args: [u64; 6],
}

impl Call {
    pub fn read(tid: Pid, task_id: u64) -> io::Result<Self> {
        let stop = Stop::read(tid)?;
        if stop.op != 3 || stop.arch != NATIVE_ARCH {
            return Err(io::Error::other("unsupported syscall stop/ABI"));
        }
        Ok(Self {
            tid,
            task_id,
            number: stop.data[0] as i64,
            args: stop.data[1..7].try_into().unwrap(),
        })
    }
}

pub fn result_fields(data: &mut serde_json::Value, result: i64) {
    data["result"] = result.into();
    let restarted = matches!(result, -512 | -513 | -514 | -516);
    data["restart"] = restarted.into();
    data["errno"] = if (-4095..0).contains(&result) && !restarted {
        (-result).into()
    } else {
        serde_json::Value::Null
    };
}
