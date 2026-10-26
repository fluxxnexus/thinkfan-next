//! Bounded subprocess helpers shared by CLI and native ABI integration tests.
use std::{
    io::{Read, Seek, SeekFrom},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[track_caller]
pub fn wait_for(description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if condition() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out: {description}");
        thread::sleep(Duration::from_millis(10));
    }
}

pub struct Process(pub Child);
impl Process {
    pub fn signal(&self, name: &str) {
        let signal = match name {
            "TERM" => libc::SIGTERM,
            "INT" => libc::SIGINT,
            "HUP" => libc::SIGHUP,
            "USR1" => libc::SIGUSR1,
            "USR2" => libc::SIGUSR2,
            "PWR" => libc::SIGPWR,
            _ => panic!("unsupported test signal: {name}"),
        };
        // SAFETY: signal only the live child owned by this test.
        assert_eq!(
            unsafe { libc::kill(self.0.id() as i32, signal) },
            0,
            "sending {name}: {}",
            std::io::Error::last_os_error()
        );
    }
    pub fn wait(&mut self) -> ExitStatus {
        let mut status = None;
        wait_for(&format!("child {} to exit", self.0.id()), || {
            status = self.0.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap()
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub trait CommandOutput {
    fn output_timeout(&mut self) -> Output;
}
impl CommandOutput for Command {
    fn output_timeout(&mut self) -> Output {
        // Files avoid pipe-buffer deadlocks if a regression produces lots of logs.
        let mut stdout = tempfile::tempfile().unwrap();
        let mut stderr = tempfile::tempfile().unwrap();
        let mut child = Process(
            self.stdin(Stdio::null())
                .stdout(stdout.try_clone().unwrap())
                .stderr(stderr.try_clone().unwrap())
                .spawn()
                .unwrap(),
        );
        let status = child.wait();
        let mut output = Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        stdout.seek(SeekFrom::Start(0)).unwrap();
        stderr.seek(SeekFrom::Start(0)).unwrap();
        stdout.read_to_end(&mut output.stdout).unwrap();
        stderr.read_to_end(&mut output.stderr).unwrap();
        output
    }
}
