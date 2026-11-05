//! A syscall-local scratch address. Never rewrite a caller-owned sockaddr,
//! which may be read-only or shared by concurrently connecting threads.

use super::{Call, memory};
use nix::unistd::Pid;
use std::io;

pub struct Patch {
    address: u64,
    saved: Vec<u8>,
    pointer: u64,
    length: u64,
}

impl Patch {
    #[cfg(target_arch = "x86_64")]
    pub fn connect(call: &Call, sockaddr: &[u8]) -> io::Result<Self> {
        let mut registers = nix::sys::ptrace::getregs(call.tid)?;
        // Below the SysV x86_64 128-byte red zone, with room for sockaddr_in6.
        let address = registers
            .rsp
            .checked_sub(256)
            .ok_or_else(|| io::Error::other("invalid stack pointer"))?
            & !15;
        let saved = memory::read(call.tid, address, 32)?;
        let mut scratch = [0; 32];
        scratch[..sockaddr.len()].copy_from_slice(sockaddr);
        let patch = Self {
            address,
            saved,
            pointer: call.args[1],
            length: call.args[2],
        };
        if let Err(error) = memory::write(call.tid, address, &scratch) {
            let _ = memory::write(call.tid, address, &patch.saved);
            return Err(error);
        }
        registers.rsi = address;
        registers.rdx = sockaddr.len() as u64;
        if let Err(error) = nix::sys::ptrace::setregs(call.tid, registers) {
            let _ = memory::write(call.tid, address, &patch.saved);
            return Err(error.into());
        }
        Ok(patch)
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub fn connect(_: &Call, _: &[u8]) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TCP redirection requires the native x86_64 ABI",
        ))
    }

    pub fn restore(self, tid: Pid) -> io::Result<()> {
        memory::write(tid, self.address, &self.saved)?;
        #[cfg(target_arch = "x86_64")]
        {
            let mut registers = nix::sys::ptrace::getregs(tid)?;
            registers.rsi = self.pointer;
            registers.rdx = self.length;
            // Keep the kernel result and restart state exactly as returned.
            nix::sys::ptrace::setregs(tid, registers)?;
        }
        Ok(())
    }
}
