use super::shell::ShellError;
use crate::sozo_debug;
use std::collections::HashMap;
use std::fs::{read_dir, read_link, read_to_string, symlink_metadata};
use std::io::ErrorKind;

#[derive(Default)]
struct DirEntry {
    permissions: u32,
    inode: u64,
    link_count: u32,
    user: String,
    group: String,
    size: u64,
    mtime: u64,
    ctime: u64,
    filename: String,
    link: Option<String>,
}

pub struct DirWalker {}

impl DirWalker {
    pub fn new() -> DirWalker {
        DirWalker {}
    }

    pub fn get_listing(&self, args: Vec<u8>) -> Result<Vec<u8>, ShellError> {
        let Some(path) = self.parse_args(args) else {
            return Err(ShellError::InvalidArguments);
        };

        let metadata = match symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(e) => match e.kind() {
                ErrorKind::PermissionDenied => return Err(ShellError::PermissionDenied),
                ErrorKind::NotFound => return Err(ShellError::PathNotFound),
                _ => return Err(ShellError::Unknown),
            },
        };

        Ok(Vec::new())
    }

    fn parse_args(&self, args: Vec<u8>) -> Option<String> {
        if args.len() < size_of::<u16>() {
            return None;
        }

        let path_len = u16::from_be_bytes(args[..size_of::<u16>()].try_into().ok()?);

        if args.len() != path_len as usize + size_of::<u16>() {
            return None;
        }

        Some(
            str::from_utf8(&args[size_of::<u16>()..size_of::<u16>() + path_len as usize])
                .ok()?
                .to_string(),
        )
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

    fn get_group_map(&self) -> Option<HashMap<u32, String>> {
        let contents = read_to_string("/etc/group").ok()?;
        Some(
            contents
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split(':');
                    let group_name = fields.next()?.to_string();
                    let _ = fields.next()?;
                    let uid = fields.next()?.parse::<u32>().ok()?;
                    Some((uid, group_name))
                })
                .collect(),
        )
    }
}

// parse_file function -- handles symlink or regular file and appends to the vector
// parse_dir function -- parses entire directory and calls teh parse file function
// continues to append to a vector within self
// or each entry can just return option<vec<u8>>
