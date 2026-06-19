use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_void as raw_c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{Ordering, fence};
use std::sync::{Mutex, OnceLock};

use kimojio_stack::{Mode, OFlags, OwnedFd, RuntimeContext, StatxFlags};
use libc::{MAP_FAILED, MAP_SHARED, PROT_READ, PROT_WRITE, mmap, munmap};
use libsqlite3_sys as ffi;

use crate::path;

#[derive(Default)]
pub struct ShmState {
    path: Option<PathBuf>,
    fd: Option<OwnedFd>,
    size: Option<u64>,
    nofollow: bool,
    owner: usize,
    maps: Vec<Option<Mapping>>,
}

struct Mapping {
    ptr: *mut c_void,
    len: usize,
}

impl ShmState {
    pub fn new(owner: usize, nofollow: bool) -> Self {
        Self {
            path: None,
            fd: None,
            size: None,
            nofollow,
            owner,
            maps: Vec::new(),
        }
    }

    pub unsafe fn map(
        &mut self,
        db_path: Option<&Path>,
        i_pg: c_int,
        page_size: c_int,
        extend: c_int,
        out: *mut *mut raw_c_void,
    ) -> c_int {
        if out.is_null() || i_pg < 0 || page_size <= 0 {
            return ffi::SQLITE_IOERR_SHMMAP;
        }
        unsafe {
            *out = std::ptr::null_mut();
        }

        let Some(db_path) = db_path else {
            return ffi::SQLITE_IOERR_SHMMAP;
        };
        let shm_path = match &self.path {
            Some(path) => path.clone(),
            None => {
                let path = path::sidecar_path(db_path, "-shm");
                retain_shm_path(&path);
                self.path = Some(path.clone());
                path
            }
        };
        let page_index = i_pg as usize;
        if let Some(Some(mapping)) = self.maps.get(page_index) {
            unsafe {
                *out = mapping.ptr.cast::<raw_c_void>();
            }
            return ffi::SQLITE_OK;
        }

        let needed = (page_index + 1)
            .checked_mul(page_size as usize)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or(ffi::SQLITE_IOERR_SHMSIZE)
            .unwrap_or(0);
        if needed == 0 {
            return ffi::SQLITE_IOERR_SHMSIZE;
        }

        if self.fd.is_none() {
            let mut open_flags = OFlags::RDWR | OFlags::CLOEXEC;
            if self.nofollow {
                open_flags |= OFlags::NOFOLLOW;
            }
            if extend != 0 {
                open_flags |= OFlags::CREATE;
            } else {
                let available = RuntimeContext::with_current(|cx| {
                    cx.statx(
                        &shm_path,
                        kimojio_stack::AtFlags::empty(),
                        StatxFlags::BASIC_STATS,
                    )
                    .map(|stat| stat.stx_size >= needed)
                });
                match available {
                    Some(Ok(true)) => {}
                    Some(Ok(false)) => return ffi::SQLITE_OK,
                    Some(Err(err)) if err == kimojio_stack::Errno::NOENT => {
                        return ffi::SQLITE_OK;
                    }
                    Some(Err(_)) | None => return ffi::SQLITE_IOERR_SHMOPEN,
                }
            }

            let open_result = RuntimeContext::with_current(|cx| {
                cx.open(&shm_path, open_flags, Mode::RUSR | Mode::WUSR)
                    .and_then(|fd| {
                        let current_size = cx.fstat(&fd, StatxFlags::BASIC_STATS)?.stx_size;
                        if current_size < needed {
                            if extend == 0 {
                                cx.close(fd)?;
                                return Ok(None);
                            }
                            cx.ftruncate(&fd, needed)?;
                            Ok(Some((fd, needed)))
                        } else {
                            Ok(Some((fd, current_size)))
                        }
                    })
            });
            let Some(open_result) = open_result else {
                return ffi::SQLITE_IOERR_SHMOPEN;
            };
            let Ok(Some((fd, size))) = open_result else {
                return if open_result.is_ok() {
                    ffi::SQLITE_OK
                } else {
                    ffi::SQLITE_IOERR_SHMOPEN
                };
            };
            self.fd = Some(fd);
            self.size = Some(size);
        } else {
            let resize_result: Option<Result<bool, kimojio_stack::Errno>> =
                RuntimeContext::with_current(|cx| {
                    let size = self.size.unwrap_or_else(|| {
                        let fd = self.fd.as_ref().expect("checked above");
                        cx.fstat(fd, StatxFlags::BASIC_STATS)
                            .map(|stat| stat.stx_size)
                            .unwrap_or(0)
                    });
                    if size < needed {
                        if extend == 0 {
                            return Ok(false);
                        }
                        let fd = self.fd.as_ref().expect("checked above");
                        cx.ftruncate(fd, needed)?;
                        self.size = Some(needed);
                    }
                    Ok(true)
                });
            match resize_result {
                Some(Ok(true)) => {}
                Some(Ok(false)) => return ffi::SQLITE_OK,
                Some(Err(_)) | None => return ffi::SQLITE_IOERR_SHMSIZE,
            }
        }

        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                page_size as usize,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                self.fd.as_ref().expect("SHM fd opened").as_raw_fd(),
                i64::from(i_pg) * i64::from(page_size),
            )
        };
        if ptr == MAP_FAILED {
            return ffi::SQLITE_IOERR_SHMMAP;
        }

        if self.maps.len() <= page_index {
            self.maps.resize_with(page_index + 1, || None);
        }
        self.maps[page_index] = Some(Mapping {
            ptr,
            len: page_size as usize,
        });
        unsafe {
            *out = ptr.cast::<raw_c_void>();
        }
        ffi::SQLITE_OK
    }

    pub fn lock(&self, offset: c_int, n: c_int, flags: c_int) -> c_int {
        let Some(path) = &self.path else {
            return ffi::SQLITE_IOERR_SHMLOCK;
        };
        if offset < 0 || n <= 0 || offset + n > ffi::SQLITE_SHM_NLOCK {
            return ffi::SQLITE_IOERR_SHMLOCK;
        }

        let mut table = shm_locks().lock().expect("SQLite SHM lock table poisoned");
        let locks = table.entry(path.clone()).or_default();
        let range = offset as usize..(offset + n) as usize;
        if flags & ffi::SQLITE_SHM_LOCK != 0 {
            let exclusive = flags & ffi::SQLITE_SHM_EXCLUSIVE != 0;
            if !locks.can_lock(self.owner, range.clone(), exclusive) {
                return ffi::SQLITE_BUSY;
            }
            locks.lock(self.owner, range, exclusive);
        } else if flags & ffi::SQLITE_SHM_UNLOCK != 0 {
            let exclusive = flags & ffi::SQLITE_SHM_EXCLUSIVE != 0;
            locks.unlock(self.owner, range, exclusive);
        }

        ffi::SQLITE_OK
    }

    pub fn barrier(&self) {
        fence(Ordering::SeqCst);
    }

    pub unsafe fn unmap(&mut self, delete_file: bool) -> c_int {
        let mut rc = ffi::SQLITE_OK;
        for mapping in self.maps.iter_mut().filter_map(Option::take) {
            let result = unsafe { munmap(mapping.ptr, mapping.len) };
            if result != 0 {
                rc = ffi::SQLITE_IOERR_SHMMAP;
            }
        }

        if let Some(path) = &self.path {
            let mut table = shm_locks().lock().expect("SQLite SHM lock table poisoned");
            if let Some(locks) = table.get_mut(path) {
                locks.release_owner(self.owner);
            }
            if let Some(fd) = self.fd.take() {
                let close_result = RuntimeContext::with_current(|cx| cx.close(fd));
                if close_result.is_none_or(|result| result.is_err()) {
                    rc = ffi::SQLITE_IOERR_SHMOPEN;
                }
            }
            let last_reference = release_shm_path(path);
            if delete_file && last_reference {
                let delete_result = RuntimeContext::with_current(|cx| cx.unlink(path));
                if delete_result.is_none_or(|result| result.is_err()) {
                    rc = ffi::SQLITE_IOERR_SHMOPEN;
                }
            }
        }
        rc
    }
}

impl Drop for ShmState {
    fn drop(&mut self) {
        unsafe {
            let _ = self.unmap(false);
        }
    }
}

// WAL SHM locks are coordinated in-process for this milestone. SQLite passes
// slot ranges through xShmLock; shared locks can coexist, exclusive locks require
// every slot in the requested range to be unheld except by the same owner.
#[derive(Default)]
struct ShmLocks {
    slots: [ShmSlot; ffi::SQLITE_SHM_NLOCK as usize],
}

impl ShmLocks {
    fn can_lock(&self, owner: usize, range: std::ops::Range<usize>, exclusive: bool) -> bool {
        range.into_iter().all(|slot| {
            let slot = &self.slots[slot];
            if let Some(exclusive_owner) = slot.exclusive
                && exclusive_owner != owner
            {
                return false;
            }
            if exclusive {
                slot.shared.is_empty() || (slot.shared.len() == 1 && slot.shared.contains(&owner))
            } else {
                true
            }
        })
    }

    fn lock(&mut self, owner: usize, range: std::ops::Range<usize>, exclusive: bool) {
        for slot in range {
            let slot = &mut self.slots[slot];
            if exclusive {
                slot.shared.remove(&owner);
                slot.exclusive = Some(owner);
            } else {
                slot.shared.insert(owner);
            }
        }
    }

    fn unlock(&mut self, owner: usize, range: std::ops::Range<usize>, exclusive: bool) {
        for slot in range {
            let slot = &mut self.slots[slot];
            if exclusive {
                if slot.exclusive == Some(owner) {
                    slot.exclusive = None;
                }
            } else {
                slot.shared.remove(&owner);
            }
        }
    }

    fn release_owner(&mut self, owner: usize) {
        for slot in &mut self.slots {
            if slot.exclusive == Some(owner) {
                slot.exclusive = None;
            }
            slot.shared.remove(&owner);
        }
    }
}

#[derive(Default)]
struct ShmSlot {
    shared: HashSet<usize>,
    exclusive: Option<usize>,
}

fn shm_locks() -> &'static Mutex<HashMap<PathBuf, ShmLocks>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, ShmLocks>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn retain_shm_path(path: &Path) {
    let mut refs = shm_refs().lock().expect("SQLite SHM ref table poisoned");
    *refs.entry(path.to_path_buf()).or_default() += 1;
}

fn release_shm_path(path: &Path) -> bool {
    let mut refs = shm_refs().lock().expect("SQLite SHM ref table poisoned");
    let Some(count) = refs.get_mut(path) else {
        return true;
    };
    *count -= 1;
    if *count == 0 {
        refs.remove(path);
        true
    } else {
        false
    }
}

fn shm_refs() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static REFS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    REFS.get_or_init(|| Mutex::new(HashMap::new()))
}
