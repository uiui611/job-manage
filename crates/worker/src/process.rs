//! Lifetime control for runtimes and their descendants. This is not a security sandbox.
use anyhow::{Context, Result};
use tokio::process::{Child, Command};

#[cfg(not(windows))]
pub fn initialize_worker() -> Result<()> {
    Ok(())
}

pub struct ProcessTree {
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ProcessTree {
    pub fn spawn(command: &mut Command) -> Result<(Child, Self)> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        {
            command.process_group(0);
            let child = command.spawn().context("cannot start selected runtime")?;
            let tree = Self { pid: child.id() };
            Ok((child, tree))
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::{
                JobObjects::AssignProcessToJobObject,
                Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED},
            };
            initialize_worker()?;
            let job = windows::create_job()?;
            // No runtime instruction may run until its per-attempt Job Object owns it.
            // The worker's outer Job Object covers a forced worker exit during this gap.
            command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
            let child = command.spawn().context("cannot start selected runtime")?;
            let handle = child.raw_handle().context("runtime handle unavailable")?;
            // SAFETY: both handles are live; child is still suspended and job is unnamed.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), handle) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("cannot contain runtime in Windows Job Object");
            }
            let tree = Self { job };
            windows::resume_primary_thread(child.id().context("runtime PID unavailable")?)?;
            Ok((child, tree))
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take() {
            // SAFETY: spawn made the runtime leader of its own process group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: this guard owns the live handle. Closing it also kills descendants.
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(
                    self.job.as_raw_handle(),
                    1,
                );
            }
        }
    }
}

#[cfg(windows)]
pub use windows::initialize_worker;

#[cfg(all(test, target_os = "linux"))]
pub fn is_alive(pid: u32) -> bool {
    let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    !state.is_empty()
        && !state
            .split(") ")
            .nth(1)
            .is_some_and(|tail| tail.starts_with('Z'))
}

#[cfg(all(test, windows))]
pub fn is_alive(pid: u32) -> bool {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    // SAFETY: no handle inheritance and only synchronization access requested.
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(ERROR_INVALID_PARAMETER as i32)
        );
        return false;
    }
    // SAFETY: successful OpenProcess returned an owned live handle.
    let _handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: raw stays live and a zero timeout never blocks the test executor.
    unsafe { WaitForSingleObject(raw, 0) == WAIT_TIMEOUT }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{process::Stdio, time::Duration};

    #[tokio::test]
    #[ignore = "subprocess fixture invoked by forced_worker_exit_kills_descendants"]
    async fn worker_exit_fixture() {
        initialize_worker().unwrap();
        let python = std::env::var("JOB_TEST_PYTHON").unwrap_or_else(|_| "python".into());
        let pid_file = std::env::var("JOB_TEST_PID_FILE").unwrap();
        let mut command = Command::new(python);
        command.arg("-c").arg("import os,pathlib,time; pathlib.Path(os.environ['JOB_TEST_PID_FILE']).write_text(str(os.getpid())); time.sleep(60)").env("JOB_TEST_PID_FILE", pid_file).stdout(Stdio::null()).stderr(Stdio::null()).stdin(Stdio::null());
        let (_child, _tree) = ProcessTree::spawn(&mut command).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    }

    #[tokio::test]
    async fn forced_worker_exit_kills_descendants() {
        let temporary = tempfile::tempdir().unwrap();
        let pid_file = temporary.path().join("runtime.pid");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "process::tests::worker_exit_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("JOB_TEST_PID_FILE", &pid_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        let (mut worker, _tree) = ProcessTree::spawn(&mut command).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("subprocess fixture did not start its runtime");
        let pid = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        assert!(is_alive(pid));
        worker.kill().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while is_alive(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("runtime survived forced worker exit");
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use anyhow::{bail, ensure};
    use std::{
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr::null,
        sync::OnceLock,
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::{GetCurrentProcess, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    pub fn create_job() -> Result<OwnedHandle> {
        // SAFETY: no security attributes/name; the job handle is not inheritable.
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        ensure!(
            !raw.is_null(),
            "cannot create Windows Job Object: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: CreateJobObjectW transferred a valid owned handle.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: this C structure is valid zero-initialized and contains no references.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // No BREAKAWAY flags: children stay contained, including nested subprocesses.
        // SAFETY: structure pointer/size match the requested information class.
        if unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("cannot configure Windows Job Object");
        }
        Ok(job)
    }

    pub fn initialize_worker() -> Result<()> {
        // Intentionally process-lifetime: static handles close on process exit, even
        // TerminateProcess. Every runtime inherits this outer job before executing.
        static WORKER_JOB: OnceLock<std::result::Result<OwnedHandle, String>> = OnceLock::new();
        let result = WORKER_JOB.get_or_init(|| {
            (|| -> Result<OwnedHandle> {
                let job = create_job()?;
                // SAFETY: pseudo current-process handle is accepted here; job is live.
                if unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) }
                    == 0
                {
                    return Err(std::io::Error::last_os_error())
                        .context("cannot contain worker in Windows Job Object");
                }
                Ok(job)
            })()
            .map_err(|error| format!("{error:#}"))
        });
        match result {
            Ok(_) => Ok(()),
            Err(error) => bail!("{error}"),
        }
    }

    pub fn resume_primary_thread(pid: u32) -> Result<()> {
        // CREATE_SUSPENDED leaves the new process's initial thread suspended.
        // std/tokio retain the process handle, not the primary thread handle.
        // SAFETY: snapshot API accepts these flags with ignored PID.
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        ensure!(
            raw != INVALID_HANDLE_VALUE,
            "cannot inspect suspended runtime thread"
        );
        // SAFETY: raw is a valid owned snapshot handle.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: plain C structure, initialized dwSize is required by Toolhelp.
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        // SAFETY: snapshot and writable structure are valid throughout iteration.
        let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
        while found != 0 {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: thread belongs to our suspended child; request only resume access.
                let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                ensure!(!raw.is_null(), "cannot open suspended runtime thread");
                // SAFETY: OpenThread transferred a valid owned handle.
                let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
                // SAFETY: thread handle is live and has THREAD_SUSPEND_RESUME access.
                let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
                ensure!(previous == 1, "cannot resume contained runtime thread");
                return Ok(());
            }
            // SAFETY: same live snapshot and initialized structure as above.
            found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
        }
        bail!("suspended runtime thread not found")
    }
}
