use super::shell::ShellError;
use crate::sozo_debug;
use std::collections::HashMap;
use std::fs::Metadata;
use std::fs::{read_dir, read_link, read_to_string, symlink_metadata};
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;

#[derive(Default)]
struct DirEntry {
    permissions: u32,
    inode: u64,
    link_count: u64,
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
    pub fn get_listing(args: Vec<u8>) -> Result<Vec<u8>, ShellError> {
        let Some(path) = Self::parse_args(args) else {
            return Err(ShellError::InvalidArguments);
        };

        let Some(passwd_db) = Self::get_passwd_map() else {
            return Err(ShellError::Critical);
        };

        let Some(group_db) = Self::get_group_map() else {
            return Err(ShellError::Critical);
        };

        let metadata = match symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) => match e.kind() {
                ErrorKind::PermissionDenied => return Err(ShellError::PermissionDenied),
                ErrorKind::NotFound => return Err(ShellError::PathNotFound),
                _ => return Err(ShellError::Unknown),
            },
        };

        let entries = if metadata.is_dir() {
            Self::parse_dir(path, metadata, &passwd_db, &group_db)?
        } else {
            match Self::parse_file(path, metadata, &passwd_db, &group_db) {
                Some(entry) => {
                    let mut v: Vec<DirEntry> = Vec::new();
                    if v.try_reserve(1).is_err() {
                        return Err(ShellError::Critical);
                    }
                    v.push(entry);
                    v
                }
                None => return Err(ShellError::Critical),
            }
        };

        Self::pack_entries(entries)
    }

    fn parse_args(args: Vec<u8>) -> Option<String> {
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

    fn get_passwd_map() -> Option<HashMap<u32, String>> {
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

    fn get_group_map() -> Option<HashMap<u32, String>> {
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

    fn parse_file(
        path: String,
        file_data: Metadata,
        passwd_db: &HashMap<u32, String>,
        group_db: &HashMap<u32, String>,
    ) -> Option<DirEntry> {
        let link = if file_data.is_symlink() {
            read_link(&path)
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
        } else {
            None
        };

        let user = passwd_db
            .get(&file_data.uid())
            .cloned()
            .unwrap_or_else(|| file_data.uid().to_string());

        let group = group_db
            .get(&file_data.gid())
            .cloned()
            .unwrap_or_else(|| file_data.gid().to_string());

        Some(DirEntry {
            permissions: file_data.mode(),
            inode: file_data.ino(),
            link_count: file_data.nlink(),
            user,
            group,
            size: file_data.size(),
            mtime: file_data.mtime() as u64,
            ctime: file_data.ctime() as u64,
            filename: path,
            link,
        })
    }

    fn parse_dir(
        path: String,
        directory_data: Metadata,
        passwd_db: &HashMap<u32, String>,
        group_db: &HashMap<u32, String>,
    ) -> Result<Vec<DirEntry>, ShellError> {
        /* declare entries vector for storing all process directory entries -- first entry will also be the supplied directory */
        let mut entries: Vec<DirEntry> = Vec::new();

        let Some(dir_entry) = Self::parse_file(path.clone(), directory_data, &passwd_db, &group_db)
        else {
            return Err(ShellError::Critical);
        };

        if entries.try_reserve(1).is_err() {
            return Err(ShellError::Critical);
        }

        entries.push(dir_entry);

        /* attempt to open the specified directory and begin parsing child items */
        let open_dir = match read_dir(&path) {
            Ok(dir) => dir,
            Err(e) => match e.kind() {
                ErrorKind::PermissionDenied => return Err(ShellError::PermissionDenied),
                ErrorKind::NotFound => return Err(ShellError::PathNotFound),
                _ => return Err(ShellError::Unknown),
            },
        };

        for entry in open_dir.flatten() {
            let Ok(metadata) = symlink_metadata(entry.path()) else {
                continue;
            };

            let str_path = entry
                .path()
                .to_str()
                .ok_or(ShellError::Critical)?
                .to_string();

            let Some(dir_entry) = Self::parse_file(str_path, metadata, passwd_db, group_db) else {
                return Err(ShellError::Critical);
            };

            if entries.try_reserve(1).is_err() {
                return Err(ShellError::Critical);
            }

            entries.push(dir_entry);
        }

        Ok(entries)
    }

    fn pack_entries(entries: Vec<DirEntry>) -> Result<Vec<u8>, ShellError> {
        /*
         * permissions  : u32
         * inode        : u64
         * link_count   : u64
         * user_len     : u8
         * group_len    : u8
         * size         : u64
         * mtime        : u64
         * ctime        : u64
         * fn_len       : u16
         * link_len     : u16
         */
        const FIXED_DIRENTRY_HDR_LEN: usize = 50;

        let mut buffer: Vec<u8> = Vec::new();
        if buffer.try_reserve(size_of::<u32>()).is_err() {
            return Err(ShellError::Critical);
        }

        buffer.extend_from_slice(&0u32.to_be_bytes());

        for entry in entries {
            let link_len = entry.link.as_ref().map(|s| s.len()).unwrap_or(0);
            let variable_len =
                entry.filename.len() + entry.group.len() + entry.user.len() + link_len;

            if buffer
                .try_reserve(FIXED_DIRENTRY_HDR_LEN + variable_len)
                .is_err()
            {
                return Err(ShellError::Critical);
            }

            /* header data */
            buffer.extend_from_slice(&entry.permissions.to_be_bytes());
            buffer.extend_from_slice(&entry.inode.to_be_bytes());
            buffer.extend_from_slice(&entry.link_count.to_be_bytes());
            buffer.extend_from_slice(&(entry.user.len() as u8).to_be_bytes());
            buffer.extend_from_slice(&(entry.group.len() as u8).to_be_bytes());
            buffer.extend_from_slice(&entry.size.to_be_bytes());
            buffer.extend_from_slice(&entry.mtime.to_be_bytes());
            buffer.extend_from_slice(&entry.ctime.to_be_bytes());
            buffer.extend_from_slice(&(entry.filename.len() as u16).to_be_bytes());
            buffer.extend_from_slice(&(link_len as u16).to_be_bytes());

            /* variable data */
            buffer.extend_from_slice(entry.user.as_bytes());
            buffer.extend_from_slice(entry.group.as_bytes());
            buffer.extend_from_slice(entry.filename.as_bytes());

            if let Some(link) = entry.link {
                buffer.extend_from_slice(link.as_bytes());
            }
        }

        let total_size = buffer.len() as u32;
        buffer[..size_of::<u32>()].copy_from_slice(&total_size.to_be_bytes());

        Ok(buffer)
    }
}
