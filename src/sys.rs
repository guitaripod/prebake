#![allow(unsafe_code)]

use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

const IOPRIO_WHO_PROCESS: libc::c_int = 1;
const IOPRIO_CLASS_IDLE: libc::c_int = 3;
const IOPRIO_CLASS_SHIFT: libc::c_int = 13;

/// Starts the command in its own process group at the priority Steam uses for shader
/// replays (nice 19, `SCHED_BATCH`, idle I/O class), lets it inherit `keep_fd`, and has the
/// kernel kill it if prebake dies.
pub fn background_child(command: &mut Command, keep_fd: RawFd) {
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
            let param = libc::sched_param { sched_priority: 0 };
            libc::sched_setscheduler(0, libc::SCHED_BATCH, &param);
            libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT);
            if libc::fcntl(keep_fd, libc::F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Sends SIGKILL to every process in a process group.
pub fn kill_group(pgid: i32) {
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}
