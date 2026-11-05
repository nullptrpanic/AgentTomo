//! Domain collectors share only syscall snapshots and the observation schema.

pub mod file;
pub mod network;

pub fn watched_syscalls() -> Vec<i64> {
    file::SYSCALLS
        .iter()
        .map(|(number, _)| *number)
        .chain(network::SYSCALLS.iter().copied())
        .collect()
}
