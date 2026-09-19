//! Root-only translation of four application file ports to kernel operations.
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::rc::Rc;

use rustix::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, OFlags, ResolveFlags, StatxFlags};
use rustix_uring::{
    Errno, opcode,
    squeue::Entry,
    types::{Fd, OpenHow, Statx},
};

use crate::app;
use crate::ring::Operation;

pub enum Request {
    Open(app::Open),
    Stat(app::Stat),
    Read(app::Read),
    Close(app::Close),
}

pub struct FileOperation {
    pub owner: u64,
    request: Request,
    descriptor: Option<OwnedFd>,
    root: Option<Rc<OwnedFd>>,
    path: Option<CString>,
    how: OpenHow,
    metadata: MaybeUninit<Statx>,
    result: Option<Result<u32, Errno>>,
}

pub struct Finished {
    pub owner: u64,
    pub completion: app::Completion,
    /// A successful open and every stat/read return the live descriptor.
    /// Close consumes it, including on a close error.
    pub descriptor: Option<OwnedFd>,
}

impl FileOperation {
    pub fn open(owner: u64, op: app::Open, root: Rc<OwnedFd>) -> Self {
        let path = CString::new(op.path.clone()).expect("application validated path");
        Self::new(owner, Request::Open(op), None, Some(root), Some(path))
    }

    pub fn stat(owner: u64, op: app::Stat, descriptor: OwnedFd) -> Self {
        Self::new(owner, Request::Stat(op), Some(descriptor), None, None)
    }

    pub fn read(owner: u64, op: app::Read, descriptor: OwnedFd) -> Self {
        Self::new(owner, Request::Read(op), Some(descriptor), None, None)
    }

    pub fn close(owner: u64, op: app::Close, descriptor: OwnedFd) -> Self {
        Self::new(owner, Request::Close(op), Some(descriptor), None, None)
    }

    fn new(
        owner: u64,
        request: Request,
        descriptor: Option<OwnedFd>,
        root: Option<Rc<OwnedFd>>,
        path: Option<CString>,
    ) -> Self {
        Self {
            owner,
            request,
            descriptor,
            root,
            path,
            // NONBLOCK prevents opening a FIFO from occupying an io-wq worker
            // indefinitely. Metadata rejects all non-regular files.
            how: OpenHow::new()
                .flags(OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY)
                .resolve(ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS),
            metadata: MaybeUninit::uninit(),
            result: None,
        }
    }

    pub fn finish(self, key: u64) -> Finished {
        let result = self.result.expect("operation must complete before finish");
        let completion = match self.request {
            Request::Open(op) => app::Completion::Open {
                id: op.id,
                result: result.map(|_| app::File(key)).map_err(file_error),
            },
            Request::Stat(op) => app::Completion::Stat {
                id: op.id,
                result: result
                    .and_then(|_| {
                        // Statx initialized this allocation before its successful CQE.
                        let metadata = unsafe { self.metadata.assume_init() };
                        if !StatxFlags::from_bits_retain(metadata.stx_mask)
                            .contains(StatxFlags::SIZE | StatxFlags::TYPE)
                        {
                            return Err(Errno::IO);
                        }
                        Ok(app::Metadata {
                            length: metadata.stx_size,
                            regular: FileType::from_raw_mode(metadata.stx_mode.into())
                                == FileType::RegularFile,
                        })
                    })
                    .map_err(file_error),
            },
            Request::Read(op) => app::Completion::Read {
                id: op.id,
                buffer: op.buffer,
                result: result.map(|count| count as usize).map_err(file_error),
            },
            Request::Close(op) => app::Completion::Close {
                id: op.id,
                result: result.map(|_| ()).map_err(file_error),
            },
        };
        Finished {
            owner: self.owner,
            completion,
            descriptor: self.descriptor,
        }
    }
}

// All kernel pointers refer to fields in the executor's stable Box, or to owned
// buffer allocations. Only one app file operation can borrow a descriptor.
unsafe impl Operation for FileOperation {
    unsafe fn entry(&mut self) -> Entry {
        match &mut self.request {
            Request::Open(_) => opcode::OpenAt2::new(
                Fd(self.root.as_ref().unwrap().as_raw_fd()),
                self.path.as_ref().unwrap().as_ptr(),
                &self.how,
            )
            .build(),
            Request::Stat(_) => opcode::Statx::new(
                Fd(self.descriptor.as_ref().unwrap().as_raw_fd()),
                c"".as_ptr(),
                self.metadata.as_mut_ptr(),
            )
            .flags(AtFlags::EMPTY_PATH)
            .mask(StatxFlags::SIZE | StatxFlags::TYPE)
            .build(),
            Request::Read(op) => opcode::Read::new(
                Fd(self.descriptor.as_ref().unwrap().as_raw_fd()),
                op.buffer.as_mut_ptr(),
                op.limit.min(op.buffer.len()).min(u32::MAX as usize) as u32,
            )
            .offset(op.offset)
            .build(),
            Request::Close(_) => {
                let fd = self.descriptor.take().unwrap().into_raw_fd();
                opcode::Close::new(Fd(fd)).build()
            }
        }
    }

    unsafe fn completed(&mut self, result: Result<u32, Errno>) {
        if matches!(self.request, Request::Open(_))
            && let Ok(fd) = result
        {
            // A successful OpenAt2 CQE transfers a newly owned descriptor.
            self.descriptor = Some(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        }
        self.result = Some(result);
    }

    fn cancelable(&self) -> bool {
        !matches!(self.request, Request::Close(_))
    }
}

fn file_error(error: Errno) -> app::FileError {
    match error {
        Errno::NOENT | Errno::NOTDIR => app::FileError::Missing,
        Errno::ACCESS | Errno::PERM | Errno::LOOP | Errno::XDEV => app::FileError::Forbidden,
        _ => app::FileError::Other,
    }
}
