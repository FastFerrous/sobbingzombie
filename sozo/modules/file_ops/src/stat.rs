use crate::{FileOpsErrors, MAX_PATH_LEN};
use rustix::fs::{AtFlags, CWD, Stat, statat};

struct StatContents {
    /* device, inode, number of links */
    dev: u64,
    inode: u64,
    nlinks: u64,

    /* filetype + permissions */
    mode: u32,

    /* user and group names -- fallback to uid/gid values if not present */
    uid: u32,
    gid: u32,

    /* file size logically and physically */
    size: u64,
    io_block: u64,
    blocks: u64,

    /* timestamps */
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

    let stat_contents = StatContents {
        dev: metadata.st_dev,
        inode: metadata.st_ino,
        nlinks: metadata.st_nlink,
        mode: metadata.st_mode,
        uid: metadata.st_uid,
        gid: metadata.st_gid,
        size: metadata.st_size as u64,
        io_block: metadata.st_blksize as u64,
        blocks: metadata.st_blocks as u64,
        access: metadata.st_atime as u64,
        access_nsec: metadata.st_atime_nsec,
        modify: metadata.st_mtime as u64,
        modify_nsec: metadata.st_mtime_nsec,
        change: metadata.st_ctime as u64,
        change_nsec: metadata.st_ctime_nsec,
    };

    pack_stat(stat_contents)
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

fn pack_stat(stat_contents: StatContents) -> Result<Vec<u8>, FileOpsErrors> {
    let mut buffer: Vec<u8> = Vec::new();
    if buffer.try_reserve(size_of::<StatContents>()).is_err() {
        return Err(FileOpsErrors::Critical);
    }

    Ok(Vec::new())
}

// resolve user and group -- fallback to str id if not available
// get actual errors -- current placeholders
// pack the response
// make it look like stat
