use nix::{errno::Errno, libc};
use std::io;

/// A straight equality filter, not a general BPF compiler. Unselected syscalls
/// retain the outer sandbox's behavior; this cannot relax an existing filter.
pub fn install(numbers: &[i64]) -> io::Result<()> {
    if !super::SUPPORTED {
        return Ok(());
    }
    let statement = |code: u32, k| libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |k, jt, jf| libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k,
    };
    let ret = |value| statement(libc::BPF_RET | libc::BPF_K, value);
    let mut filter = vec![
        statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 4),
        jump(super::NATIVE_ARCH, 1, 0),
        ret(libc::SECCOMP_RET_ALLOW),
        statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0),
    ];
    for number in numbers {
        filter.push(jump(*number as u32, 0, 1));
        filter.push(ret(libc::SECCOMP_RET_TRACE));
    }
    filter.push(ret(libc::SECCOMP_RET_ALLOW));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    nix::sys::prctl::set_no_new_privs()?;
    let result = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) };
    Errno::result(result)?;
    Ok(())
}
