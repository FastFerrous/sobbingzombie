use crate::{FileOpsErrors, MAX_PATH_LEN};
use rustix::fs::{AtFlags, CWD, Stat, statat};
use std::{
    collections::HashMap,
    fs::{self, File},
};

struct StatContents {
    dev: u64,
    inode: u64,
    nlinks: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    /*
     * String variants of resolved uid & gid follow above fields -- left out of structure due to variant length and sizeof ops
     * user_len: u8
     * username
     * group_len: u8
     * group
     */
    size: u64,
    io_block: u64,
    blocks: u64,
    access: u64,
    access_nsec: u64,
    modify: u64,
    modify_nsec: u64,
    change: u64,
    change_nsec: u64,
}

pub fn stat_file(args: &[u8]) -> Result<Vec<u8>, FileOpsErrors> {
    let Some(path) = parse_args(args) else {
        return Err(FileOpsErrors::InvalidArguments);
    };

    let metadata: Stat = statat(CWD, &path, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| FileOpsErrors::UnableToEnumerate)?;

    let user_db = enumerate_users()?;
    let group_db = enumerate_groups()?;

    pack_stat(metadata, user_db, group_db)
}

fn parse_args(args: &[u8]) -> Option<String> {
    /*
     * u16: path length
     * <len>: path
     */
    if args.len() < size_of::<u16>() {
        return None;
    }

    let path_len = u16::from_be_bytes(args[..size_of::<u16>()].try_into().ok()?) as usize;
    if size_of::<u16>() + path_len != args.len() || path_len > MAX_PATH_LEN {
        return None;
    }

    let path_slice = &args[size_of::<u16>()..size_of::<u16>() + path_len];
    let mut path: Vec<u8> = Vec::new();
    if path.try_reserve(path_slice.len()).is_err() {
        return None;
    }

    path.extend_from_slice(path_slice);

    String::from_utf8(path).ok()
}

fn enumerate_users() -> Result<HashMap<u32, String>, FileOpsErrors> {
    let Ok(file_contents) = fs::read_to_string("/etc/passwd") else {
        return Err(FileOpsErrors::UnableToOpenFile);
    };

    let user_db: HashMap<u32, String> = file_contents
        .lines()
        .filter_map(|line: &str| {
            let mut fields = line.split(':');
            let username = fields.next()?;
            let _ = fields.next()?;
            let uid: u32 = fields.next()?.parse().ok()?;
            Some((uid, username.to_string()))
        })
        .collect();

    Ok(user_db)
}

fn enumerate_groups() -> Result<HashMap<u32, String>, FileOpsErrors> {
    let Ok(file_contents) = fs::read_to_string("/etc/group") else {
        return Err(FileOpsErrors::UnableToOpenFile);
    };

    let user_db: HashMap<u32, String> = file_contents
        .lines()
        .filter_map(|line: &str| {
            let mut fields = line.split(':');
            let group = fields.next()?;
            let _ = fields.next()?;
            let gid: u32 = fields.next()?.parse().ok()?;
            Some((gid, group.to_string()))
        })
        .collect();

    Ok(user_db)
}

fn pack_stat(
    metadata: Stat,
    users: HashMap<u32, String>,
    groups: HashMap<u32, String>,
) -> Result<Vec<u8>, FileOpsErrors> {
    let user: String = users
        .get(&metadata.st_uid)
        .cloned()
        .unwrap_or_else(|| metadata.st_uid.to_string());

    let group: String = groups
        .get(&metadata.st_gid)
        .cloned()
        .unwrap_or_else(|| metadata.st_gid.to_string());

    let total_size = size_of::<StatContents>() + user.len() + group.len() + (size_of::<u8>() * 2);

    let mut buffer = Vec::new();
    if buffer.try_reserve(total_size).is_err() {
        return Err(FileOpsErrors::Critical);
    }

    buffer.extend_from_slice(&total_size.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_dev.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_ino.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_nlink.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_mode.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_uid.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_gid.to_be_bytes());
    buffer.extend_from_slice(&(user.len() as u8).to_be_bytes());
    buffer.extend_from_slice(user.as_bytes());
    buffer.extend_from_slice(&(group.len() as u8).to_be_bytes());
    buffer.extend_from_slice(group.as_bytes());
    buffer.extend_from_slice(&metadata.st_size.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_blksize.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_blocks.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_atime.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_atime_nsec.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_mtime.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_mtime_nsec.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_ctime.to_be_bytes());
    buffer.extend_from_slice(&metadata.st_ctime_nsec.to_be_bytes());

    Ok(buffer)
}
