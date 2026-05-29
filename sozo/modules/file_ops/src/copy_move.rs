use crate::{FileOpsErrors, MAX_PATH_LEN};
use rustix::fs::{
    AtFlags, CWD, FileType, Gid, Mode, OFlags, RenameFlags, SeekFrom, Timespec, Timestamps, Uid,
    fchmod, fchown, fstat, fsync, ftruncate, futimens, openat, renameat_with, seek, unlinkat,
};
use rustix::io::{Errno, pread, pwrite};
use std::fs::{Metadata, symlink_metadata};
use std::io::{Error, ErrorKind};
use std::os::fd::OwnedFd;

struct PathArgs {
    src: String,
    dst: String,
}

/* both copy and mv are sparse file aware and ensure that no holes are interpreted as raw values, ballooning files on disk */

/* this impl of mv does not preserve xattrs or acls -- can be changed to include these at a later time */
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
            ErrorKind::CrossesDevices => handle_exdev_error(&args)?,
            _ => return Err(FileOpsErrors::Unknown),
        },
    }

    Ok(Vec::new())
}

pub fn copy_file(args: &[u8]) -> Result<Vec<u8>, FileOpsErrors> {
    let Some(args) = parse_args(args) else {
        return Err(FileOpsErrors::InvalidArguments);
    };

    let src: OwnedFd = openat(
        CWD,
        &args.src,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(Error::from)?;

    match fstat(&src) {
        Ok(metadata) => {
            if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
                return Err(FileOpsErrors::NotRegularFile);
            }
        }
        Err(_) => return Err(FileOpsErrors::UnableToEnumerate),
    };

    let dst: OwnedFd = match openat(
        CWD,
        &args.dst,
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC | OFlags::EXCL | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
    ) {
        Ok(fd) => fd,
        Err(Errno::EXIST) => {
            let metadata =
                symlink_metadata(&args.dst).map_err(|_| FileOpsErrors::UnableToEnumerate)?;
            if !metadata.is_file() {
                return Err(FileOpsErrors::NotRegularFile);
            }

            openat(
                CWD,
                &args.dst,
                OFlags::WRONLY | OFlags::CLOEXEC | OFlags::TRUNC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(Error::from)?
        }
        Err(err) => return Err(Error::from(err).into()),
    };

    buffered_io_copy(&src, &dst)?;

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

    renameat_with(CWD, &args.src, CWD, &args.dst, RenameFlags::empty()).map_err(Error::from)?;

    Ok(())
}

fn handle_exdev_error(args: &PathArgs) -> Result<(), FileOpsErrors> {
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
    let src_fd: OwnedFd = openat(
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

    buffered_io_copy(&src_fd, &dst_fd)?;

    copy_metadata(&src_fd, &dst_fd)?;

    unlinkat(CWD, &args.src, AtFlags::empty()).map_err(|_| FileOpsErrors::UnableToRemove)
}

fn buffered_io_copy(src: &OwnedFd, dst: &OwnedFd) -> Result<(), FileOpsErrors> {
    const READ_BUFFER_SIZE: usize = 64 * 1024;

    let mut buffer: Vec<u8> = Vec::new();
    if buffer.try_reserve(READ_BUFFER_SIZE).is_err() {
        return Err(FileOpsErrors::Critical);
    }

    buffer.resize(READ_BUFFER_SIZE, 0);

    /* get most up-to-date fstat on src to pre-trunc dst size so unwritten regions become holes */
    let Ok(metadata) = fstat(src) else {
        return Err(FileOpsErrors::UnableToEnumerate);
    };

    ftruncate(dst, metadata.st_size as u64).map_err(|_| FileOpsErrors::WriteError)?;

    let mut offset: u64 = 0u64;
    while offset < metadata.st_size as u64 {
        /* determine the start of legitimate data within the file */
        let data_start = match seek(src, SeekFrom::Data(offset)) {
            Ok(pos) => pos,
            Err(Errno::NXIO) => break, /* no remaining data or a hole exists until EOF */
            Err(_) => return Err(FileOpsErrors::ReadError),
        };

        /* determine the end of the legitimate data ie start of new hole */
        let data_end: u64 =
            seek(src, SeekFrom::Hole(data_start)).map_err(|_| FileOpsErrors::ReadError)?;

        /* set `position` to data start and loop until all data has been read and written into the correct position */
        let mut pos = data_start;
        while pos < data_end {
            let data_len = ((data_end - pos) as usize).min(READ_BUFFER_SIZE);
            let bytes_read = match pread(src, &mut buffer[..data_len], pos) {
                Ok(0) => break,
                Ok(bytes_read) => bytes_read,
                Err(Errno::INTR) => continue,
                Err(_) => return Err(FileOpsErrors::ReadError),
            };

            let mut total_written = 0;
            while total_written < bytes_read {
                match pwrite(
                    dst,
                    &buffer[total_written..bytes_read],
                    pos + total_written as u64,
                ) {
                    Ok(0) => return Err(FileOpsErrors::WriteError),
                    Ok(bytes_written) => total_written += bytes_written,
                    Err(Errno::INTR) => continue,
                    Err(_) => return Err(FileOpsErrors::WriteError),
                }
            }
            pos += bytes_read as u64;
        }

        /* set current file offset to start of `next` hole */
        offset = data_end;
    }
    Ok(())
}

fn copy_metadata(src: &OwnedFd, dst: &OwnedFd) -> Result<(), FileOpsErrors> {
    let Ok(metadata) = fstat(src) else {
        return Err(FileOpsErrors::UnableToEnumerate);
    };

    let timestamps: Timestamps = Timestamps {
        last_access: Timespec {
            tv_sec: metadata.st_atime as _,
            tv_nsec: metadata.st_atime_nsec as _,
        },
        last_modification: Timespec {
            tv_sec: metadata.st_mtime as _,
            tv_nsec: metadata.st_mtime_nsec as _,
        },
    };

    fchown(
        dst,
        Some(Uid::from_raw(metadata.st_uid)),
        Some(Gid::from_raw(metadata.st_gid)),
    )
    .map_err(|_| FileOpsErrors::UnableToApplyMetadata)?;

    fchmod(dst, Mode::from_bits_truncate(metadata.st_mode & 0o7777))
        .map_err(|_| FileOpsErrors::UnableToApplyMetadata)?;

    futimens(dst, &timestamps).map_err(|_| FileOpsErrors::UnableToApplyMetadata)?;

    fsync(dst).map_err(|_| FileOpsErrors::UnableToApplyMetadata)?;

    Ok(())
}
