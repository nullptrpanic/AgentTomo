//! A bounded getsockname query after a completed native syscall. No connection
//! argument is edited. The original result, registers and scratch are restored.
#[cfg(target_arch = "x86_64")]
use super::memory;
use super::{Call, Stop};
#[cfg(target_arch = "x86_64")]
use nix::libc;
#[cfg(target_arch = "x86_64")]
use nix::sys::ptrace;
use nix::unistd::Pid;
use std::{io, net::SocketAddr};

pub struct Query {
    #[cfg(target_arch = "x86_64")]
    registers: libc::user_regs_struct,
    #[cfg(target_arch = "x86_64")]
    address: u64,
    #[cfg(target_arch = "x86_64")]
    saved: Vec<u8>,
    #[cfg(target_arch = "x86_64")]
    signal_mask: u64,
}
impl Query {
    #[cfg(target_arch = "x86_64")]
    pub fn start(call: &Call) -> io::Result<Option<Self>> {
        let registers = ptrace::getregs(call.tid)?;
        let instruction = registers
            .rip
            .checked_sub(2)
            .ok_or_else(|| io::Error::other("invalid syscall instruction"))?;
        if memory::read(call.tid, instruction, 2).ok().as_deref() != Some(&[0x0f, 0x05]) {
            return Ok(None);
        }
        let address = registers
            .rsp
            .checked_sub(512)
            .ok_or_else(|| io::Error::other("invalid scratch pointer"))?
            & !15;
        let Ok(saved) = memory::read(call.tid, address, 144) else {
            return Ok(None);
        };
        let Ok(signal_mask) = signal_mask(call.tid, None) else {
            return Ok(None);
        };
        let query = Self {
            registers,
            address,
            saved,
            signal_mask,
        };
        // Let the kernel retain pending signals and their siginfo until the
        // target's registers and scratch are restored. Requeueing signals in
        // userspace would lose standard-signal coalescing and ordering semantics.
        if self::signal_mask(call.tid, Some(u64::MAX)).is_err() {
            return Ok(None);
        }
        let mut bytes = [0u8; 144];
        bytes[128..132].copy_from_slice(&128u32.to_ne_bytes());
        if memory::write(call.tid, address, &bytes).is_err() {
            // A partial process_vm_writev must never leave modified scratch.
            query.restore(call.tid)?;
            return Ok(None);
        }
        let mut injected = registers;
        injected.rip = instruction;
        injected.rax = libc::SYS_getsockname as u64;
        injected.orig_rax = u64::MAX;
        injected.rdi = call.args[0];
        injected.rsi = address;
        injected.rdx = address + 128;
        if ptrace::setregs(call.tid, injected).is_err() {
            query.restore(call.tid)?;
            return Ok(None);
        }
        Ok(Some(query))
    }
    #[cfg(target_arch = "x86_64")]
    pub fn finish(&self, tid: Pid, stop: &Stop) -> io::Result<Option<SocketAddr>> {
        let address = if stop.result() == 0 {
            memory::read(tid, self.address, 128)
                .ok()
                .and_then(|bytes| crate::network::address::decode(&bytes))
        } else {
            None
        };
        self.restore(tid)?;
        Ok(address)
    }

    #[cfg(target_arch = "x86_64")]
    fn restore(&self, tid: Pid) -> io::Result<()> {
        memory::write(tid, self.address, &self.saved)?;
        ptrace::setregs(tid, self.registers)?;
        signal_mask(tid, Some(self.signal_mask))?;
        Ok(())
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub fn start(_: &Call) -> io::Result<Option<Self>> {
        Ok(None)
    }
    #[cfg(not(target_arch = "x86_64"))]
    pub fn finish(&self, _: Pid, _: &Stop) -> io::Result<Option<SocketAddr>> {
        Ok(None)
    }
}

#[cfg(target_arch = "x86_64")]
fn signal_mask(tid: Pid, value: Option<u64>) -> io::Result<u64> {
    let request = if value.is_some() {
        libc::PTRACE_SETSIGMASK
    } else {
        libc::PTRACE_GETSIGMASK
    };
    let mut mask = value.unwrap_or_default();
    // The native Linux kernel sigset is 64 bits, unlike libc's larger sigset_t.
    // The tracee is stopped and the buffer remains valid throughout ptrace.
    nix::errno::Errno::result(unsafe {
        libc::ptrace(request, tid.as_raw(), size_of::<u64>(), &mut mask)
    })?;
    Ok(mask)
}
