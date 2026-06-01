use crate::{FileOpsErrors, MAX_PATH_LEN};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

pub fn read_file_contents(args: &[u8]) -> Result<Vec<u8>, FileOpsErrors> {
    let Some(path) = parse_args(args) else {
        return Err(FileOpsErrors::InvalidArguments);
    };

    let mut file = File::open(&path)?;

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

    let mut buffer: Vec<u8> = Vec::new();
    if buffer
        .try_reserve(size_of::<u64>() + file_size as usize)
        .is_err()
    {
        return Err(FileOpsErrors::Critical);
    }

    buffer.extend_from_slice(&0u64.to_be_bytes());

    let Ok(bytes_read) = file.read_to_end(&mut buffer) else {
        return Err(FileOpsErrors::ReadError);
    };

    if metadata.size() > 0 && bytes_read != metadata.size() as usize {
        return Err(FileOpsErrors::ReadError);
    };

    buffer[..size_of::<u64>()].copy_from_slice(&(bytes_read as u64).to_be_bytes());

    Ok(buffer)
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
