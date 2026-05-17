use super::shell::ShellError;
use std::collections::HashMap;
use std::fs::{read, read_dir, read_link, read_to_string};

#[derive(Default)]
struct Process {
    user: String,
    pid: u32,
    ppid: u32,
    stime: u64,
    tty: String,
    exe: String,
}

pub struct Tasklist {
    ticks_per_second: u64,
    boot_time: u64,
    passwd_db: HashMap<u32, String>,
}

impl Tasklist {
    pub fn new() -> Option<Tasklist> {
        Some(Tasklist {
            ticks_per_second: unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64,
            boot_time: Self::get_boot_time()?,
            passwd_db: Self::get_passwd_map()?,
        })
    }

    pub fn get_snapshot(&self) -> Result<Vec<u8>, ShellError> {
        let mut entries: Vec<Process> = Vec::new();

        let dir_entries = read_dir("/proc").map_err(|_| ShellError::UnableToOpenDir)?;
        for entry in dir_entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };

            let Some(process) = self.process_from_pid(pid) else {
                continue;
            };

            if entries.try_reserve(1).is_err() {
                return Err(ShellError::Critical);
            }

            entries.push(process);
        }

        Self::pack_snapshot(&entries)
    }

    fn get_boot_time() -> Option<u64> {
        let contents = read_to_string("/proc/stat").ok()?;
        contents
            .lines()
            .find(|l| l.starts_with("btime"))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
    }

    fn get_passwd_map() -> Option<HashMap<u32, String>> {
        let contents = read_to_string("/etc/passwd").ok()?;
        Some(
            contents
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split(':');
                    let username = fields.next()?.to_string();
                    let _ = fields.next();
                    let uid = fields.next()?.parse::<u32>().ok()?;
                    Some((uid, username))
                })
                .collect(),
        )
    }

    fn process_from_pid(&self, pid: u32) -> Option<Process> {
        let status = read(&format!("/proc/{}/status", pid)).ok()?;
        let stat = read(&format!("/proc/{}/stat", pid)).ok()?;
        let cmdline = read(&format!("/proc/{}/cmdline", pid)).ok()?;
        let tty = read_link(&format!("/proc/{}/fd/0", pid))
            .map(|p| {
                let s = p.to_string_lossy();
                if s.starts_with("/dev/pts/")
                    || s.starts_with("/dev/pty/")
                    || s.starts_with("/dev/tty")
                {
                    s.into_owned()
                } else {
                    "?".to_string()
                }
            })
            .unwrap_or_else(|_| "?".to_string());

        let (ppid, uid) = Self::parse_status(&status)?;
        let stime = Self::parse_stat(&stat)?;
        let exe = Self::parse_cmdline(&cmdline, &stat)?;

        Some(Process {
            user: self
                .passwd_db
                .get(&uid)
                .cloned()
                .unwrap_or_else(|| uid.to_string()),
            pid,
            ppid,
            stime: self.boot_time + (stime / self.ticks_per_second),
            tty,
            exe,
        })
    }

    fn parse_status(contents: &[u8]) -> Option<(u32, u32)> {
        /* Converting u8 slice into a string reference so that we are able to convert into a dictionary for extracting required fields */
        let contents = str::from_utf8(contents).ok()?;
        let map: HashMap<&str, &str> = contents
            .lines()
            .filter_map(|line| line.split_once(':'))
            .collect();

        let ppid = map
            .get("PPid")
            .and_then(|ppid| ppid.trim().parse::<u32>().ok())?;

        let uid = map
            .get("Uid")
            .and_then(|uid| uid.trim().split_whitespace().next())?
            .parse::<u32>()
            .ok()?;

        Some((ppid, uid))
    }

    fn parse_stat(contents: &[u8]) -> Option<u64> {
        const STIME_FIELD: usize = 21;

        let contents = str::from_utf8(contents).ok()?;
        contents
            .split_ascii_whitespace()
            .nth(STIME_FIELD)
            .and_then(|v| v.parse::<u64>().ok())
    }

    fn parse_cmdline(cmdline_contents: &[u8], stat_contents: &[u8]) -> Option<String> {
        const MAX_CMDLINE_LEN: usize = 255;

        if cmdline_contents.is_empty() {
            let stat = str::from_utf8(stat_contents).ok()?;
            let name = stat.split_ascii_whitespace().nth(1)?;
            let s = format!("[{}]", name.trim_matches(|c| c == '(' || c == ')'));
            return Some(s.chars().take(MAX_CMDLINE_LEN).collect());
        }

        let cmdline = cmdline_contents
            .split(|&b| b == 0)
            .filter_map(|arg| str::from_utf8(arg).ok())
            .collect::<Vec<&str>>()
            .join(" ");

        Some(cmdline.chars().take(MAX_CMDLINE_LEN).collect())
    }

    fn pack_snapshot(process_entries: &Vec<Process>) -> Result<Vec<u8>, ShellError> {
        const PROCESS_PACKED_HDR_SIZE: usize = 19;

        if process_entries.is_empty() {
            return Err(ShellError::Critical);
        }

        /* declare buffer and reserve first four bytes for total size */
        let mut buffer: Vec<u8> = Vec::new();
        if buffer.try_reserve(size_of::<u32>()).is_err() {
            return Err(ShellError::Critical);
        }

        buffer.extend_from_slice(&0u32.to_be_bytes());

        /* iterate through vector and pack entries into the buffer that is returned to Shell module */
        for process in process_entries {
            let total_variable_strings_len =
                process.user.len() + process.tty.len() + process.exe.len();

            if buffer
                .try_reserve(PROCESS_PACKED_HDR_SIZE + total_variable_strings_len)
                .is_err()
            {
                return Err(ShellError::Critical);
            }

            buffer.extend_from_slice(&process.pid.to_be_bytes());
            buffer.extend_from_slice(&process.ppid.to_be_bytes());
            buffer.extend_from_slice(&process.stime.to_be_bytes());
            buffer.extend_from_slice(&u8::to_be_bytes(process.user.len() as u8));
            buffer.extend_from_slice(&u8::to_be_bytes(process.tty.len() as u8));
            buffer.extend_from_slice(&u8::to_be_bytes(process.exe.len() as u8));
            buffer.extend_from_slice(process.user.as_bytes());
            buffer.extend_from_slice(process.tty.as_bytes());
            buffer.extend_from_slice(process.exe.as_bytes());
        }

        let total_size = buffer.len() as u32;
        buffer[..4].copy_from_slice(&total_size.to_be_bytes());

        Ok(buffer)
    }
}
