// Imports
// -------

use std::{
    ffi::c_void,
    io,
    iter::once,
    os::windows::prelude::OsStrExt,
    path::{Path, PathBuf},
    ptr::null_mut,
};

use clap::{Parser, Subcommand};
use jwalk::WalkDir;
use parse_display::{Display, FromStr};
use winapi::um::{
    errhandlingapi::GetLastError,
    fileapi::{
        CreateFileW, SetFileInformationByHandle, SetFileTime, FILE_BASIC_INFO, OPEN_EXISTING,
    },
    handleapi::{CloseHandle, INVALID_HANDLE_VALUE},
    minwinbase::FileBasicInfo,
    winbase::GetFileInformationByHandleEx,
    winbase::FILE_FLAG_BACKUP_SEMANTICS,
    winnt::{FILE_SHARE_READ, HANDLE, LARGE_INTEGER},
};
use winapi::{
    shared::minwindef::FILETIME,
    um::winnt::{FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES},
};
// -------

// Shared Win32 wrappers
// ---------------------

unsafe fn make_large_integer(v: i64) -> LARGE_INTEGER {
    let mut ret: LARGE_INTEGER = std::mem::zeroed();
    *ret.QuadPart_mut() = v;
    ret
}

enum Win32OpenMode {
    Read,
    Write,
}

unsafe fn win32_open_file(path: &Path, mode: Win32OpenMode) -> HANDLE {
    let os_path: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    let access = match mode {
        // We also need to retain the last access time via SetFileTime() in
        // this case.
        Win32OpenMode::Read => FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,

        Win32OpenMode::Write => FILE_WRITE_ATTRIBUTES,
    };

    let handle = CreateFileW(
        os_path.as_ptr(),
        access,
        FILE_SHARE_READ,
        null_mut(),
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS,
        null_mut(),
    );
    if handle == INVALID_HANDLE_VALUE {
        eprintln!("{}: error opening: {}", path.display(), GetLastError());
        return handle;
    }

    // Leave last access time unchanged
    // https://devblogs.microsoft.com/oldnewthing/20111010-00/?p=9433
    let leave_unchanged = FILETIME {
        dwLowDateTime: u32::MAX,
        dwHighDateTime: u32::MAX,
    };
    if SetFileTime(handle, null_mut(), &leave_unchanged, null_mut()) == 0 {
        eprintln!(
            "{}: error leaving access time unchanged: {}",
            path.display(),
            GetLastError()
        );
    }
    handle
}

unsafe fn get_file_basic_info(path: &Path) -> Option<FILE_BASIC_INFO> {
    let mut ret: FILE_BASIC_INFO = std::mem::zeroed();

    let handle = win32_open_file(path, Win32OpenMode::Read);
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let valid = GetFileInformationByHandleEx(
        handle,
        FileBasicInfo,
        &mut ret as *mut _ as *mut c_void,
        std::mem::size_of::<FILE_BASIC_INFO>().try_into().unwrap(),
    );
    CloseHandle(handle);
    if valid == 0 {
        let err = GetLastError();
        eprintln!("{}: error retrieving timestamps: {}", path.display(), err);
        return None;
    }
    Some(ret)
}

unsafe fn set_file_basic_info(path: &Path, mut fi: FILE_BASIC_INFO) {
    let handle = win32_open_file(path, Win32OpenMode::Write);
    if handle == INVALID_HANDLE_VALUE {
        return;
    }

    let valid = SetFileInformationByHandle(
        handle,
        FileBasicInfo,
        &mut fi as *mut _ as *mut c_void,
        std::mem::size_of::<FILE_BASIC_INFO>().try_into().unwrap(),
    );
    CloseHandle(handle);
    if valid == 0 {
        let err = GetLastError();
        eprintln!("{}: error applying timestamps: {}", path.display(), err);
    }
}

// ---------------------

// Data structures
// ---------------

const HEADER_PREFIX: &str = "Version ";

trait Timestamps: std::fmt::Debug + std::fmt::Display + std::str::FromStr {
    fn version() -> i32;
    fn header() -> &'static str;
    fn get(path: &Path) -> Option<Self>;
    fn set(self, path: &Path);
}

#[derive(Display, FromStr, Debug)]
#[display("{created}\t{modified}\t{changed}\t{accessed}")]
struct V0Timestamps {
    created: i64,
    modified: i64,
    changed: i64,
    accessed: i64,
}

impl Timestamps for V0Timestamps {
    fn version() -> i32 {
        0
    }

    fn header() -> &'static str {
        "Created\tModified\tChanged\tAccessed"
    }

    fn get(path: &Path) -> Option<Self> {
        unsafe {
            get_file_basic_info(path).map(|fi| V0Timestamps {
                created: *fi.CreationTime.QuadPart(),
                modified: *fi.LastWriteTime.QuadPart(),
                accessed: *fi.LastAccessTime.QuadPart(),
                changed: *fi.ChangeTime.QuadPart(),
            })
        }
    }

    fn set(self, path: &Path) {
        unsafe {
            set_file_basic_info(
                path,
                FILE_BASIC_INFO {
                    CreationTime: make_large_integer(self.created),
                    LastAccessTime: make_large_integer(self.accessed),
                    LastWriteTime: make_large_integer(self.modified),
                    ChangeTime: make_large_integer(self.changed),
                    FileAttributes: 0, // keeps original attributes
                },
            )
        }
    }
}
// ---------------

// Top-level functions
// -------------------

fn column_header<V: Timestamps>() -> String {
    format!("Path\t{}", V::header())
}

fn dump<V: Timestamps>(root: &Path) {
    println!("{}{}", HEADER_PREFIX, V::version());
    println!("{}", column_header::<V>());

    for entry in WalkDir::new(root) {
        if let Err(err) = entry {
            eprintln!("{}", err);
            continue;
        }
        let entry = entry.unwrap();

        match V::get(&entry.path()) {
            None => continue,
            Some(ts) => println!("{}\t{}", entry.path().display(), ts),
        }
    }
}

fn apply<V: Timestamps, T: std::io::BufRead>(mut lines: std::io::Lines<T>)
where
    <V as std::str::FromStr>::Err: std::fmt::Debug,
{
    let column_header = column_header::<V>();
    assert_eq!(lines.next().unwrap().unwrap(), column_header);
    for line in lines {
        let line = line.unwrap();
        let (path, timestamps) = line.split_once('\t').unwrap();
        timestamps.parse::<V>().unwrap().set(Path::new(path));
    }
}

fn apply_any<T: std::io::BufRead>(mut file: T) {
    let mut version: [u8; HEADER_PREFIX.len()] = [0; HEADER_PREFIX.len()];
    file.read_exact(&mut version).unwrap();
    assert_eq!(version, HEADER_PREFIX.as_bytes());

    let mut timestamps = file.lines();
    let version = timestamps.next().unwrap().unwrap().parse::<i32>().unwrap();
    match version {
        0 => apply::<V0Timestamps, T>(timestamps),
        _ => eprintln!("unknown version: {}", version),
    }
}
// -------------------

// Command line
// ------------

/// Operations on all Win32 timestamps, including the ones that are not
/// typically supported by Unix-native tools.
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Dumps all timestamps in a directory tree to stdout.
    Dump {
        /// Root of the path to be dumped
        root: PathBuf,
    },

    /// Applies previously dumped timestamps from stdin.
    Apply,
}
// ------------

fn main() {
    let args = Cli::parse();

    match args.command {
        CliCommand::Dump { root } => dump::<V0Timestamps>(&root),
        CliCommand::Apply => apply_any(io::stdin().lock()),
    }
}
