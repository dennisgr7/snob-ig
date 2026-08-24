//! Starting the browser with its debugging protocol on a private pipe, and
//! tying its lifetime to ours.
//!
//! **This is what replaces the loopback debugging port**, and the reason it
//! exists is a demonstrated hole rather than a theoretical one. With
//! `--remote-debugging-port`, the browser listens on 127.0.0.1 and answers
//! anybody who asks: a second local process read the port out of
//! `DevToolsActivePort`, called `/json/version` with no credential at all, and
//! got the Instagram session cookie back from `Storage.getCookies` — `httpOnly`
//! is a rule for page scripts and means nothing to the protocol itself.
//! Loopback sockets carry no per-user access control, so that was every account
//! on the machine, for as long as the login window stayed open.
//!
//! `--remote-debugging-pipe` moves the same protocol onto two anonymous pipes
//! that only this process and the browser hold handles to. There is nothing to
//! connect to and nothing to guess.
//!
//! **Why this module exists at all** is that `std::process::Command` cannot
//! start that browser. Chromium reads the protocol from file descriptor 3 and
//! writes it to descriptor 4, and on Windows it reaches them through
//! `_get_osfhandle`, which only answers if the C runtime found those
//! descriptors in the handle-inheritance blob the parent passed in
//! `STARTUPINFO.lpReserved2`. `Command` exposes stdin, stdout and stderr and
//! nothing beyond them, so the blob has to be built here and handed to
//! `CreateProcessW` directly. On Unix the same idea is four lines of `dup2` in
//! a `pre_exec` hook.
//!
//! **The job object is the other half, and it closes a case no handler can
//! catch.** `kill_on_panic` covers a panic and the interrupt handler covers
//! Ctrl+C, but neither runs when snob is killed from outside — Task Manager,
//! a service manager's stop timeout, `taskkill /F`. The browser would then be
//! left running with a live Instagram session in it. A job object carrying
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` makes the kernel do it: when the last
//! handle to the job goes, which is when this process's handles are closed
//! however it died, everything in the job goes with it. The child is created
//! suspended and assigned to the job before it is resumed, so there is no
//! instant in which it exists outside the job.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

/// Ceiling on one protocol message.
///
/// The transport reads until a NUL, so without a ceiling a browser that never
/// sent one would decide how much memory this process uses. `Browser.getVersion`
/// and `Storage.getCookies` answer in kilobytes; this is far above anything
/// real.
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// How the two ends of the protocol are numbered.
///
/// Chromium's own constants: it reads commands from 3 and writes events and
/// replies to 4. They are not configurable, on either platform.
const CHILD_READ_FD: usize = 3;
const CHILD_WRITE_FD: usize = 4;

/// One message, in either direction, without its NUL terminator.
pub type Message = Vec<u8>;

/// The browser's end of the protocol.
///
/// Reading and writing are done by two ordinary threads rather than by async
/// file handles, because a pipe is not pollable the same way on both platforms
/// and this needs no concurrency beyond "one reader, one writer". The channels
/// are what the async side sees.
pub struct PipeTransport {
    to_browser: tokio::sync::mpsc::Sender<Message>,
    from_browser: tokio::sync::mpsc::Receiver<Message>,
}

impl PipeTransport {
    /// Sends one message. `Err` means the browser's end has gone.
    pub async fn send(&self, message: Message) -> Result<()> {
        self.to_browser
            .send(message)
            .await
            .map_err(|_| anyhow::anyhow!("the connection to the browser broke"))
    }

    /// Waits for the next message. `None` means the browser closed the pipe.
    pub async fn recv(&mut self) -> Option<Message> {
        self.from_browser.recv().await
    }
}

/// Pulls whole messages out of whatever has arrived so far.
///
/// The protocol over a pipe is JSON documents separated by NUL bytes, and a
/// read returns whatever the pipe had — half a message, several messages, a
/// message and half of the next. Split out as a pure function precisely so that
/// case can be tested without a browser: it was the one part of this transport
/// that a WebSocket library used to do for us.
///
/// Anything left over stays in `buffer` for the next read, and `scanned`
/// remembers how much of it has already been searched.
///
/// The cursor is what keeps this linear. Without it, every read re-searched
/// the buffer from byte zero while a message arrived in pieces, which is
/// O(len²/chunk): at the 8 MiB ceiling that is on the order of four billion
/// bytes scanned to deliver one message -- and `Storage.getCookies` against a
/// real profile is exactly the kind of answer that arrives in pieces. The
/// caller owns the cursor for the same reason it owns the buffer: this stays
/// a pure function a test can drive.
fn take_messages(buffer: &mut Vec<u8>, scanned: &mut usize) -> Vec<Message> {
    let mut out = Vec::new();
    while let Some(end) = buffer[*scanned..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|found| found + *scanned)
    {
        let mut message: Vec<u8> = buffer.drain(..=end).collect();
        message.pop();
        out.push(message);
        // The drain shifted everything left, so the next search starts over
        // -- at the front of a buffer that no longer holds what was searched.
        *scanned = 0;
    }
    *scanned = buffer.len();
    out
}

/// Wires two blocking file handles up to the channels above.
fn pump<R, W>(reader: R, writer: W) -> PipeTransport
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    // Bounded, so a browser narrating faster than the login loop reads cannot
    // grow this without limit. Sixty-four is far more than the handful of
    // messages a login exchanges.
    let (to_browser, mut outgoing) = tokio::sync::mpsc::channel::<Message>(64);
    let (incoming, from_browser) = tokio::sync::mpsc::channel::<Message>(64);

    let mut reader = reader;
    std::thread::spawn(move || {
        let mut buffer: Vec<u8> = Vec::new();
        let mut scanned = 0usize;
        // 64 KiB rather than 8: a cookie answer runs to hundreds of
        // kilobytes, and the chunk size is how many syscalls that costs.
        let mut chunk = [0u8; 65536];
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > MAX_MESSAGE_BYTES {
                tracing::warn!("the browser sent a protocol message past the ceiling");
                return;
            }
            for message in take_messages(&mut buffer, &mut scanned) {
                if incoming.blocking_send(message).is_err() {
                    return;
                }
            }
        }
    });

    let mut writer = writer;
    std::thread::spawn(move || {
        while let Some(mut message) = outgoing.blocking_recv() {
            message.push(0);
            if writer.write_all(&message).is_err() || writer.flush().is_err() {
                return;
            }
        }
    });

    PipeTransport {
        to_browser,
        from_browser,
    }
}

/// How a process ended.
#[derive(Debug, Clone, Copy)]
pub struct Ended {
    /// `None` on Unix when a signal killed it, which is a different story from
    /// an exit code and is told as one.
    pub code: Option<i32>,
}

/// The browser this process started.
///
/// Owns whatever has to be closed for the browser to go away, which on Windows
/// includes the job handle: dropping this is what kills it there, even if this
/// process is dying in a way that runs no other code of ours.
pub struct BrowserProcess {
    #[cfg(windows)]
    inner: windows_impl::Process,
    #[cfg(unix)]
    inner: std::process::Child,
}

impl BrowserProcess {
    pub fn id(&self) -> u32 {
        #[cfg(windows)]
        {
            self.inner.pid
        }
        #[cfg(unix)]
        {
            self.inner.id()
        }
    }

    /// Whether it has already exited, without waiting for it.
    pub fn try_wait(&mut self) -> std::io::Result<Option<Ended>> {
        #[cfg(windows)]
        {
            self.inner.try_wait()
        }
        #[cfg(unix)]
        {
            Ok(self.inner.try_wait()?.map(|status| Ended {
                code: status.code(),
            }))
        }
    }

    /// Waits for it to leave, giving up after `patience`.
    ///
    /// Off the async worker: this is a blocking wait on a process handle, and
    /// it runs on the way out of a command that has nothing else to do.
    pub async fn wait_up_to(&mut self, patience: Duration) -> Option<Ended> {
        let deadline = std::time::Instant::now() + patience;
        loop {
            match self.try_wait() {
                Ok(Some(ended)) => return Some(ended),
                Ok(None) => {}
                Err(_) => return None,
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Ends it now.
    pub fn kill(&mut self) {
        #[cfg(windows)]
        {
            self.inner.kill();
        }
        #[cfg(unix)]
        {
            let _ = self.inner.kill();
        }
    }
}

/// Starts `program` with `args`, talking the debugging protocol over a pipe.
///
/// The browser is in a job object that takes it down with this process, and the
/// two protocol descriptors are the only handles it inherits.
pub fn spawn(program: &Path, args: &[String]) -> Result<(BrowserProcess, PipeTransport)> {
    #[cfg(windows)]
    {
        windows_impl::spawn(program, args)
    }
    #[cfg(unix)]
    {
        unix_impl::spawn(program, args)
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;

    use anyhow::Result;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        SetHandleInformation, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        ResumeThread, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };

    use super::{CHILD_READ_FD, CHILD_WRITE_FD, Ended};

    /// The two flag bits the C runtime wants on an inherited descriptor.
    ///
    /// `FOPEN` says the slot is in use and `FPIPE` says what it is. They are
    /// not in any header this can import: the layout of `lpReserved2` is a
    /// private arrangement between a Microsoft C runtime and its own
    /// `CreateProcess` wrapper, which is why it has to be written out by hand
    /// here. libuv builds the same structure for Node, which is how every
    /// `--remote-debugging-pipe` client on Windows already works.
    const FOPEN: u8 = 0x01;
    const FPIPE: u8 = 0x08;

    /// A process that is inside a job that kills it when this struct is
    /// dropped.
    pub(super) struct Process {
        handle: HANDLE,
        /// Held for exactly as long as the browser should live. Closing it is
        /// what `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` reacts to, and the
        /// operating system closes it for us however this process dies.
        job: HANDLE,
        pub(super) pid: u32,
        ended: Option<Ended>,
    }

    // SAFETY: the two handles are owned by this struct and reached only through
    // it, and `&mut self` on every method that touches them means no two threads
    // hold one at once. Windows handles are process-wide values with no thread
    // affinity, so moving one across a thread is what the API already expects.
    unsafe impl Send for Process {}
    // SAFETY: the same, for a shared reference. Nothing reachable through `&self`
    // reads or writes either handle.
    unsafe impl Sync for Process {}

    impl Process {
        pub(super) fn try_wait(&mut self) -> std::io::Result<Option<Ended>> {
            if let Some(ended) = self.ended {
                return Ok(Some(ended));
            }
            // SAFETY: `handle` is a process handle this struct owns.
            let waited = unsafe { WaitForSingleObject(self.handle, 0) };
            if waited != WAIT_OBJECT_0 {
                return Ok(None);
            }
            let mut code: u32 = 0;
            // SAFETY: the process has signaled, so the code is final.
            unsafe { GetExitCodeProcess(self.handle, &mut code) };
            let ended = Ended {
                code: Some(code as i32),
            };
            self.ended = Some(ended);
            Ok(Some(ended))
        }

        pub(super) fn kill(&mut self) {
            // SAFETY: `handle` is a process handle this struct owns. A process
            // that has already gone answers an error, which is not interesting.
            unsafe { TerminateProcess(self.handle, 1) };
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // The job goes first. Closing the last handle to it is what takes
            // the browser down, and it must happen whether or not anybody
            // remembered to kill anything.
            // SAFETY: both handles are owned by this struct and closed once.
            unsafe {
                CloseHandle(self.job);
                CloseHandle(self.handle);
            }
        }
    }

    /// An owned handle, so that every early return closes what it opened.
    struct Owned(HANDLE);

    impl Owned {
        fn take(&mut self) -> HANDLE {
            std::mem::replace(&mut self.0, INVALID_HANDLE_VALUE)
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
                // SAFETY: owned, and replaced with a sentinel when handed on.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn last_error(what: &str) -> anyhow::Error {
        // SAFETY: reads a thread-local error code.
        anyhow::anyhow!("{what} failed: Windows error {}", unsafe { GetLastError() })
    }

    /// One pipe, with the end the child gets marked inheritable and the end we
    /// keep marked not.
    ///
    /// Both matter. The child's end has to be inheritable or the handle list
    /// below refuses it; ours has to not be, or a browser holding a copy of our
    /// own end would keep the pipe open after it had exited and the reader
    /// thread would never see the end of the stream.
    fn pipe() -> Result<(Owned, Owned)> {
        let mut read: HANDLE = std::ptr::null_mut();
        let mut write: HANDLE = std::ptr::null_mut();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: both out-parameters are valid for the call.
        if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
            return Err(last_error("CreatePipe"));
        }
        Ok((Owned(read), Owned(write)))
    }

    fn keep_out_of_the_child(handle: HANDLE) -> Result<()> {
        // SAFETY: a handle this process owns.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(last_error("SetHandleInformation"));
        }
        Ok(())
    }

    /// The C runtime's handle-inheritance blob.
    ///
    /// Five slots, because the descriptor numbers are positional: 0, 1 and 2
    /// have to be present and closed for 3 and 4 to land where Chromium looks
    /// for them. Its layout is a count, then one flag byte per slot, then one
    /// handle per slot.
    ///
    /// Written out rather than imported because no Windows header describes
    /// it — see [`FOPEN`].
    fn inheritance_blob(child_read: HANDLE, child_write: HANDLE) -> Vec<u8> {
        let count = CHILD_WRITE_FD + 1;
        let mut blob = Vec::with_capacity(4 + count + count * std::mem::size_of::<HANDLE>());

        blob.extend_from_slice(&(count as u32).to_ne_bytes());
        for fd in 0..count {
            let open = fd == CHILD_READ_FD || fd == CHILD_WRITE_FD;
            blob.push(if open { FOPEN | FPIPE } else { 0 });
        }
        for fd in 0..count {
            let handle = match fd {
                CHILD_READ_FD => child_read,
                CHILD_WRITE_FD => child_write,
                _ => INVALID_HANDLE_VALUE,
            };
            blob.extend_from_slice(&(handle as usize).to_ne_bytes());
        }
        blob
    }

    /// Quotes one argument the way the C runtime un-quotes it.
    ///
    /// `CreateProcessW` takes a single string and every program splits it
    /// itself, so this has to produce what the split on the other side will put
    /// back together. The backslash rule is the awkward part: a run of
    /// backslashes is only special immediately before a quote, where each one
    /// has to be doubled.
    ///
    /// It matters here rather than being pedantry: the profile directory is one
    /// of these arguments and it sits under the user's own path, which contains
    /// spaces on almost every Windows machine.
    fn quote(argument: &str) -> String {
        if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
            return argument.to_string();
        }
        let mut out = String::with_capacity(argument.len() + 2);
        out.push('"');
        let mut backslashes = 0;
        for ch in argument.chars() {
            match ch {
                '\\' => {
                    backslashes += 1;
                    out.push('\\');
                }
                '"' => {
                    for _ in 0..=backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push('"');
                }
                _ => {
                    backslashes = 0;
                    out.push(ch);
                }
            }
        }
        for _ in 0..backslashes {
            out.push('\\');
        }
        out.push('"');
        out
    }

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub(super) fn spawn(
        program: &Path,
        args: &[String],
    ) -> Result<(super::BrowserProcess, super::PipeTransport)> {
        // Named from the browser's point of view, which is the direction that
        // keeps getting mixed up: `to_browser` is the pipe it reads commands
        // from, so we hold the write end and it inherits the read end.
        let (mut to_browser_read, mut to_browser_write) = pipe()?;
        let (mut from_browser_read, mut from_browser_write) = pipe()?;
        keep_out_of_the_child(to_browser_write.0)?;
        keep_out_of_the_child(from_browser_read.0)?;

        let mut blob = inheritance_blob(to_browser_read.0, from_browser_write.0);

        // Exactly the two protocol handles, and nothing else this process
        // happens to hold open. Without it `bInheritHandles = TRUE` hands the
        // child every inheritable handle in the process, which is how a
        // database file or a socket ends up in a browser.
        let mut inherited: [HANDLE; 2] = [to_browser_read.0, from_browser_write.0];
        let mut attribute_size: usize = 0;
        // SAFETY: the documented way to ask for the size; it fails and sets the
        // size, which is why the return value is ignored here.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attribute_size)
        };
        let mut attribute_storage = vec![0u8; attribute_size];
        let attributes = attribute_storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        // SAFETY: `attributes` points at `attribute_size` bytes, which is what
        // the call above asked for.
        if unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &mut attribute_size) } == 0
        {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        // SAFETY: the handle array outlives the call to `CreateProcessW`, which
        // is what this attribute requires.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited.as_mut_ptr() as *mut std::ffi::c_void,
                std::mem::size_of_val(&inherited),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if updated == 0 {
            // SAFETY: initialized above.
            unsafe { DeleteProcThreadAttributeList(attributes) };
            return Err(last_error("UpdateProcThreadAttribute"));
        }

        // SAFETY: all-zero is the documented empty value for this structure —
        // `cb` and the fields below are what fill it in, and every pointer in it
        // is one Windows reads as absent when null.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.cbReserved2 = blob.len() as u16;
        startup.StartupInfo.lpReserved2 = blob.as_mut_ptr();
        startup.lpAttributeList = attributes;

        let mut command_line = quote(&program.display().to_string());
        for argument in args {
            command_line.push(' ');
            command_line.push_str(&quote(argument));
        }
        let mut command_line = wide(&command_line);

        // SAFETY: an out parameter. `CreateProcessW` fills it, and all-zero is
        // what it is documented to be handed.
        let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // Suspended, so the job below is joined before a single instruction of
        // the browser runs. Created after the job would leave an instant in
        // which a browser exists outside it, and that instant is the whole of
        // what this is protecting against.
        //
        // SAFETY: every pointer here is to storage that outlives the call, and
        // `command_line` is the writable buffer `CreateProcessW` requires.
        let created = unsafe {
            CreateProcessW(
                std::ptr::null(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
                std::ptr::null(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut information,
            )
        };
        // SAFETY: initialized above, and no longer needed either way.
        unsafe { DeleteProcThreadAttributeList(attributes) };
        if created == 0 {
            return Err(last_error("CreateProcessW"));
        }

        let mut process = Owned(information.hProcess);
        let thread = Owned(information.hThread);

        // The child holds its own copies now, and ours have to go: a pipe stays
        // open while any handle to either end is open, so keeping these would
        // mean the reader thread never sees the browser leave.
        //
        // SAFETY: handed to the child by inheritance; these are our own copies,
        // taken out of their owners so nothing closes them twice.
        unsafe {
            CloseHandle(to_browser_read.take());
            CloseHandle(from_browser_write.take());
        }

        let job = match make_job() {
            Ok(job) => job,
            Err(e) => {
                // SAFETY: a process handle we own, suspended and unreachable.
                unsafe { TerminateProcess(process.0, 1) };
                return Err(e);
            }
        };
        // SAFETY: both handles are ours; the process has not run yet.
        if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
            let failure = last_error("AssignProcessToJobObject");
            // SAFETY: as above.
            unsafe { TerminateProcess(process.0, 1) };
            return Err(failure);
        }

        // SAFETY: the thread handle from `CreateProcessW`, resumed once.
        if unsafe { ResumeThread(thread.0) } == u32::MAX {
            let failure = last_error("ResumeThread");
            // SAFETY: as above.
            unsafe { TerminateProcess(process.0, 1) };
            return Err(failure);
        }
        drop(thread);

        let mut job = job;
        let inner = Process {
            handle: process.take(),
            job: job.take(),
            pid: information.dwProcessId,
            ended: None,
        };

        // SAFETY: two pipe handles this process owns and hands over exactly
        // once; `File` closes them from here on.
        let writer = unsafe { std::fs::File::from_raw_handle(to_browser_write.take() as _) };
        // SAFETY: the same, for the other end. `take` is what makes each handover
        // happen exactly once.
        let reader = unsafe { std::fs::File::from_raw_handle(from_browser_read.take() as _) };

        Ok((super::BrowserProcess { inner }, super::pump(reader, writer)))
    }

    /// A job whose closing kills what is in it.
    fn make_job() -> Result<Owned> {
        // SAFETY: an unnamed job with default security.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = Owned(job);

        // SAFETY: all-zero means "no limits", which is the base this then sets
        // exactly one flag on.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the structure matches the class being set.
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(last_error("SetInformationJobObject"));
        }
        Ok(job)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The blob is positional: five slots, the first three closed, and the
        /// two the protocol uses carrying a pipe's flags. Getting the count or
        /// the order wrong does not fail to start the browser — it starts one
        /// that cannot find the protocol and sits there until the login times
        /// out, which is a far more expensive way to learn about it.
        #[test]
        fn the_inheritance_blob_puts_the_pipes_at_three_and_four() {
            let read = 0x1111_usize as HANDLE;
            let write = 0x2222_usize as HANDLE;
            let blob = inheritance_blob(read, write);

            let width = std::mem::size_of::<HANDLE>();
            assert_eq!(blob.len(), 4 + 5 + 5 * width);
            assert_eq!(&blob[..4], &5u32.to_ne_bytes());

            let flags = &blob[4..9];
            assert_eq!(flags[0], 0);
            assert_eq!(flags[1], 0);
            assert_eq!(flags[2], 0);
            assert_eq!(flags[3], FOPEN | FPIPE);
            assert_eq!(flags[4], FOPEN | FPIPE);

            let handles = &blob[9..];
            let at = |slot: usize| -> usize {
                let bytes = &handles[slot * width..(slot + 1) * width];
                usize::from_ne_bytes(bytes.try_into().unwrap())
            };
            assert_eq!(at(0), INVALID_HANDLE_VALUE as usize);
            assert_eq!(at(1), INVALID_HANDLE_VALUE as usize);
            assert_eq!(at(2), INVALID_HANDLE_VALUE as usize);
            assert_eq!(at(3), read as usize);
            assert_eq!(at(4), write as usize);
        }

        /// The profile directory sits under the user's own path, which has a
        /// space in it on almost every Windows machine, so this is the ordinary
        /// case rather than the exotic one.
        #[test]
        fn arguments_survive_being_put_on_one_command_line() {
            assert_eq!(quote("--no-first-run"), "--no-first-run");
            assert_eq!(
                quote(r"--user-data-dir=C:\Users\Ann Smith\data"),
                r#""--user-data-dir=C:\Users\Ann Smith\data""#
            );
            // A run of backslashes only doubles in front of a quote.
            assert_eq!(quote(r"a\\b c"), r#""a\\b c""#);
            assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
            // Nothing here needs quoting, so a trailing backslash is left
            // exactly as it came: it is only special in front of the closing
            // quote that this form does not have.
            assert_eq!(quote(r"ends\"), r"ends\");
            // And when there is a closing quote, it doubles.
            assert_eq!(quote(r"a path\"), r#""a path\\""#);
            assert_eq!(quote(""), r#""""#);
        }
    }
}

#[cfg(unix)]
mod unix_impl {
    use std::os::unix::io::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::Path;

    use anyhow::{Context, Result};

    use super::{CHILD_READ_FD, CHILD_WRITE_FD};

    /// Both ends of one pipe, as raw descriptors.
    fn pipe() -> Result<(i32, i32)> {
        let mut ends = [0 as libc::c_int; 2];
        // SAFETY: `ends` is the two-element array the call fills in.
        if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("could not open a pipe");
        }
        Ok((ends[0], ends[1]))
    }

    pub(super) fn spawn(
        program: &Path,
        args: &[String],
    ) -> Result<(super::BrowserProcess, super::PipeTransport)> {
        // Named from the browser's point of view, as on Windows.
        let (to_browser_read, to_browser_write) = pipe()?;
        let (from_browser_read, from_browser_write) = pipe()?;

        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        // SAFETY: everything in here is async-signal-safe -- `dup2` and
        // `close`, and nothing that allocates or takes a lock. That is the
        // whole contract of `pre_exec`, and it is why the descriptor numbers
        // are computed before the fork rather than inside it.
        //
        // `dup2` clears close-on-exec on the descriptor it creates, which is
        // what puts these two through the `exec` and in front of Chromium.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(to_browser_read, CHILD_READ_FD as libc::c_int) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(from_browser_write, CHILD_WRITE_FD as libc::c_int) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Closed by the numbers captured before the fork — and
                // `libc::pipe` hands out the lowest free descriptors, so with
                // nothing else open those numbers are 3 and 4: exactly where
                // the two `dup2`s above just installed the protocol ends. A
                // blind `close(4)` then closed the pipe the child was about
                // to speak on. A descriptor sitting on a protocol number is
                // either already the right end or was already replaced by
                // `dup2`, so there is nothing left to close either way.
                for stray in [to_browser_write, from_browser_read] {
                    if stray != CHILD_READ_FD as libc::c_int
                        && stray != CHILD_WRITE_FD as libc::c_int
                    {
                        libc::close(stray);
                    }
                }
                Ok(())
            });
        }

        let child = command
            .spawn()
            .with_context(|| format!("could not start {}", program.display()))?;

        // Ours to close: a pipe stays open while any handle to either end is,
        // so keeping the child's ends would mean never seeing it leave.
        // SAFETY: descriptors this process owns and closes once.
        unsafe {
            libc::close(to_browser_read);
            libc::close(from_browser_write);
        }

        // SAFETY: two descriptors this process owns and hands over exactly
        // once; `File` closes them from here on.
        let writer = unsafe { std::fs::File::from_raw_fd(to_browser_write) };
        // SAFETY: the same, for the other end.
        let reader = unsafe { std::fs::File::from_raw_fd(from_browser_read) };

        Ok((
            super::BrowserProcess { inner: child },
            super::pump(reader, writer),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read returns whatever the pipe had, which is not whatever the browser
    /// sent: half a message, three messages, a message and the start of the
    /// next. The WebSocket library this replaced did the reassembly for us, so
    /// this is the piece that arrived with the transport and it is the one that
    /// can be tested without a browser.
    #[test]
    fn messages_are_reassembled_across_reads() {
        let mut buffer = Vec::new();
        let mut scanned = 0;

        buffer.extend_from_slice(br#"{"id":1}"#);
        assert!(
            take_messages(&mut buffer, &mut scanned).is_empty(),
            "no terminator has arrived yet"
        );
        assert_eq!(scanned, buffer.len(), "what was searched is remembered");

        buffer.push(0);
        let first = take_messages(&mut buffer, &mut scanned);
        assert_eq!(first, vec![br#"{"id":1}"#.to_vec()]);
        assert!(buffer.is_empty());
        assert_eq!(scanned, 0);

        // Two whole messages and the beginning of a third, in one read.
        buffer.extend_from_slice(b"{\"id\":2}\0{\"id\":3}\0{\"id\"");
        let rest = take_messages(&mut buffer, &mut scanned);
        assert_eq!(rest, vec![br#"{"id":2}"#.to_vec(), br#"{"id":3}"#.to_vec()]);
        assert_eq!(buffer, b"{\"id\"");
        assert_eq!(scanned, buffer.len());
    }

    /// An empty message is a message. Dropping it would desynchronize the
    /// stream rather than skip a blank line.
    #[test]
    fn an_empty_message_is_still_one() {
        let mut buffer = b"\0a\0".to_vec();
        assert_eq!(
            take_messages(&mut buffer, &mut 0),
            vec![Vec::new(), b"a".to_vec()]
        );
    }
}
