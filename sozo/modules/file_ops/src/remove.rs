use crate::{FileOpsErrors, MAX_PATH_LEN};
use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat, openat, statat, unlinkat};
use std::ffi::CString;
use std::io;

struct RemoveArgs {
    dir: bool,
    path: CString,
}

pub fn remove_path(args: &[u8]) -> Result<Vec<u8>, FileOpsErrors> {
    let Some(args) = parse_args(args) else {
        return Err(FileOpsErrors::InvalidArguments);
    };

    let metadata: Stat =
        statat(CWD, &args.path, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;

    match FileType::from_raw_mode(metadata.st_mode) {
        FileType::Directory => {
            if !args.dir {
                return Err(FileOpsErrors::InvalidArguments);
            }

            remove_directory_contents(CWD, &args.path)?;
        }
        _ => unlinkat(CWD, &args.path, AtFlags::empty()).map_err(io::Error::from)?,
    };

    Ok(Vec::new())
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

    let flag = args[0];
    let dir_flag = bool::try_from(flag).ok()?;

    let mut index: usize = size_of::<u8>();
    let path_len =
        u16::from_be_bytes(args[index..index + size_of::<u16>()].try_into().ok()?) as usize;
    index += size_of::<u16>();

    if index + path_len != args.len() || path_len > MAX_PATH_LEN || path_len == 0 {
        return None;
    }

    let path_slice = &args[index..index + path_len];
    if path_slice == b"." || path_slice == b".." {
        return None;
    }

    let mut path: Vec<u8> = Vec::new();
    if path.try_reserve(path_slice.len()).is_err() {
        return None;
    }

    path.extend_from_slice(path_slice);

    Some(RemoveArgs {
        dir: dir_flag,
        path: CString::new(path).ok()?,
    })
}

fn remove_directory_contents(dir_fd: BorrowedFd<'_>, path: &CString) -> Result<(), FileOpsErrors> {
    let fd = openat(
        dir_fd,
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;

    let contents = read_directory_entries(fd.as_fd())?;

    for (filename, filetype) in contents {
        /*
         * d_type is not always populated in dirents structure -- mainly old platforms.
         * use statat to determine st_mode for follow on action
         */
        let filetype = match filetype {
            FileType::Unknown => {
                let metadata = statat(fd.as_fd(), &filename, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
                FileType::from_raw_mode(metadata.st_mode)
            }
            other => other,
        };

        match filetype {
            FileType::Directory => remove_directory_contents(fd.as_fd(), &filename)?,
            _ => {
                unlinkat(fd.as_fd(), &filename, AtFlags::empty()).map_err(io::Error::from)?;
            }
        }
    }

    unlinkat(dir_fd, path, AtFlags::REMOVEDIR).map_err(io::Error::from)?;

    Ok(())
}

fn read_directory_entries(fd: BorrowedFd<'_>) -> Result<Vec<(CString, FileType)>, FileOpsErrors> {
    let dir = Dir::read_from(fd).map_err(io::Error::from)?;
    let mut entries: Vec<(CString, FileType)> = Vec::new();

    for entry in dir {
        let Ok(entry) = entry else {
            return Err(FileOpsErrors::ReadError);
        };

        let file_name = entry.file_name();
        if file_name == c"." || file_name == c".." {
            continue;
        }

        let mut name: Vec<u8> = Vec::new();
        if name
            .try_reserve(file_name.to_bytes_with_nul().len())
            .is_err()
        {
            return Err(FileOpsErrors::Critical);
        }

        name.extend_from_slice(file_name.to_bytes_with_nul());

        if entries.try_reserve(1).is_err() {
            return Err(FileOpsErrors::Critical);
        }

        entries.push((
            CString::from_vec_with_nul(name).map_err(|_| FileOpsErrors::Critical)?,
            entry.file_type(),
        ))
    }

    Ok(entries)
}
