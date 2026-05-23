use crate::{FileOpsErrors, MAX_PATH_LEN, PathArgs};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;

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
    let spath_len = u16::from_be_bytes(args[index..size_of::<u16>()].try_into().ok()?) as usize;
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
