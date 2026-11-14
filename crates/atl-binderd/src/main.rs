// SPDX-License-Identifier: MIT
//! atl-binderd: binderd as its own process, for tests until atld hosts it. Its stdin is the admin connection (a SOCK_SEQPACKET socket): the harness registers each process there (`server::encode_register` + the process's control connection as fd).
//! Exits when stdin closes, printing on stderr one `binderd: <counter> <n>` line per nonzero counter (core rejections, ignored inputs, protocol faults), then `binderd: exit (<n> nonzero counters)`.
#![forbid(unsafe_code)]

use std::io::Write;
use std::os::fd::AsFd;
use std::process::ExitCode;

use atl_binderd::server::Server;

/// The line an orderly exit always ends with (xtask test app reads it from the run's binderd.log).
const EXIT_LINE: &str = "binderd: exit";

fn main() -> ExitCode {
    let admin = match rustix::io::fcntl_dupfd_cloexec(std::io::stdin().as_fd(), 3) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "binderd: stdin: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut server = Server::new(admin);
    let result = server.run();
    let mut err = std::io::stderr().lock();
    for (k, n) in server.core().rejects() {
        let _ = writeln!(err, "binderd: reject {k:?} {n}");
    }
    for (k, n) in server.core().ignored() {
        let _ = writeln!(err, "binderd: ignored {k:?} {n}");
    }
    let mut lines = 0usize;
    for (k, n) in server.faults() {
        // hex: an ioctl code reads as its _IOC fields
        let _ = writeln!(err, "binderd: fault {k:x?} {n}");
        lines += 1;
    }
    lines += server.core().rejects().len() + server.core().ignored().len();
    // always written: an orderly exit with every counter zero still says it ran to its end (a SIGKILL leaves no line)
    let _ = writeln!(err, "{EXIT_LINE} ({lines} nonzero counters)");
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "binderd: poll: {e}");
            ExitCode::FAILURE
        }
    }
}
