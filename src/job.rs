//! Run a child as the terminal's foreground job, the way a shell does.
//!
//! A plain `Command::status()` leaves the editor in recto's process group, so
//! the terminal's foreground group is still led by recto. Anything that asks
//! "what's in the foreground of this terminal" then sees recto rather than the
//! editor. Rex's ctrl+hjkl navigation is one such asker: it only hands the key
//! to nvim when nvim leads the foreground group, so nvim's own splits were
//! skipped and Rex moved between panes instead. Giving the editor its own
//! process group and handing it the terminal makes the answer honest.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Run `cmd` to completion as the terminal's foreground job, then take the
/// terminal back. When stdin isn't a terminal there's no job to hand over, so
/// the command just runs in place.
pub fn run_foreground(cmd: &mut Command) -> io::Result<()> {
    let tty = libc::STDIN_FILENO;
    // SAFETY: tcgetpgrp only reads the terminal's foreground group.
    if unsafe { libc::tcgetpgrp(tty) } < 0 {
        return cmd.status().map(drop);
    }

    // A process outside the foreground group that calls tcsetpgrp gets
    // SIGTTOU. Recto has to call it from the background to reclaim the
    // terminal, and the child calls it before it owns the terminal, so ignore
    // SIGTTOU for the duration. The child restores the default before exec,
    // since an ignored disposition would otherwise survive into the editor.
    // SAFETY: swapping a signal disposition; the old one is restored below.
    let old_ttou = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };

    // SAFETY: the hook runs between fork and exec, and only calls
    // async-signal-safe functions. Both sides of the fork set the group and
    // hand over the terminal, so it doesn't matter which runs first: the
    // editor never reads the terminal before it owns it.
    unsafe {
        cmd.pre_exec(move || {
            libc::setpgid(0, 0);
            libc::tcsetpgrp(tty, libc::getpid());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    let result = cmd.spawn().map(|child| {
        let pid = child.id() as libc::pid_t;
        // SAFETY: plain syscalls on the child we just spawned.
        unsafe {
            libc::setpgid(pid, pid);
            libc::tcsetpgrp(tty, pid);
        }
        wait(pid);
    });

    // SAFETY: reclaim the terminal for recto's own group and put SIGTTOU back.
    unsafe {
        libc::tcsetpgrp(tty, libc::getpgrp());
        libc::signal(libc::SIGTTOU, old_ttou);
    }
    result
}

/// Wait for `pid` to exit. A ctrl+z in the editor stops its group, and recto
/// has no shell above it to resume anything, so a stopped editor would leave
/// the terminal frozen. Hand the terminal straight back and continue it, which
/// makes suspend a no-op.
fn wait(pid: libc::pid_t) {
    loop {
        let mut status = 0;
        // SAFETY: waits on our own child, reaping it once it exits.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if !libc::WIFSTOPPED(status) {
            return;
        }
        // SAFETY: resume the child's group and give it back the terminal.
        unsafe {
            libc::tcsetpgrp(libc::STDIN_FILENO, pid);
            libc::kill(-pid, libc::SIGCONT);
        }
    }
}
