use crate::{FileOpsErrors, MAX_PATH_LEN};
use rustix::{
    fs::{AtFlags, CWD, FileType, Stat, statat, unlinkat},
    io::Errno,
};
use std::io;

struct RemoveArgs {
    dir: bool,
    path: String,
}

pub fn remove_path(args: &[u8]) -> Result<Vec<u8>, FileOpsErrors> {
    let Some(args) = parse_args(args) else {
        return Err(FileOpsErrors::InvalidArguments);
    };

    let metadata = get_metadata(&args.path)?;
    match FileType::from_raw_mode(metadata.st_mode) {
        FileType::Directory => {
            if !args.dir {
                return Err(FileOpsErrors::InvalidArguments);
            }
            // call into function where we attempt to perform recursion -- need to openat, etc.
            // use the safe functions unlinkat, fdopendir, and openat for fd recursion -- never paths
        }
        _ => unlinkat(CWD, &args.path, AtFlags::empty()).map_err(io::Error::from)?,
    };

    Ok(Vec::new())
}

fn get_metadata(path: &String) -> Result<Stat, FileOpsErrors> {
    match statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(stat),
        Err(Errno::ACCESS) => Err(FileOpsErrors::PermissionDenied),
        Err(Errno::NOENT) => Err(FileOpsErrors::PathNotFound),
        Err(_) => Err(FileOpsErrors::Unknown),
    }
}

fn parse_args(args: &[u8]) -> Option<RemoveArgs> {
    /*
     * u8: dir boolean
     * u16: path length
     * <len>: path
     */
    if args.len() < size_of::<u16>() + size_of::<u8>() {
        return None;
    }

    let flag = u8::from_be_bytes(args[..size_of::<u8>()].try_into().ok()?);
    let dir_flag = bool::try_from(flag).ok()?;

    let mut index: usize = size_of::<u8>();
    let path_len =
        u16::from_be_bytes(args[index..index + size_of::<u16>()].try_into().ok()?) as usize;
    index += size_of::<u16>();

    if index + path_len != args.len() || path_len > MAX_PATH_LEN {
        return None;
    }

    let path_slice = &args[index..index + path_len];
    let mut path: Vec<u8> = Vec::new();
    if path.try_reserve(path_slice.len()).is_err() {
        return None;
    }

    path.extend_from_slice(path_slice);

    Some(RemoveArgs {
        dir: dir_flag,
        path: String::from_utf8(path).ok()?,
    })
}
