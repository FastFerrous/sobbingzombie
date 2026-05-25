use crate::{FileOpsErrors, MAX_PATH_LEN};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use std::fs::{self, File};
use std::io;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;

struct PathArgs {
    src: String,
    dst: String,
}

pub fn copy_file(args: &[u8]) -> Result<Vec<u8>, FileOpsErrors> {
    let Some(args) = parse_args(args) else {
        return Err(FileOpsErrors::InvalidArguments);
    };

    let source_data = get_file_contents(&args.src)?;

    std::fs::write(&args.dst, source_data).map_err(|e| match e.kind() {
        ErrorKind::PermissionDenied => FileOpsErrors::PermissionDenied,
        ErrorKind::NotFound => FileOpsErrors::PathNotFound,
        _ => FileOpsErrors::Unknown,
    })?;

    Ok(Vec::new())
}

pub fn move_file(args: &[u8]) -> Result<Vec<u8>, FileOpsErrors> {
    let Some(args) = parse_args(args) else {
        return Err(FileOpsErrors::InvalidArguments);
    };

    /*
     * call initial `renameat`` to determine whether destination file exists, cross filesystem move, etc.
     * using noreplace to prevent destination clobbering; however, if successful then operation is complete
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

fn get_file_contents(path: &String) -> Result<Vec<u8>, FileOpsErrors> {
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) => match e.kind() {
            ErrorKind::NotFound => return Err(FileOpsErrors::PathNotFound),
            ErrorKind::PermissionDenied => return Err(FileOpsErrors::PermissionDenied),
            _ => return Err(FileOpsErrors::Unknown),
        },
    };

    let Ok(metadata) = file.metadata() else {
        return Err(FileOpsErrors::UnableToEnumerate);
    };

    if !metadata.is_file() {
        return Err(FileOpsErrors::NotRegularFile);
    }

    let file_size = match metadata.size() {
        0u64 => u16::MAX as u64,
        _ => metadata.size(),
    };

    let mut file_data: Vec<u8> = Vec::new();
    if file_data.try_reserve(file_size as usize).is_err() {
        return Err(FileOpsErrors::Critical);
    }

    let Ok(bytes_read) = file.read_to_end(&mut file_data) else {
        return Err(FileOpsErrors::ReadError);
    };

    if metadata.size() > 0 && bytes_read != metadata.size() as usize {
        return Err(FileOpsErrors::ReadError);
    };

    Ok(file_data)
}

fn handle_eexist_error(args: &PathArgs) -> Result<(), FileOpsErrors> {
    let metadata = fs::symlink_metadata(&args.dst).map_err(|_| FileOpsErrors::UnableToEnumerate)?;
    if metadata.is_dir() {
        return Err(FileOpsErrors::NotRegularFile);
    }

    renameat_with(CWD, &args.src, CWD, &args.dst, RenameFlags::empty())
        .map(|_| Ok(()))
        .map_err(io::Error::from)?
}

fn handle_exdev_error(args: &PathArgs) -> Result<(), FileOpsErrors> {
    // copy behavior
    // update all metadata from the src file
    Ok(())
}

// move /dev/shm /dev/shm/apples2 worked -- fix this
// same as inverse as above -- need to check for directories it seems

/*


// same fs
ioctl(0, TCGETS2, {c_iflag=ICRNL|IXON|IUTF8, c_oflag=NL0|CR0|TAB0|BS0|VT0|FF0|OPOST|ONLCR, c_cflag=B38400|CS8|CREAD, c_lflag=ISIG|ICANON|ECHO|ECHOE|ECHOK|IEXTEN|ECHOCTL|ECHOKE, ...}) = 0
renameat2(AT_FDCWD, "/dev/shm/apples", AT_FDCWD, "/dev/shm/apples2", RENAME_NOREPLACE) = -1 EEXIST (File exists)
openat(AT_FDCWD, "/dev/shm/apples2", O_RDONLY|O_PATH|O_DIRECTORY) = -1 ENOTDIR (Not a directory)
newfstatat(AT_FDCWD, "/dev/shm/apples", {st_mode=S_IFREG|0664, st_size=0, ...}, AT_SYMLINK_NOFOLLOW) = 0
newfstatat(AT_FDCWD, "/dev/shm/apples2", {st_mode=S_IFREG|0664, st_size=0, ...}, AT_SYMLINK_NOFOLLOW) = 0
geteuid()                               = 1000
faccessat2(AT_FDCWD, "/dev/shm/apples2", W_OK, AT_EACCESS) = 0
renameat(AT_FDCWD, "/dev/shm/apples", AT_FDCWD, "/dev/shm/apples2") = 0
close(0)

// cross fs
renameat2(AT_FDCWD, "passwd", AT_FDCWD, "/tmp/passwd", RENAME_NOREPLACE) = -1 EXDEV (Invalid cross-device link)
openat(AT_FDCWD, "/tmp/passwd", O_RDONLY|O_PATH|O_DIRECTORY) = -1 ENOTDIR (Not a directory)
newfstatat(AT_FDCWD, "passwd", {st_mode=S_IFREG|0664, st_size=2766, ...}, AT_SYMLINK_NOFOLLOW) = 0
newfstatat(AT_FDCWD, "/tmp/passwd", {st_mode=S_IFREG|0664, st_size=2766, ...}, AT_SYMLINK_NOFOLLOW) = 0
geteuid()                               = 1000
faccessat2(AT_FDCWD, "/tmp/passwd", W_OK, AT_EACCESS) = 0
renameat(AT_FDCWD, "passwd", AT_FDCWD, "/tmp/passwd") = -1 EXDEV (Invalid cross-device link)
unlinkat(AT_FDCWD, "/tmp/passwd", 0)    = 0
openat(AT_FDCWD, "passwd", O_RDONLY|O_NOFOLLOW) = 3
fstat(3, {st_mode=S_IFREG|0664, st_size=2766, ...}) = 0
openat(AT_FDCWD, "/tmp/passwd", O_WRONLY|O_CREAT|O_EXCL, 0600) = 4
ioctl(4, BTRFS_IOC_CLONE or FICLONE, 3) = -1 EXDEV (Invalid cross-device link)
fstat(4, {st_mode=S_IFREG|0600, st_size=0, ...}) = 0
fadvise64(3, 0, 0, POSIX_FADV_SEQUENTIAL) = 0
uname({sysname="Linux", nodename="ubuntu-dev", ...}) = 0
copy_file_range(3, NULL, 4, NULL, 9223372035781033984, 0) = -1 EXDEV (Invalid cross-device link)
mmap(NULL, 270336, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) = 0x71a17b512000
read(3, "root:x:0:0:root:/root:/bin/bash\n"..., 262144) = 2766
write(4, "root:x:0:0:root:/root:/bin/bash\n"..., 2766) = 2766
read(3, "", 262144)                     = 0
utimensat(4, NULL, [{tv_sec=1779721311, tv_nsec=871355210} /* 2026-05-25T11:01:51.871355210-0400 */, {tv_sec=1779720347, tv_nsec=434817158} /* 2026-05-25T10:45:47.434817158-0400 */], 0) = 0
flistxattr(3, NULL, 0)                  = 0
flistxattr(3, 0x7ffc273a4820, 0)        = 0
fchmod(4, 0100664)                      = 0
flistxattr(3, NULL, 0)                  = 0
flistxattr(3, 0x7ffc273a4810, 0)        = 0
close(4)                                = 0
close(3)                                = 0
munmap(0x71a17b512000, 270336)          = 0
newfstatat(AT_FDCWD, "/", {st_mode=S_IFDIR|0755, st_size=4096, ...}, AT_SYMLINK_NOFOLLOW) = 0
newfstatat(AT_FDCWD, "passwd", {st_mode=S_IFREG|0664, st_size=2766, ...}, AT_SYMLINK_NOFOLLOW) = 0
unlinkat(AT_FDCWD, "passwd", 0)         = 0

*/
