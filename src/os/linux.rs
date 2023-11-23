/*
 * Copyright © 2018-2019, Steve Smith <tarkasteve@gmail.com>
 *
 * This program is free software: you can redistribute it and/or
 * modify it under the terms of the GNU General Public License version
 * 3 as published by the Free Software Foundation.
 *
 * This program is distributed in the hope that it will be useful, but
 * WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */


use std::{fs::File, os::raw::c_void};
use std::ops::Range;
use std::os::linux::fs::MetadataExt;

use rustix::fd::AsRawFd;
use rustix::{fs::{copy_file_range, seek, SeekFrom}, io::Errno, ioctl::{Ioctl, IoctlOutput, Opcode, RawOpcode, ioctl}};

use crate::errors::Result;
use crate::os::common::{copy_bytes_uspace, copy_range_uspace};

// Wrapper for copy_file_range(2) that checks for non-fatal errors due
// to limitations of the syscall.
fn try_copy_file_range(
    infd: &File,
    in_off: Option<&mut u64>,
    outfd: &File,
    out_off: Option<&mut u64>,
    bytes: u64,
) -> Option<Result<usize>> {
    let cfr_ret = copy_file_range(infd, in_off, outfd, out_off, bytes as usize);

    match cfr_ret {
        Ok(retval) => {
            Some(Ok(retval))
        },
        Err(Errno::NOSYS) | Err(Errno::PERM) | Err(Errno::XDEV) => {
            None
        },
        Err(errno) => {
            Some(Err(errno.into()))
        },
    }
}

// Wrapper for copy_file_range(2) that defers file offset tracking to
// the underlying call.  Falls back to user-space if
// `copy_file_range()` ia not available for thie operation.
pub fn copy_file_bytes(infd: &File, outfd: &File, bytes: u64) -> Result<usize> {
    try_copy_file_range(infd, None, outfd, None, bytes)
        .unwrap_or_else(|| copy_bytes_uspace(infd, outfd, bytes as usize))
}

// Wrapper for copy_file_range(2) that copies a block at offset
// `off`. Falls back to user-space if `copy_file_range()` ia not
// available for thie operation.
#[allow(dead_code)]
pub fn copy_file_offset(infd: &File, outfd: &File, bytes: u64, off: i64) -> Result<usize> {
    let mut off_in = off as u64;
    let mut off_out = off as u64;
    try_copy_file_range(infd, Some(&mut off_in), outfd, Some(&mut off_out), bytes)
        .unwrap_or_else(|| copy_range_uspace(infd, outfd, bytes as usize, off as usize))
}

// Guestimate if file is sparse; if it has less blocks that would be
// expected for its stated size. This is the same test used by
// coreutils `cp`.
pub fn probably_sparse(fd: &File) -> Result<bool> {
    const ST_NBLOCKSIZE: u64 = 512;
    let stat = fd.metadata()?;
    Ok(stat.st_blocks() < stat.st_size() / ST_NBLOCKSIZE)
}

#[derive(PartialEq, Debug)]
pub enum SeekOff {
    Offset(u64),
    EOF,
}

pub fn lseek(fd: &File, from: SeekFrom) -> Result<SeekOff> {
    match seek(fd, from) {
        Err(errno) if errno == Errno::NXIO => Ok(SeekOff::EOF),
        Err(err) => Err(err.into()),
        Ok(off) => Ok(SeekOff::Offset(off)),
    }
}

// See ioctl_list(2)
#[allow(unused)]
const FS_IOC_FIEMAP: libc::c_ulong = 0xC020660B;
#[allow(unused)]
const FIEMAP_EXTENT_LAST: u32 = 0x00000001;
const PAGE_SIZE: usize = 32;

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct FiemapExtent {
    fe_logical: u64,  // Logical offset in bytes for the start of the extent
    fe_physical: u64, // Physical offset in bytes for the start of the extent
    fe_length: u64,   // Length in bytes for the extent
    fe_reserved64: [u64; 2],
    fe_flags: u32, // FIEMAP_EXTENT_* flags for this extent
    fe_reserved: [u32; 3],
}
#[allow(unused)]
impl FiemapExtent {
    fn new() -> FiemapExtent {
        FiemapExtent {
            fe_logical: 0,
            fe_physical: 0,
            fe_length: 0,
            fe_reserved64: [0; 2],
            fe_flags: 0,
            fe_reserved: [0; 3],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct FiemapReq {
    fm_start: u64,          // Logical offset (inclusive) at which to start mapping (in)
    fm_length: u64,         // Logical length of mapping which userspace cares about (in)
    fm_flags: u32,          // FIEMAP_FLAG_* flags for request (in/out)
    fm_mapped_extents: u32, // Number of extents that were mapped (out)
    fm_extent_count: u32,   // Size of fm_extents array (in)
    fm_reserved: u32,
    fm_extents: [FiemapExtent; PAGE_SIZE], // Array of mapped extents (out)
}

impl Default for FiemapReq {
    fn default() -> Self {
        FiemapReq {
            fm_start: 0,
            fm_length: u64::max_value(),
            fm_flags: 0,
            fm_mapped_extents: 0,
            fm_extent_count: PAGE_SIZE as u32,
            fm_reserved: 0,
            fm_extents: [FiemapExtent::new(); PAGE_SIZE],
        }
    }
}

unsafe impl Ioctl for FiemapReq {
    type Output = &Self;
    const OPCODE: Opcode = Opcode::old(FS_IOC_FIEMAP as RawOpcode);
    const IS_MUTATING: bool = true;

    fn as_ptr(&mut self) -> *mut c_void {
        self as *const Self as *mut c_void
    }

    unsafe fn output_from_ptr(_out: IoctlOutput, optr: *mut c_void) -> rustix::io::Result<Self::Output> {
        //Ok(optr as *const Self as &Self)
        Ok(&*optr.cast())
    }
}


#[allow(unused)]
pub fn map_extents(fd: &File) -> Result<Option<Vec<Range<u64>>>> {
    let mut req = FiemapReq::default();
    let req_ptr: *const FiemapReq = &req;
    let mut extents = Vec::with_capacity(PAGE_SIZE);

    loop {
        // if unsafe { libc::ioctl(fd.as_raw_fd(), FS_IOC_FIEMAP, req_ptr) } != 0 {
        //     let oserr = std::io::Error::last_os_error();
        //     if oserr.raw_os_error() == Some(95) {
        //         return Ok(None)
        //     }
        //     return Err(oserr.into());
        // }
        println!("TESTING");
        match unsafe { ioctl(fd, req) } {
            Err(Errno::OPNOTSUPP) => return Ok(None),
            Err(errno) => {
                println!("GOT ERRNOR: {:?}", errno);
                return Err(errno.into())
            },
            Ok(_) => {
                println!("OK");
            }
        }

        println!("EXTENTS == {}", req.fm_mapped_extents);
        if req.fm_mapped_extents == 0 {
            break;
        }

        for i in 0..req.fm_mapped_extents as usize {
            let e = req.fm_extents[i];
            let start = e.fe_logical;
            let end = start + e.fe_length;
            extents.push(start..end);
        }

        let last = req.fm_extents[(req.fm_mapped_extents - 1) as usize];
        if last.fe_flags & FIEMAP_EXTENT_LAST != 0 {
            break;
        }

        // Looks like we're going around again...
        req.fm_start = last.fe_logical + last.fe_length;
    }

    Ok(Some(extents))
}

pub fn next_sparse_segments(infd: &File, outfd: &File, pos: u64) -> Result<(u64, u64)> {
    let next_data = match lseek(infd, SeekFrom::Data(pos as i64))? {
        SeekOff::Offset(off) => off,
        SeekOff::EOF => infd.metadata()?.len(),
    };
    let next_hole = match lseek(infd, SeekFrom::Hole(next_data as i64))? {
        SeekOff::Offset(off) => off,
        SeekOff::EOF => infd.metadata()?.len(),
    };

    lseek(infd, SeekFrom::Start(next_data))?; // FIXME: EOF (but shouldn't happen)
    lseek(outfd, SeekFrom::Start(next_data))?;

    Ok((next_data, next_hole))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os::allocate_file;
    use std::env::{current_dir, var};
    use std::fs::{read, OpenOptions};
    use std::io::{self, Seek, Write};
    use std::iter;
    use std::path::PathBuf;
    use std::process::Command;
    use tempfile::{tempdir_in, TempDir};

    fn tempdir() -> Result<TempDir> {
        // Force into local dir as /tmp might be tmpfs, which doesn't
        // support all VFS options (notably fiemap).
        Ok(tempdir_in(current_dir()?.join("target"))?)
    }

    fn fs_supports_extents() -> bool {
        // See `.github/workflows/rust.yml`
        let unsupported = ["ext2", "ntfs", "fat", "vfat", "zfs"];
        match var("XCP_TEST_FS") {
            Ok(fs) => {
                !unsupported.contains(&fs.as_str())
            },
            Err(_) => true // Not CI, assume 'normal' linux environment.
        }
    }

    fn fs_supports_sparse() -> bool {
        // FIXME: Same set for now.
        fs_supports_extents()
    }

    #[test]
    fn test_sparse_detection_small_data() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        assert!(!probably_sparse(&File::open("Cargo.toml")?)?);

        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");
        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());

        {
            let fd = File::open(&file)?;
            assert!(probably_sparse(&fd)?);
        }
        {
            let mut fd = OpenOptions::new().write(true).append(false).open(&file)?;
            write!(fd, "test")?;
            assert!(probably_sparse(&fd)?);
        }

        Ok(())
    }

    #[test]
    fn test_sparse_detection_half() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        assert!(!probably_sparse(&File::open("Cargo.toml")?)?);

        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");
        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());
        {
            let mut fd = OpenOptions::new().write(true).append(false).open(&file)?;
            let s = "x".repeat(512*1024);
            fd.write(s.as_bytes())?;
            assert!(probably_sparse(&fd)?);
        }

        Ok(())
    }

    #[test]
    fn test_copy_bytes_sparse() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");
        let from = dir.path().join("from.txt");
        let data = "test data";

        {
            let mut fd = File::create(&from)?;
            write!(fd, "{}", data)?;
        }

        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());

        {
            let infd = File::open(&from)?;
            let outfd: File = OpenOptions::new().write(true).append(false).open(&file)?;
            copy_file_bytes(&infd, &outfd, data.len() as u64)?;
        }

        assert!(probably_sparse(&File::open(file)?)?);

        Ok(())
    }

    #[test]
    fn test_sparse_copy_middle() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");
        let from = dir.path().join("from.txt");
        let data = "test data";

        {
            let mut fd = File::create(&from)?;
            write!(fd, "{}", data)?;
        }

        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());

        let offset = 512 * 1024;
        {
            let infd = File::open(&from)?;
            let outfd: File = OpenOptions::new().write(true).append(false).open(&file)?;
            let mut off_in = 0;
            let mut off_out = offset as u64;
            let copied = copy_file_range(
                &infd,
                Some(&mut off_in),
                &outfd,
                Some(&mut off_out),
                data.len(),
            )?;
            assert_eq!(copied as usize, data.len());
        }

        assert!(probably_sparse(&File::open(&file)?)?);

        let bytes = read(&file)?;

        assert!(bytes.len() == 1024 * 1024);
        assert!(bytes[offset] == b't');
        assert!(bytes[offset + 1] == b'e');
        assert!(bytes[offset + 2] == b's');
        assert!(bytes[offset + 3] == b't');
        assert!(bytes[offset + data.len()] == 0);

        Ok(())
    }

    #[test]
    fn test_copy_range_middle() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");
        let from = dir.path().join("from.txt");
        let data = "test data";
        let offset: usize = 512 * 1024;

        {
            let mut fd = File::create(&from)?;
            fd.seek(io::SeekFrom::Start(offset as u64))?;
            write!(fd, "{}", data)?;
        }

        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());

        {
            let infd = File::open(&from)?;
            let outfd: File = OpenOptions::new().write(true).append(false).open(&file)?;
            let copied =
                copy_file_offset(&infd, &outfd, data.len() as u64, offset as i64)?;
            assert_eq!(copied as usize, data.len());
        }

        assert!(probably_sparse(&File::open(&file)?)?);

        let bytes = read(&file)?;
        assert_eq!(bytes.len(), 1024 * 1024);
        assert_eq!(bytes[offset], b't');
        assert_eq!(bytes[offset + 1], b'e');
        assert_eq!(bytes[offset + 2], b's');
        assert_eq!(bytes[offset + 3], b't');
        assert_eq!(bytes[offset + data.len()], 0);

        Ok(())
    }

    #[test]
    fn test_lseek_data() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");
        let from = dir.path().join("from.txt");
        let data = "test data";
        let offset = 512 * 1024;

        {
            let mut fd = File::create(&from)?;
            write!(fd, "{}", data)?;
        }

        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());
        {
            let infd = File::open(&from)?;
            let outfd: File = OpenOptions::new().write(true).append(false).open(&file)?;
            let mut off_in = 0;
            let mut off_out = offset;
            let copied = copy_file_range(
                &infd,
                Some(&mut off_in),
                &outfd,
                Some(&mut off_out),
                data.len(),
            )?;
            assert_eq!(copied as usize, data.len());
        }

        assert!(probably_sparse(&File::open(&file)?)?);

        let off = lseek(&File::open(&file)?, SeekFrom::Data(0))?;
        assert_eq!(off, SeekOff::Offset(offset));

        Ok(())
    }

    #[test]
    fn test_sparse_rust_seek() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        let dir = PathBuf::from("target");
        let file = dir.join("sparse.bin");

        let data = "c00lc0d3";

        {
            let mut fd = File::create(&file)?;
            write!(fd, "{}", data)?;

            fd.seek(io::SeekFrom::Start(1024 * 4096))?;
            write!(fd, "{}", data)?;

            fd.seek(io::SeekFrom::Start(4096 * 4096 - data.len() as u64))?;
            write!(fd, "{}", data)?;
        }

        assert!(probably_sparse(&File::open(&file)?)?);

        let bytes = read(&file)?;
        assert!(bytes.len() == 4096 * 4096);

        let offset = 1024 * 4096;
        assert!(bytes[offset] == b'c');
        assert!(bytes[offset + 1] == b'0');
        assert!(bytes[offset + 2] == b'0');
        assert!(bytes[offset + 3] == b'l');
        assert!(bytes[offset + data.len()] == 0);

        Ok(())
    }

    #[test]
    fn test_lseek_no_data() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");

        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());
        assert!(probably_sparse(&File::open(&file)?)?);

        let fd = File::open(&file)?;
        let off = lseek(&fd, SeekFrom::Data(0))?;
        assert!(off == SeekOff::EOF);

        Ok(())
    }

    #[test]
    fn test_allocate_file_is_sparse() -> Result<()> {
        if !fs_supports_sparse() {
            return Ok(())
        }

        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");
        let len = 32 * 1024 * 1024;

        {
            let fd = File::create(&file)?;
            allocate_file(&fd, len)?;
        }

        assert_eq!(len, file.metadata()?.len());
        assert!(probably_sparse(&File::open(&file)?)?);

        Ok(())
    }

    #[test]
    fn test_empty_extent() -> Result<()> {
        if !fs_supports_extents() {
            return Ok(())
        }
        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");

        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());

        let fd = File::open(file)?;

        let extents_p = map_extents(&fd)?;
        assert!(extents_p.is_some());
        let extents = extents_p.unwrap();
        assert_eq!(extents.len(), 0);

        Ok(())
    }

    #[test]
    fn test_extent_fetch() -> Result<()> {
        if !fs_supports_extents() {
            return Ok(())
        }
        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");
        let from = dir.path().join("from.txt");
        let data = "test data";

        {
            let mut fd = File::create(&from)?;
            write!(fd, "{}", data)?;
        }

        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());

        let offset = 512 * 1024;
        {
            let infd = File::open(&from)?;
            let outfd: File = OpenOptions::new().write(true).append(false).open(&file)?;
            let mut off_in = 0;
            let mut off_out = offset;
            let copied = copy_file_range(
                &infd,
                Some(&mut off_in),
                &outfd,
                Some(&mut off_out),
                data.len(),
            )?;
            assert_eq!(copied as usize, data.len());
        }

        let fd = File::open(file)?;

        let extents_p = map_extents(&fd)?;
        assert!(extents_p.is_some());
        let extents = extents_p.unwrap();
        assert_eq!(extents.len(), 1);
        assert_eq!(extents[0].start, offset as u64);
        assert_eq!(extents[0].end, offset as u64 + 4 * 1024); // FIXME: Assume 4k blocks

        Ok(())
    }

    #[test]
    fn test_extent_fetch_many() -> Result<()> {
        if !fs_supports_extents() {
            return Ok(())
        }
        let dir = tempdir()?;
        let file = dir.path().join("sparse.bin");

        let out = Command::new("/usr/bin/truncate")
            .args(["-s", "1M", file.to_str().unwrap()])
            .output()?;
        assert!(out.status.success());

        let fsize = 1024 * 1024;
        // FIXME: Assumes 4k blocks
        let bsize = 4 * 1024;
        let block = iter::repeat(0xff_u8).take(bsize).collect::<Vec<u8>>();

        let mut fd = OpenOptions::new().write(true).append(false).open(&file)?;
        // Skip every-other block
        for off in (0..fsize).step_by(bsize * 2) {
            lseek(&fd, SeekFrom::Start(off))?;
            fd.write_all(block.as_slice())?;
        }

        let extents_p = map_extents(&fd)?;
        assert!(extents_p.is_some());
        let extents = extents_p.unwrap();
        assert_eq!(extents.len(), fsize as usize / bsize / 2);

        Ok(())
    }

    #[test]
    fn test_extent_not_sparse() -> Result<()> {
        if !fs_supports_extents() {
            return Ok(())
        }
        let dir = tempdir()?;
        let file = dir.path().join("file.bin");
        let size = 128 * 1024;

        {
            let mut fd: File = File::create(&file)?;
            let data = "X".repeat(size);
            write!(fd, "{}", data)?;
        }

        let fd = File::open(file)?;
        let extents_p = map_extents(&fd)?;
        assert!(extents_p.is_some());
        let extents = extents_p.unwrap();

        assert_eq!(1, extents.len());
        assert_eq!(0..size as u64, extents[0]);

        Ok(())
    }

    #[test]
    fn test_extent_unsupported_fs() -> Result<()> {
        if !fs_supports_extents() {
            return Ok(())
        }
        let file = "/proc/cpuinfo";
        let fd = File::open(file)?;
        let extents_p = map_extents(&fd)?;
        assert!(extents_p.is_none());

        Ok(())
    }
}
