//! Spawn a process with elevated privileges on Windows using "runas"

use std::ffi::OsString;
use std::io;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use windows::core::*;
use windows::Win32::System::Threading::TerminateProcess;
use windows::Win32::Foundation;
use windows::Win32::System::Threading;
use windows::Win32::System::WindowsProgramming::INFINITE;
use windows::Win32::UI::Shell;
use windows::Win32::UI::WindowsAndMessaging;

/// Builder similar to [`std::process::Command`]
#[derive(Clone)]
pub struct Command {
    executable: HSTRING,
    args: Vec<HSTRING>,
    hide: bool,
    cwd: Option<HSTRING>,
}

/// Handle to a running process with admin privileges
pub struct Child {
    // Storing the HSTRINGs that were used as PCWSTR to avoid dropping the inner memory
    _command: Command,
    _params: HSTRING,
    process_handle: Foundation::HANDLE, // do not store whole exec_info to have Child: Send
}

impl Command {
    pub fn new<S: AsRef<OsStr>>(exe: S) -> Self {
        Self {
            executable: HSTRING::from(exe.as_ref()),
            args: Vec::new(),
            hide: true,
            cwd: None,
        }
    }

    /// Should the command be run with SW_HIDE
    pub fn hide(&mut self, hide: bool) -> &mut Self {
        self.hide = hide;
        self
    }

    /// Set arguments to be passed to the process
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>
    {
        self.args = args.into_iter()
            .map(|s| HSTRING::from(s.as_ref()))
            .collect();
        self
    }

    /// Push an argument
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.args.push(HSTRING::from(arg.as_ref()));
        self
    }

    /// Set current directory of the process
    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.cwd = Some(HSTRING::from(dir.as_ref()));
        self
    }

    fn quote_param(s: &str) -> String {
        let mut param = String::new();
        param.push('"');
        for c in s.chars() {
            match c {
                '\\' => param.push_str("\\\\"),
                '"' => param.push_str("\\\""),
                c => param.push(c),
            }
        }
        param.push('"');
        param
    }

    fn params(&self) -> HSTRING {
        let sep: &OsStr = " ".as_ref();
        let params = self.args.iter()
            .map(|arg| {
                let s = arg.to_string_lossy();
                if arg.is_empty() {
                    OsString::from("\"\"")
                } else if s.find(&[' ', '\t', '"'][..]).is_none() {
                    OsString::from(s)
                } else {
                    OsString::from(Self::quote_param(&s))
                }
            })
            .collect::<Vec<_>>()
            .join(sep);
        HSTRING::from(params)
    }

    /// Spawn the process and return its handle
    pub fn spawn(&mut self) -> io::Result<Child> {
        let show = if self.hide {
            WindowsAndMessaging::SW_HIDE
        } else {
            WindowsAndMessaging::SW_NORMAL
        };
        let params = self.params();
        let dir = self.cwd.as_ref().map_or(PCWSTR::null(), |cwd| PCWSTR::from_raw(cwd.as_ptr()));

        let mut exec_info = Shell::SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<Shell::SHELLEXECUTEINFOW>() as u32,
            fMask: Shell::SEE_MASK_NOCLOSEPROCESS, // hProcess will receive process handle
            hwnd: Foundation::HWND(0),
            lpVerb: w!("runas"), // run [a]dmini[s]trator
            // we pass the executable string to Child so the inner pointer should stay valid?
            lpFile: PCWSTR::from_raw(self.executable.as_ptr()),
            lpParameters: PCWSTR::from_raw(params.as_ptr()),
            lpDirectory: dir,
            nShow: show.0 as i32,
            ..Default::default()
        };

        // With SEE_MASK_NOCLOSEPROCESS hInstApp is set to >=32 on success or SE_ERR_XXX on failure
        unsafe {
            if !Shell::ShellExecuteExW(&mut exec_info).as_bool() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "ShellExecuteExW failed"
                ));
            }
        }
        if let err @ 0..=31 = exec_info.hInstApp.0 as u32 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                se_err_string(err),
            ));
        } else if exec_info.hProcess.is_invalid() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "No process was spawned",
            ));
        }

        Ok(Child {
            _command: self.clone(),
            _params: params,
            process_handle: exec_info.hProcess,
        })
    }
}

impl Child {
    /// Wait for process completion up until `timeout`.
    ///
    /// Returns `Ok(None)` on timeout and `Ok(Some(()))` if process completed.
    ///
    /// # Panics
    ///
    /// If timeout is longer than u32 milliseconds.
    pub fn try_wait(&self, timeout: Duration) -> io::Result<Option<()>> {
        self.try_wait_raw(timeout.as_millis().try_into().unwrap())
    }

    fn try_wait_raw(&self, millis: u32) -> io::Result<Option<()>> {
        let status = unsafe {
            Threading::WaitForSingleObject(self.process_handle, millis)
        };
        match status {
            Foundation::WAIT_TIMEOUT => Ok(None),
            Foundation::WAIT_OBJECT_0 => Ok(Some(())),
            _ => Err(io::Error::new(io::ErrorKind::Other, format!("error code {}", status.0)))
        }
    }

    /// Wait for process completion without a timeout.
    pub fn wait(&self) -> io::Result<()> {
        self.try_wait_raw(INFINITE)
            .map(|res| assert!(res.is_some()))
    }

    /// Kill a running process, will succeed if the process already exited.
    pub fn kill(&mut self) -> io::Result<()> {
        // Don't kill if it already exited
        if let Ok(Some(())) = self.try_wait_raw(0) {
            return Ok(());
        }
        let ok = unsafe {
            TerminateProcess(self.process_handle, 0).into()
        };
        if ok {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "Could not terminate process"))
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        unsafe {
            Foundation::CloseHandle(self.process_handle);
        }
    }
}

fn se_err_string(err: u32) -> String {
    match err {
        Shell::SE_ERR_FNF => "File not found.".into(),
        Shell::SE_ERR_PNF => "Path not found.".into(),
        Shell::SE_ERR_ACCESSDENIED => "Access denied.".into(),
        Shell::SE_ERR_OOM => "Out of memory.".into(),
        Shell::SE_ERR_DLLNOTFOUND => "Dynamic-link library not found.".into(),
        Shell::SE_ERR_SHARE => "Cannot share an open file.".into(),
        Shell::SE_ERR_ASSOCINCOMPLETE => "File association information not complete.".into(),
        Shell::SE_ERR_DDETIMEOUT => "DDE operation timed out.".into(),
        Shell::SE_ERR_DDEFAIL => "DDE operation failed.".into(),
        Shell::SE_ERR_DDEBUSY => "DDE operation is busy.".into(),
        Shell::SE_ERR_NOASSOC => "File association not available.".into(),
        _ => format!("Unexpected SE_ERR_* code: {}", err),
    }
}
