// SPDX-License-Identifier: Apache-2.0

pub fn executable(path: &std::path::Path, mut source: impl std::io::Read) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "/bin/cat > \"$1\"", "fixture"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let copied = std::io::copy(&mut source, &mut input);
    drop(input);
    let status = child.wait().unwrap();
    copied.unwrap();
    assert!(status.success(), "fixture publication failed: {status}");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    unsafe extern "C" {
        fn fork() -> i32;
        fn close(fd: i32) -> i32;
        fn read(fd: i32, buffer: *mut std::ffi::c_void, count: usize) -> isize;
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
        fn _exit(status: i32) -> !;
    }

    struct ForkDuringRead {
        bytes: &'static [u8],
        path: std::path::PathBuf,
        control: UnixStream,
        release: Option<UnixStream>,
        child: Option<i32>,
    }

    impl Read for ForkDuringRead {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.child.is_none() {
                let descriptors: Vec<_> = std::fs::read_dir("/proc/self/fd")?
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        let fd: i32 = entry.file_name().to_str()?.parse().ok()?;
                        let keep =
                            std::fs::read_link(entry.path()).ok().as_ref() == Some(&self.path);
                        Some((fd, keep))
                    })
                    .collect();
                let control = self.control.as_raw_fd();
                let child = unsafe { fork() };
                assert!(child >= 0);
                if child == 0 {
                    for &(fd, keep) in &descriptors {
                        if fd > 2 && fd != control && !keep {
                            unsafe { close(fd) };
                        }
                    }
                    let mut byte = 0_u8;
                    unsafe {
                        read(control, std::ptr::from_mut(&mut byte).cast(), 1);
                        _exit(0);
                    }
                }
                self.child = Some(child);
            }
            self.bytes.read(buffer)
        }
    }

    impl Drop for ForkDuringRead {
        fn drop(&mut self) {
            drop(self.release.take());
            if let Some(child) = self.child {
                let mut status = 0;
                assert_eq!(unsafe { waitpid(child, &mut status, 0) }, child);
                assert_eq!(status, 0);
            }
        }
    }

    #[test]
    fn a_concurrent_fork_cannot_inherit_the_executable_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("koi");
        let (control, release) = UnixStream::pair().unwrap();
        let mut source = ForkDuringRead {
            bytes: b"#!/bin/sh\nprintf 'fixture ready\\n'\n",
            path: path.clone(),
            control,
            release: Some(release),
            child: None,
        };
        super::executable(&path, &mut source);
        let output = std::process::Command::new(&path).output();
        drop(source);
        let output = output.unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"fixture ready\n");
    }
}
