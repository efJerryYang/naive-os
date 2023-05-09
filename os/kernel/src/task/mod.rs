use crate::{fs::{
    file::{OpenFlags, RegFileINode, TerminalINode},
    vfs::INode,
}, mm::{PhysAddr, VirtAddr}};
use alloc::string::{String, ToString};
use core::arch::global_asm;
use hashbrown::HashMap;

pub use context::ProcessContext;
pub mod context;
pub mod cpu;
pub mod proc;
use self::cpu::mycpu;
use crate::mm::{MemorySet, PhysPageNum};
use alloc::{sync::Arc, vec, vec::Vec};
use lazy_static::lazy_static;
use spin::Mutex;

use crate::UPSafeCell;

global_asm!(include_str!("switch.S"));
extern "C" {
    fn __switch(current: *mut ProcessContext, next: *mut ProcessContext);
}

lazy_static! {
    pub static ref task_list: UPSafeCell<Vec<PCB>> = unsafe { UPSafeCell::new(Vec::new()) };
}
lazy_static! {
    pub static ref global_dentry_cache: GlobalDentryCache = GlobalDentryCache {
        table: Arc::new(Mutex::new(HashMap::new())),
    };
    pub static ref global_inode_table: GlobalInodeTable = GlobalInodeTable {
        table: Arc::new(Mutex::new(Vec::new())),
    };
    pub static ref global_open_file_table: GlobalOpenFileTable = GlobalOpenFileTable {
        table: Arc::new(Mutex::new(Vec::new())),
    };
}

#[derive(Copy, Clone, PartialEq)]
pub enum ProcessState {
    READY,
    RUNNING,
    ZOMBIE,
    KILLED,
    EMPTY,
}
#[derive(Clone)]
pub struct OpenFile {
    pub offset: usize,
    pub status_flags: u32,
    pub inode: Arc<Mutex<dyn INode>>,
}

impl OpenFile {
    pub fn new() -> Self {
        Self {
            offset: 0,
            status_flags: 0,
            inode: Arc::new(Mutex::new(RegFileINode::new(
                "/".to_string(),
                "null".to_string(),
                OpenFlags::new(0),
                true,
                true,
            ))),
        }
    }

    pub fn new_stdin() -> Self {
        Self {
            offset: 0,
            status_flags: 0,
            inode: Arc::new(Mutex::new(TerminalINode::new_stdin())),
        }
    }

    pub fn new_stdout() -> Self {
        Self {
            offset: 0,
            status_flags: 0,
            inode: Arc::new(Mutex::new(TerminalINode::new_stdout())),
        }
    }

    pub fn new_stderr() -> Self {
        Self {
            offset: 0,
            status_flags: 0,
            inode: Arc::new(Mutex::new(TerminalINode::new_stderr())),
        }
    }
}
pub struct GlobalOpenFileTable {
    table: Arc<Mutex<Vec<OpenFile>>>,
}

impl GlobalOpenFileTable {
    pub fn insert(&self, open_file: Arc<OpenFile>) -> Arc<OpenFile> {
        let mut table = self.table.lock();
        table.push((*open_file).clone());
        Arc::new(table.last().unwrap().clone())
    }
}
#[derive(Clone)]
pub struct FileDescriptor {
    pub open_file: Arc<OpenFile>,
    pub readable: bool,
    pub writable: bool,
}

pub struct FdManager {
    pub fd_array: Vec<FileDescriptor>,
}

impl FdManager {
    pub fn new() -> Self {
        let mut v = Vec::new();
        // 0, 1, 2 are reserved for stdin, stdout, stderr
        v.push(FileDescriptor {
            open_file: Arc::new(OpenFile::new_stdin()),
            readable: true,
            writable: false,
        });
        v.push(FileDescriptor {
            open_file: Arc::new(OpenFile::new_stdout()),
            readable: false,
            writable: true,
        });
        v.push(FileDescriptor {
            open_file: Arc::new(OpenFile::new_stderr()),
            readable: false,
            writable: true,
        });
        Self { fd_array: v }
    }
    pub fn len(&self) -> usize {
        self.fd_array.len()
    }
    pub fn close(&mut self, fd: usize) {
        let mut fd: Option<&mut FileDescriptor> = self.fd_array.get_mut(fd);
        if let Some(fd) = fd {
            if fd.readable || fd.writable {
                // Do nothing
                return;
            }
            let open_file = fd.open_file.clone();
            fd.open_file = Arc::new(OpenFile::new());
        }
    }
    pub fn insert(&mut self, file_descriptor: FileDescriptor) -> usize {
        self.fd_array.push(file_descriptor);
        self.fd_array.len() - 1
    }
    pub fn get(&self, fd: usize) -> Option<&FileDescriptor> {
        self.fd_array.get(fd)
    }
    pub fn get_mut(&mut self, fd: usize) -> Option<&mut FileDescriptor> {
        self.fd_array.get_mut(fd)
    }
    pub fn remove(&mut self, fd: usize) -> FileDescriptor {
        self.fd_array.remove(fd)
    }
}
pub struct GlobalInodeTable {
    pub table: Arc<Mutex<Vec<Arc<Mutex<dyn INode>>>>>,
}

pub struct GlobalDentryCache {
    pub table: Arc<Mutex<HashMap<String, Arc<Mutex<dyn INode>>>>>,
}

impl GlobalDentryCache {
    pub fn get(&self, path: &str) -> Option<Arc<Mutex<dyn INode>>> {
        let mut table = self.table.lock();
        match table.get(path) {
            Some(inode) => Some(inode.clone()),
            None => None,
        }
    }
    pub fn insert(&self, path: &str, inode: Arc<Mutex<dyn INode>>) -> Arc<Mutex<dyn INode>> {
        let mut table = self.table.lock();
        let old_path = path.to_string().clone();
        table.insert(path.to_string(), inode);
        table.get(&old_path).unwrap().clone()
    }
}

pub struct PCB {
    pub pid: usize,
    pub state: ProcessState,
    pub context: ProcessContext,
    pub trapframe_ppn: PhysPageNum,
    pub memory_set: MemorySet,
    pub heap_pos: VirtAddr,
    pub parent: usize,
    pub exit_code: isize,
    pub otime: usize,
    pub utime: usize,
    pub ktime: usize,
    pub cwd: String,
    pub fd_manager: Arc<Mutex<FdManager>>,
}

impl PCB {
    pub fn new() -> Self {
        PCB {
            pid: 0,
            state: ProcessState::READY,
            context: ProcessContext::new(),
            trapframe_ppn: 0.into(),
            memory_set: MemorySet::new_bare(),
            heap_pos: 0.into(),
            parent: 0xffffffff,
            exit_code: 0,
            utime: 0,
            otime: 0,
            ktime: 0,
            cwd: "/".to_string(),
            fd_manager: Arc::new(Mutex::new(FdManager::new())),
        }
    }
}

pub fn myproc() -> &'static mut PCB {
    &mut task_list.exclusive_access()[mycpu().proc_idx]
}
