use crate::{FileOpsErrors, MAX_PATH_LEN, PathArgs};
use rustix::fs::{
    AtFlags, CWD, Gid, Mode, OFlags, RenameFlags, Timespec, Timestamps, Uid, fchmod, fchown, fsync,
    futimens, openat, renameat_with, unlinkat,
};
use rustix::io::{Errno, read, write};
use std::fs::{Metadata, symlink_metadata};
use std::io::{Error, ErrorKind};
use std::os::unix::fs::MetadataExt;

pub fn move_file(args: &[u8]) -> Result<Vec<u8>, FileOpsErrors> {
    let Some(args) = parse_args(args) else {
        return Err(FileOpsErrors::InvalidArguments);
    };

    /* validate that source is a regular file -- not following symlink */
    let source_metadata: Metadata = symlink_metadata(&args.src).map_err(Error::from)?;
    if !source_metadata.is_file() {
        return Err(FileOpsErrors::NotRegularFile);
    }

    /*
     * call initial `renameat`` to determine whether destination file exists, cross filesystem move, etc.
     * using noreplace to prevent destination clobbering; however, if successful then operation was complete
     */
    match renameat_with(CWD, &args.src, CWD, &args.dst, RenameFlags::NOREPLACE) {
        Ok(()) => {}
        Err(err) => match err.kind() {
            ErrorKind::NotFound => return Err(FileOpsErrors::PathNotFound),
            ErrorKind::PermissionDenied => return Err(FileOpsErrors::PermissionDenied),
            ErrorKind::IsADirectory => return Err(FileOpsErrors::NotRegularFile),
            ErrorKind::AlreadyExists => handle_eexist_error(&args)?,
            ErrorKind::CrossesDevices => handle_exdev_error(&args, source_metadata)?,
            _ => return Err(FileOpsErrors::Unknown),
        },
    }

    Ok(Vec::new())
}

fn parse_args(args: &[u8]) -> Option<PathArgs> {
    /*
     * u16: source path len
     * u16: destination path len
     * <var>: souce path
     * <var>: destination path
     */
    if args.len() < size_of::<u32>() {
        return None;
    }

    let mut index: usize = 0;
    let spath_len =
        u16::from_be_bytes(args[index..index + size_of::<u16>()].try_into().ok()?) as usize;
    index += size_of::<u16>();

    let dpath_len =
        u16::from_be_bytes(args[index..index + size_of::<u16>()].try_into().ok()?) as usize;
    index += size_of::<u16>();

    if size_of::<u32>() + spath_len + dpath_len != args.len()
        || spath_len > MAX_PATH_LEN
        || dpath_len > MAX_PATH_LEN
    {
        return None;
    }

    let source_slice = &args[index..index + spath_len];
    index += spath_len;

    let dest_slice = &args[index..index + dpath_len];

    let mut source: Vec<u8> = Vec::new();
    if source.try_reserve(spath_len).is_err() {
        return None;
    }

    let mut destination: Vec<u8> = Vec::new();
    if destination.try_reserve(dpath_len).is_err() {
        return None;
    }

    source.extend_from_slice(source_slice);
    destination.extend_from_slice(dest_slice);

    Some(PathArgs {
        src: String::from_utf8(source).ok()?,
        dst: String::from_utf8(destination).ok()?,
    })
}

fn handle_eexist_error(args: &PathArgs) -> Result<(), FileOpsErrors> {
    /* destination exists -- validating that destination is a regular file */
    let metadata = symlink_metadata(&args.dst).map_err(|_| FileOpsErrors::UnableToEnumerate)?;
    if !metadata.is_file() {
        return Err(FileOpsErrors::NotRegularFile);
    }

    renameat_with(CWD, &args.src, CWD, &args.dst, RenameFlags::empty())
        .map(|_| Ok(()))
        .map_err(Error::from)?
}

fn handle_exdev_error(args: &PathArgs, source_metadata: Metadata) -> Result<(), FileOpsErrors> {
    /*
     * falling back to copy operation due destination file crossing file systems
     * checking whether destination file exists and if so, whether it is a non regular file
     */
    match symlink_metadata(&args.dst) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(FileOpsErrors::NotRegularFile);
            }

            unlinkat(CWD, &args.dst, AtFlags::empty())
                .map_err(|_| FileOpsErrors::UnableToRemove)?;
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(_) => return Err(FileOpsErrors::UnableToEnumerate),
    };

    /*
     * opening source and destination to perform buffered io read/write
     * once transfer has been completed, copy metadata from source prior to unlink
     */
    let src_fd = openat(
        CWD,
        &args.src,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(Error::from)?;

    let dst_fd = openat(
        CWD,
        &args.dst,
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC | OFlags::EXCL | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(Error::from)?;

    // copy file contents function that performs the below io

    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve(128 * 1024).is_err() {
        return Err(FileOpsErrors::Critical);
    }

    buf.resize(128 * 1024, 0);

    loop {
        let bytes_read = match read(&src_fd, &mut buf) {
            Ok(0) => break,
            Ok(bytes_read) => bytes_read,
            Err(Errno::INTR) => continue,
            Err(_) => return Err(FileOpsErrors::ReadError),
        };

        let mut bytes_written: usize = 0 as usize;
        while bytes_written < bytes_read {
            match write(&dst_fd, &buf[bytes_written..bytes_read]) {
                Ok(wrote) => bytes_written += wrote,
                Err(Errno::INTR) => continue,
                Err(_) => return Err(FileOpsErrors::WriteError),
            }
        }
    }

    // copy metadata function -- takes in the dst file descriptor and source metadata
    let timestamps: Timestamps = Timestamps {
        last_access: Timespec {
            tv_sec: source_metadata.atime() as _,
            tv_nsec: source_metadata.atime_nsec() as _,
        },
        last_modification: Timespec {
            tv_sec: source_metadata.mtime() as _,
            tv_nsec: source_metadata.mtime_nsec() as _,
        },
    };

    futimens(&dst_fd, &timestamps).map_err(|_| FileOpsErrors::Unknown)?; // return actual value -- placeholder
    fchmod(
        &dst_fd,
        Mode::from_bits_truncate(source_metadata.mode() & 0o7777),
    )
    .map_err(|_| FileOpsErrors::Unknown)?;

    fchown(
        &dst_fd,
        Some(Uid::from_raw(source_metadata.uid())),
        Some(Gid::from_raw(source_metadata.gid())),
    )
    .map_err(|_| FileOpsErrors::Unknown)?;

    fsync(&dst_fd).map_err(|_| FileOpsErrors::Unknown)?; // temp placeholder retunr vlaue
    unlinkat(CWD, &args.src, AtFlags::empty()).map_err(|_| FileOpsErrors::UnableToRemove)?;
    // better cleanup -- possibly use a temporary dest file that then gets overwritten by the renameat to move to new file
    // can use raii on temp file so its always cleaned and deleted
    // this will ensure that upon any failure temp file gets deleted and no changes are made to the destination file yet

    // update the vector to use a macro for the size

    // possibly fstatat the src_fd to have a fresh copy

    Ok(())
}
