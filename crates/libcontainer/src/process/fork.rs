use std::{ffi::c_int, num::NonZeroUsize};

use libc::SIGCHLD;
use nix::{
    sys::{mman, resource},
    unistd::Pid,
};
use prctl;

#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    #[error("failed to clone process")]
    Clone(#[source] nix::Error),
    #[error("failed to get system memory page size")]
    PageSize(#[source] nix::Error),
    #[error("failed to get resource limit")]
    ResourceLimit(#[source] nix::Error),
    #[error("the stack size is zero")]
    ZeroStackSize,
    #[error("failed to allocate stack")]
    StackAllocation(#[source] nix::Error),
    #[error("failed to create stack guard page")]
    GuardPage(#[source] nix::Error),
    #[error("unkown error code {0}")]
    UnknownErrno(i32),
}

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    #[error(transparent)]
    IntermediateProcess(
        #[from] crate::process::container_intermediate_process::IntermediateProcessError,
    ),
    #[error(transparent)]
    InitProcess(#[from] crate::process::container_init_process::InitProcessError),
    // Need a fake error for testing
    #[cfg(test)]
    #[error("unknown")]
    Test,
}

type CallbackResult<T> = std::result::Result<T, CallbackError>;

// The clone callback is used in clone call. It is a boxed closure and it needs
// to trasfer the ownership of related memory to the new process.
type CloneCb = Box<dyn FnOnce() -> i32>;

// Fork/Clone a sibling process that shares the same parent as the calling
// process. This is used to launch the container init process so the parent
// process of the calling process can receive ownership of the process. If we
// clone a child process as the init process, the calling process (likely the
// youki main process) will exit and the init process will be re-parented to the
// process 1 (system init process), which is not the right behavior of what we
// look for.
pub fn container_clone_sibling<'a, F: FnMut() -> CallbackResult<()> + 'a>(
    child_name: &str,
    cb: F,
) -> Result<Pid, CloneError> {
    // Note: normally, an exit signal is required, but when using
    // `CLONE_PARENT`, the `clone3` will return EINVAL if an exit signal is set.
    // The older `clone` will not return EINVAL in this case. Instead it ignores
    // the exit signal bits in the glibc wrapper. Therefore, we explicitly set
    // the exit_signal to None here, so this works for both version of clone.
    clone_internal(child_name, cb, libc::CLONE_PARENT as u64, None)
}

// Execute the cb in another process. Make the fork works more like thread_spawn
// or clone, so it is easier to reason. Compared to clone call, fork is easier
// to use since fork will magically take care of all the variable copying. If
// using clone, we would have to manually make sure all the variables are
// correctly send to the new process, especially Rust borrow checker will be a
// lot of hassel to deal with every details.
pub fn container_clone<'a, F: FnMut() -> CallbackResult<()> + 'a>(
    child_name: &str,
    cb: F,
) -> Result<Pid, CloneError> {
    clone_internal(child_name, cb, 0, Some(SIGCHLD as u64))
}

fn clone_internal<'a, F: FnMut() -> CallbackResult<()> + 'a>(
    child_name: &str,
    mut cb: F,
    flags: u64,
    exit_signal: Option<u64>,
) -> Result<Pid, CloneError> {
    let mut child_closure = || {
        if let Err(ret) = prctl::set_name(child_name) {
            tracing::error!(?ret, "failed to set name for child process");
            return ret;
        }
        match cb() {
            Ok(_) => 0,
            Err(err) => {
                tracing::error!(?err, "failed to run callback in clone");
                -1
            }
        }
    };

    match clone3(&mut child_closure, flags, exit_signal) {
        Ok(pid) => return Ok(pid),
        Err(CloneError::Clone(nix::Error::ENOSYS)) => {
            tracing::debug!("clone3 is not supported, fallback to clone");
            let pid = clone(&mut child_closure, flags, exit_signal)?;

            Ok(pid)
        }
        Err(err) => return Err(err),
    }
}

fn clone3<'a, F: FnMut() -> i32 + 'a>(
    mut cb: F,
    flags: u64,
    exit_signal: Option<u64>,
) -> Result<Pid, CloneError> {
    #[repr(C)]
    struct clone3_args {
        flags: u64,
        pidfd: u64,
        child_tid: u64,
        parent_tid: u64,
        exit_signal: u64,
        stack: u64,
        stack_size: u64,
        tls: u64,
        set_tid: u64,
        set_tid_size: u64,
        cgroup: u64,
    }
    let mut args = clone3_args {
        flags,
        pidfd: 0,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: exit_signal.unwrap_or(0),
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: 0,
    };
    let args_ptr = &mut args as *mut clone3_args;
    let args_size = std::mem::size_of::<clone3_args>();
    // For now, we can only use clone3 as a kernel syscall. Libc wrapper is not
    // available yet.
    match unsafe { libc::syscall(libc::SYS_clone3, args_ptr, args_size) } {
        -1 => {
            return Err(CloneError::Clone(nix::Error::last()));
        }
        0 => {
            // Inside the cloned process, continue in the cloned process, we
            // execute the callback and exit with the return code.
            let ret = cb();
            std::process::exit(ret);
        }
        ret if ret >= 0 => return Ok(Pid::from_raw(ret as i32)),
        ret => return Err(CloneError::UnknownErrno(ret as i32)),
    };
}

/// clone uses syscall clone(2) to create a new process for the container init
/// process. Using clone syscall gives us better control over how to can create
/// the new container process, where we can enter into namespaces directly instead
/// of using unshare and fork. This call will only create one new process, instead
/// of two using fork.
fn clone<'a, F: FnMut() -> i32 + 'a>(
    cb: F,
    flags: u64,
    exit_signal: Option<u64>,
) -> Result<Pid, CloneError> {
    const DEFAULT_STACK_SIZE: usize = 8 * 1024 * 1024; // 8M
    const DEFAULT_PAGE_SIZE: usize = 4 * 1024; // 4K

    // Use sysconf to find the page size. If there is an error, we assume
    // the default 4K page size.
    let page_size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .map_err(CloneError::PageSize)?
        .map(|size| size as usize)
        .unwrap_or(DEFAULT_PAGE_SIZE);

    // Find out the default stack max size through getrlimit.
    let (rlim_cur, _) =
        resource::getrlimit(resource::Resource::RLIMIT_STACK).map_err(CloneError::ResourceLimit)?;
    // mmap will return ENOMEM if stack size is unlimited when we create the
    // child stack, so we need to set a reasonable default stack size.
    let default_stack_size = if rlim_cur != u64::MAX {
        rlim_cur as usize
    } else {
        tracing::debug!(
            "stack size returned by getrlimit() is unlimited, use DEFAULT_STACK_SIZE(8MB)"
        );
        DEFAULT_STACK_SIZE
    };

    // Using the clone syscall requires us to create the stack space for the
    // child process instead of taken cared for us like fork call. We use mmap
    // here to create the stack.  Instead of guessing how much space the child
    // process needs, we allocate through mmap to the system default limit,
    // which is 8MB on most of the linux system today. This is OK since mmap
    // will only researve the address space upfront, instead of allocating
    // physical memory upfront.  The stack will grow as needed, up to the size
    // researved, so no wasted memory here. Lastly, the child stack only needs
    // to support the container init process set up code in Youki. When Youki
    // calls exec into the container payload, exec will reset the stack.  Note,
    // do not use MAP_GROWSDOWN since it is not well supported.
    // Ref: https://man7.org/linux/man-pages/man2/mmap.2.html
    let child_stack = unsafe {
        mman::mmap(
            None,
            NonZeroUsize::new(default_stack_size).ok_or(CloneError::ZeroStackSize)?,
            mman::ProtFlags::PROT_READ | mman::ProtFlags::PROT_WRITE,
            mman::MapFlags::MAP_PRIVATE | mman::MapFlags::MAP_ANONYMOUS | mman::MapFlags::MAP_STACK,
            -1,
            0,
        )
        .map_err(CloneError::StackAllocation)?
    };
    unsafe {
        // Consistant with how pthread_create sets up the stack, we create a
        // guard page of 1 page, to protect the child stack collision. Note, for
        // clone call, the child stack will grow downward, so the bottom of the
        // child stack is in the beginning.
        mman::mprotect(child_stack, page_size, mman::ProtFlags::PROT_NONE)
            .map_err(CloneError::GuardPage)?;
    };

    // Since the child stack for clone grows downward, we need to pass in
    // the top of the stack address.
    let child_stack_top = unsafe { child_stack.add(default_stack_size) };

    // Combine the clone flags with exit signals.
    let combined_flags = (flags | exit_signal.unwrap_or(0)) as c_int;

    // We are passing the boxed closure "cb" into the clone function as the a
    // function pointer in C. The box closure in Rust is both a function pointer
    // and a struct. However, when casting the box closure into libc::c_void,
    // the function pointer will be lost. Therefore, to work around the issue,
    // we double box the closure. This is consistant with how std::unix::thread
    // handles the closure.
    // Ref: https://github.com/rust-lang/rust/blob/master/library/std/src/sys/unix/thread.rs
    let data = Box::into_raw(Box::new(Box::new(cb)));
    // The main is a wrapper function passed into clone call below. The "data"
    // arg is actually a raw pointer to a Box closure. so here, we re-box the
    // pointer back into a box closure so the main takes ownership of the
    // memory. Then we can call the closure passed in.
    extern "C" fn main(data: *mut libc::c_void) -> libc::c_int {
        unsafe { Box::from_raw(data as *mut CloneCb)() as i32 }
    }

    // The nix::sched::clone wrapper doesn't provide the right interface.  Using
    // the clone syscall is one of the rare cases where we don't want rust to
    // manage the child stack memory. Instead, we want to use c_void directly
    // here.  Therefore, here we are using libc::clone syscall directly for
    // better control.  The child stack will be cleaned when exec is called or
    // the child process terminates. The nix wrapper also does not treat the
    // closure memory correctly. The wrapper implementation fails to pass the
    // right ownership to the new child process.
    // Ref: https://github.com/nix-rust/nix/issues/919
    // Ref: https://github.com/nix-rust/nix/pull/920
    match unsafe {
        libc::clone(
            main,
            child_stack_top,
            combined_flags,
            data as *mut libc::c_void,
        )
    } {
        -1 => {
            // Since the clone call failed, the closure passed in didn't get
            // consumed. To complete the circle, we can safely box up the
            // closure again and let rust manage this memory for us.
            unsafe { drop(Box::from_raw(data)) };
            Err(CloneError::Clone(nix::Error::last()))
        }
        pid => Ok(Pid::from_raw(pid)),
    }
}

#[cfg(test)]
mod test {
    use crate::channel::channel;

    use super::*;
    use anyhow::{bail, Context, Result};
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd;

    #[test]
    fn test_container_fork() -> Result<()> {
        let pid = container_clone("test:child", || Ok(()))?;
        match waitpid(pid, None).expect("wait pid failed.") {
            WaitStatus::Exited(p, status) => {
                assert_eq!(pid, p);
                assert_eq!(status, 0);
                Ok(())
            }
            _ => bail!("test failed"),
        }
    }

    #[test]
    fn test_container_err_fork() -> Result<()> {
        let pid = container_clone("test:child", || Err(CallbackError::Test))?;
        match waitpid(pid, None).expect("wait pid failed.") {
            WaitStatus::Exited(p, status) => {
                assert_eq!(pid, p);
                assert_eq!(status, 255);
                Ok(())
            }
            _ => bail!("test failed"),
        }
    }

    #[test]
    fn test_container_clone_sibling() -> Result<()> {
        // The `container_clone_sibling` will create a sibling process (share
        // the same parent) of the calling process. In Unix, a process can only
        // wait on the immediate children process and can't wait on the sibling
        // process. Therefore, to test the logic, we will have to fork a process
        // first and then let the forked process call `container_clone_sibling`.
        // Then the testing process (the process where test is called), who are
        // the parent to this forked process and the sibling process cloned by
        // the `container_clone_sibling`, can wait on both processes.

        // We need to use a channel so that the forked process can pass the pid
        // of the sibling process to the testing process.
        let (sender, receiver) = &mut channel::<i32>()?;

        match unsafe { unistd::fork()? } {
            unistd::ForkResult::Parent { child } => {
                let sibling_process_pid =
                    Pid::from_raw(receiver.recv().with_context(|| {
                        "failed to receive the sibling pid from forked process"
                    })?);
                receiver.close()?;
                match waitpid(sibling_process_pid, None).expect("wait pid failed.") {
                    WaitStatus::Exited(p, status) => {
                        assert_eq!(sibling_process_pid, p);
                        assert_eq!(status, 0);
                    }
                    _ => bail!("failed to wait on the sibling process"),
                }
                // After sibling process exits, we can wait on the forked process.
                match waitpid(child, None).expect("wait pid failed.") {
                    WaitStatus::Exited(p, status) => {
                        assert_eq!(child, p);
                        assert_eq!(status, 0);
                    }
                    _ => bail!("failed to wait on the forked process"),
                }
            }
            unistd::ForkResult::Child => {
                // Inside the forked process. We call `container_clone` and pass
                // the pid to the parent process.
                let pid = container_clone_sibling("test:child", || Ok(()))?;
                sender.send(pid.as_raw())?;
                sender.close()?;
                std::process::exit(0);
            }
        };

        Ok(())
    }

    // This test depends on libseccomp to work.
    #[cfg(feature = "libseccomp")]
    #[test]
    fn test_clone_fallback() -> Result<()> {
        use crate::test_utils::TestCallbackError;
        use oci_spec::runtime::{
            Arch, LinuxSeccompAction, LinuxSeccompBuilder, LinuxSyscallBuilder,
        };

        fn has_clone3() -> bool {
            // We use the probe syscall to check if the kernel supports clone3 or
            // seccomp has successfully blocked clone3.
            let res = unsafe { libc::syscall(libc::SYS_clone3, 0, 0) };
            let err = (res == -1)
                .then(std::io::Error::last_os_error)
                .expect("probe syscall should not succeed");
            err.raw_os_error() != Some(libc::ENOSYS)
        }

        // To test the fallback behavior, we will create a seccomp rule that
        // blocks `clone3` as ENOSYS.
        let syscall = LinuxSyscallBuilder::default()
            .names(vec![String::from("clone3")])
            .action(LinuxSeccompAction::ScmpActErrno)
            .errno_ret(libc::ENOSYS as u32)
            .build()?;
        let seccomp_profile = LinuxSeccompBuilder::default()
            .default_action(LinuxSeccompAction::ScmpActAllow)
            .architectures(vec![Arch::ScmpArchNative])
            .syscalls(vec![syscall])
            .build()?;

        crate::test_utils::test_in_child_process(|| {
            // We use seccomp to block `clone3`
            let _ = prctl::set_no_new_privileges(true);
            crate::seccomp::initialize_seccomp(&seccomp_profile)
                .expect("failed to initialize seccomp");

            if has_clone3() {
                return Err(TestCallbackError::Custom(
                    "clone3 is not blocked by seccomp".into(),
                ));
            }

            let pid = container_clone("test:child", || Ok(())).map_err(|err| err.to_string())?;
            match waitpid(pid, None).expect("wait pid failed.") {
                WaitStatus::Exited(p, status) => {
                    assert_eq!(pid, p);
                    assert_eq!(status, 0);
                }
                status => {
                    return Err(TestCallbackError::Custom(format!(
                        "failed to wait on child process: {:?}",
                        status
                    )));
                }
            };

            Ok(())
        })?;

        Ok(())
    }
}
