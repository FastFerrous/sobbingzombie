use super::shell::ShellError;
use crate::sozo_debug;
use std::collections::HashMap;
use std::fs::{read_dir, read_link, read_to_string};

#[repr(u8)]
enum Protocol {
    TCP = 0,
    UDP = 1,
    TCP6 = 2,
    UDP6 = 3,
}

#[derive(Default)]
struct Connection {
    protocol: u8,
    local_addr: [u8; 16],
    local_port: u16,
    remote_addr: [u8; 16],
    remote_port: u16,
    state: u8,
    pid: u32,
    exe: String,
    user: String,
}

pub struct Netstat {
    connections: Vec<Connection>,
    inode_map: HashMap<u64, u32>,
    password_db: HashMap<u32, String>,
}

impl Netstat {
    pub fn new() -> Netstat {
        Netstat {
            connections: Vec::new(),
            inode_map: HashMap::new(),
            password_db: HashMap::new(),
        }
    }

    pub fn get_connections(&mut self) -> Result<Vec<u8>, ShellError> {
        /*
         * Netstat is designed to be instantiated at each execution due to no requirements for `long living data`
         * however, if the struct is kept alive, the public function will ensure that each internal member is reset prior to
         * the next call
         */
        if !self.inode_map.is_empty()
            || !self.connections.is_empty()
            || !self.password_db.is_empty()
        {
            self.reset();
        }

        /* obtain map of socket indoes to owner process id's */
        if self.map_inodes_to_pids().is_err() {
            return Err(ShellError::Critical);
        }

        /* obtain map of uids to usernames */
        self.password_db = match self.get_passwd_map() {
            Some(db) => db,
            None => return Err(ShellError::Critical),
        };

        /* iterate through each net file within /proc and build out netstat */
        let network_files = [
            ("/proc/net/tcp", Protocol::TCP as u8),
            ("/proc/net/udp", Protocol::UDP as u8),
            ("/proc/net/tcp6", Protocol::TCP6 as u8),
            ("/proc/net/udp6", Protocol::UDP6 as u8),
        ];

        for (path, proto) in network_files {
            match self.parse_network_file(path, proto) {
                Ok(_) => continue,
                Err(ShellError::UnableToOpenFile) => {
                    sozo_debug!(
                        "netstat::get_connections",
                        "unable to open specified file -- {}",
                        path
                    );
                    continue;
                }
                Err(_) => {
                    sozo_debug!(
                        "netstat::get_connections",
                        "error while attempting to parse network file -- {}",
                        path
                    );
                    return Err(ShellError::Critical);
                }
            }
        }

        /* pack all connections into singular buffer */
        self.parse_connections()
    }

    fn parse_network_file(&mut self, path: &str, protocol: u8) -> Result<(), ShellError> {
        let file_data = match read_to_string(path) {
            Ok(s) => s,
            Err(_) => {
                return Err(ShellError::UnableToOpenFile);
            }
        };

        let line_iter = file_data.lines().skip(1);
        let line_count = line_iter.clone().count();

        let mut connections: Vec<Connection> = Vec::new();
        if connections.try_reserve(line_count).is_err() {
            return Err(ShellError::Critical);
        }

        for line in line_iter {
            let Some(connection) = self.parse_line(line, protocol) else {
                sozo_debug!(
                    "netstat::parse_network_file",
                    "error while attempting to parse {}",
                    line
                );
                return Err(ShellError::Critical);
            };

            connections.push(connection);
        }

        if self.connections.try_reserve(connections.len()).is_err() {
            return Err(ShellError::Critical);
        }

        self.connections.extend(connections);
        Ok(())
    }

    fn parse_line(&self, line: &str, protocol: u8) -> Option<Connection> {
        let iter = line.split_whitespace();
        let count = iter.clone().count();

        let mut fields: Vec<&str> = Vec::new();
        if fields.try_reserve(count).is_err() {
            return None;
        }

        fields.extend(iter);

        /* extract local and remote address fields to then split for `address : port` */
        let Some(local_addr) = fields.get(1).copied() else {
            return None;
        };
        let Some(remote_addr) = fields.get(2).copied() else {
            return None;
        };

        let (laddr, lport) = local_addr.split_once(':')?;
        let (raddr, rport) = remote_addr.split_once(':')?;

        let laddr = self.parse_net_addr(laddr)?;
        let raddr = self.parse_net_addr(raddr)?;

        let lport = u16::from_str_radix(lport, 16).ok()?;
        let rport = u16::from_str_radix(rport, 16).ok()?;

        /* extract uid to search through passwd db */
        let uid = fields
            .get(7)
            .and_then(|s| u32::from_str_radix(s, 10).ok())?;

        let user = self
            .password_db
            .get(&uid)
            .cloned()
            .unwrap_or_else(|| uid.to_string());

        /* convert state and extract inode for process identity search */
        let state = u8::from_str_radix(fields.get(3).copied()?, 16).ok()?;
        let inode = fields.get(9).copied()?.parse::<u64>().ok()?;

        let (pid, exe) = match self.inode_map.get(&inode).copied() {
            Some(pid) => {
                let cmd = read_to_string(format!("/proc/{}/stat", pid))
                    .ok()
                    .and_then(|s| {
                        let name = s.split_whitespace().nth(1)?;
                        Some(name.trim_matches(|c| c == '(' || c == ')').to_string())
                    })
                    .unwrap_or_else(|| "-".to_string());
                (pid, cmd)
            }
            None => (0, "-".to_string()),
        };

        Some(Connection {
            protocol,
            local_addr: laddr,
            local_port: lport,
            remote_addr: raddr,
            remote_port: rport,
            state,
            pid,
            exe,
            user,
        })
    }

    fn parse_net_addr(&self, addr: &str) -> Option<[u8; 16]> {
        let mut address = [0u8; 16];

        match addr.len() {
            8 => {
                let hex_addr = u32::from_str_radix(addr, 16).ok()?;
                address[..4].copy_from_slice(&hex_addr.to_be_bytes());
            }
            32 => {
                /* /proc/net/tcp6 or /proc/net/udp6 splits address into 4 words each in reverse order */
                const IPV6_WORD_COUNT: usize = 4;
                const IPV6_WORD_LEN: usize = 8;

                for i in 0..IPV6_WORD_COUNT {
                    let str_start = i * IPV6_WORD_LEN;
                    let arr_start = i * size_of::<u32>();

                    let word = &addr[str_start..str_start + IPV6_WORD_LEN];
                    let hex_addr = u32::from_str_radix(word, 16).ok()?;
                    address[arr_start..arr_start + size_of::<u32>()]
                        .copy_from_slice(&hex_addr.to_be_bytes());
                }
            }
            _ => return None,
        };

        Some(address)
    }

    fn map_inodes_to_pids(&mut self) -> Result<(), ShellError> {
        let dir_entries = read_dir("/proc").map_err(|_| ShellError::UnableToOpenDir)?;
        for entry in dir_entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };

            let fd_dir = match read_dir(format!("/proc/{}/fd", pid)) {
                Ok(dir) => dir,
                _ => continue,
            };
            for fd_entry in fd_dir.flatten() {
                let link = match read_link(fd_entry.path()) {
                    Ok(link) => link.to_string_lossy().into_owned(),
                    Err(_) => continue,
                };

                if !link.starts_with("socket:[") {
                    continue;
                }

                let inode = link
                    .strip_prefix("socket:[")
                    .ok_or(ShellError::Critical)?
                    .strip_suffix(']')
                    .ok_or(ShellError::Critical)?
                    .parse::<u64>()
                    .map_err(|_| ShellError::Critical)?;

                if self.inode_map.try_reserve(1).is_err() {
                    return Err(ShellError::Critical);
                }

                self.inode_map.insert(inode, pid);
            }
        }
        Ok(())
    }

    fn get_passwd_map(&self) -> Option<HashMap<u32, String>> {
        let contents = std::fs::read_to_string("/etc/passwd").ok()?;
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

    fn parse_connections(&mut self) -> Result<Vec<u8>, ShellError> {
        /*
         * protocol         - u8
         * local_addr len   - u8
         * local_port       - u16
         * remote_addr len  - u8
         * remote_port      - u16
         * state            - u8
         * pid              - u32
         * exe_len          - u8
         * user_len         - u8
         *
         *
         * variable data will be actual address lengths + executable path + username
         */
        const FIXED_CONNECTION_HDR_LEN: usize = 14;

        /* length fields include both local and remote values */
        const FIXED_IPV4_ADDRS_LEN: usize = 8;
        const FIXED_IPV6_ADDRS_LEN: usize = 32;

        let mut packed_buffer: Vec<u8> = Vec::new();
        if packed_buffer.try_reserve(size_of::<u32>()).is_err() {
            return Err(ShellError::Critical);
        }

        packed_buffer.extend_from_slice(&0u32.to_be_bytes());

        for connection in &self.connections {
            let (variable_len, addr_len) = match connection.protocol {
                0 | 1 => {
                    /* TCP or UDP */
                    (
                        connection.exe.len() + connection.user.len() + FIXED_IPV4_ADDRS_LEN,
                        FIXED_IPV4_ADDRS_LEN / size_of::<u16>(),
                    )
                }
                2 | 3 => {
                    /* TCP6 or UDP6 */
                    (
                        connection.exe.len() + connection.user.len() + FIXED_IPV6_ADDRS_LEN,
                        FIXED_IPV6_ADDRS_LEN / size_of::<u16>(),
                    )
                }
                _ => return Err(ShellError::Critical),
            };

            if packed_buffer
                .try_reserve(FIXED_CONNECTION_HDR_LEN + variable_len)
                .is_err()
            {
                return Err(ShellError::Critical);
            }

            packed_buffer.extend_from_slice(&connection.protocol.to_be_bytes());
            packed_buffer.extend_from_slice(&(addr_len as u8).to_be_bytes());
            packed_buffer.extend_from_slice(&connection.local_port.to_be_bytes());
            packed_buffer.extend_from_slice(&(addr_len as u8).to_be_bytes());
            packed_buffer.extend_from_slice(&connection.remote_port.to_be_bytes());
            packed_buffer.extend_from_slice(&connection.state.to_be_bytes());
            packed_buffer.extend_from_slice(&connection.pid.to_be_bytes());
            packed_buffer.extend_from_slice(&(connection.exe.len() as u8).to_be_bytes());
            packed_buffer.extend_from_slice(&(connection.user.len() as u8).to_be_bytes());
            packed_buffer.extend_from_slice(&connection.local_addr[..addr_len]);
            packed_buffer.extend_from_slice(&connection.remote_addr[..addr_len]);
            packed_buffer.extend_from_slice(connection.exe.as_bytes());
            packed_buffer.extend_from_slice(connection.user.as_bytes());
        }

        let total_size = packed_buffer.len() as u32;
        packed_buffer[..size_of::<u32>()].copy_from_slice(&total_size.to_be_bytes());

        self.reset();
        Ok(packed_buffer)
    }

    fn reset(&mut self) {
        self.connections = Vec::new();
        self.inode_map = HashMap::new();
        self.password_db = HashMap::new();
    }
}
