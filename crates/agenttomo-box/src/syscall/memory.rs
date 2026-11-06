//! Bounded snapshots of a stopped task. process_vm_readv is a fast path; ptrace
//! reads also work where the outer sandbox denies that optional syscall.

use nix::{
    sys::{
        ptrace,
        uio::{RemoteIoVec, process_vm_readv, process_vm_writev},
    },
    unistd::Pid,
};
use std::io::{self, IoSlice, IoSliceMut};

pub fn read(tid: Pid, address: u64, length: usize) -> io::Result<Vec<u8>> {
    address
        .checked_add(length as u64)
        .ok_or_else(|| io::Error::other("address overflow"))?;
    let mut bytes = vec![0; length];
    if process_vm_readv(
        tid,
        &mut [IoSliceMut::new(&mut bytes)],
        &[RemoteIoVec {
            base: address as usize,
            len: length,
        }],
    )
    .is_ok_and(|n| n == length)
    {
        return Ok(bytes);
    }
    let word_size = std::mem::size_of::<nix::libc::c_long>();
    let mut offset = 0;
    while offset < length {
        let current = address as usize + offset;
        let aligned = current & !(word_size - 1);
        let word = ptrace::read(tid, aligned as ptrace::AddressType)?.to_ne_bytes();
        let start = current - aligned;
        let count = (word_size - start).min(length - offset);
        bytes[offset..offset + count].copy_from_slice(&word[start..start + count]);
        offset += count;
    }
    Ok(bytes)
}

pub fn c_string(tid: Pid, address: u64, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    while bytes.len() < limit {
        let current = address
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("address overflow"))?;
        // Never read past a page merely to find a NUL near its end.
        let count = (limit - bytes.len())
            .min(256)
            .min(4096 - current as usize % 4096);
        let chunk = read(tid, current, count)?;
        if let Some(end) = chunk.iter().position(|byte| *byte == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return Ok((bytes, false));
        }
        bytes.extend(chunk);
    }
    Ok((bytes, true))
}

pub fn write(tid: Pid, address: u64, bytes: &[u8]) -> io::Result<()> {
    address
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| io::Error::other("address overflow"))?;
    if process_vm_writev(
        tid,
        &[IoSlice::new(bytes)],
        &[RemoteIoVec {
            base: address as usize,
            len: bytes.len(),
        }],
    )
    .is_ok_and(|n| n == bytes.len())
    {
        return Ok(());
    }
    let word_size = std::mem::size_of::<nix::libc::c_long>();
    let mut offset = 0;
    while offset < bytes.len() {
        let current = address as usize + offset;
        let aligned = current & !(word_size - 1);
        let mut word = ptrace::read(tid, aligned as ptrace::AddressType)?.to_ne_bytes();
        let start = current - aligned;
        let count = (word_size - start).min(bytes.len() - offset);
        word[start..start + count].copy_from_slice(&bytes[offset..offset + count]);
        ptrace::write(
            tid,
            aligned as ptrace::AddressType,
            nix::libc::c_long::from_ne_bytes(word),
        )?;
        offset += count;
    }
    Ok(())
}
