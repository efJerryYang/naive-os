//! App management syscalls

use core::{clone, future::Future, pin::Pin, task::{Context, Poll}, ops::DerefMut, option};

use alloc::{
    boxed::Box,
    slice,
    string::{String, ToString},
    sync::Arc,
    task,
    vec::Vec, fmt::format, format,
};
use lazy_static::lazy_static;
use riscv::register::fcsr::Flag;
use xmas_elf::{ElfFile, header::parse_header};

use crate::{
    mm::{page_table::translate_str, translated_byte_buffer, MemorySet, VirtAddr, KERNEL_SPACE, MapPermission},
    sync::UPSafeCell,
    task::{
         ProcessState, PCB, Thread, TASK_QUEUE, PID_ALLOCATOR, ProcessContext, Process, GLOBAL_DENTRY_CACHE,
    }, config::{PAGE_SIZE, TRAPFRAME, TRAMPOLINE, KERNEL_STACK_SIZE, PRINT_SYSCALL}, trap::{TrapFrame, user_loop}, sbi::shutdown,
};

use super::raw_ptr::{UserPtr, Out};

struct YieldFuture(bool);

impl Future for YieldFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
bitflags! {
	/// 用于 sys_clone 的选项
	pub struct CloneFlags: u32 {
		/// .
		const CLONE_NEWTIME = 1 << 7;
		/// 共享地址空间
		const CLONE_VM = 1 << 8;
		/// 共享文件系统新信息
		const CLONE_FS = 1 << 9;
		/// 共享文件描述符(fd)表
		const CLONE_FILES = 1 << 10;
		/// 共享信号处理函数
		const CLONE_SIGHAND = 1 << 11;
		/// 创建指向子任务的fd，用于 sys_pidfd_open
		const CLONE_PIDFD = 1 << 12;
		/// 用于 sys_ptrace
		const CLONE_PTRACE = 1 << 13;
		/// 指定父任务创建后立即阻塞，直到子任务退出才继续
		const CLONE_VFORK = 1 << 14;
		/// 指定子任务的 ppid 为当前任务的 ppid，相当于创建“兄弟”而不是“子女”
		const CLONE_PARENT = 1 << 15;
		/// 作为一个“线程”被创建。具体来说，它同 CLONE_PARENT 一样设置 ppid，且不可被 wait
		const CLONE_THREAD = 1 << 16;
		/// 子任务使用新的命名空间。目前还未用到
		const CLONE_NEWNS = 1 << 17;
		/// 子任务共享同一组信号量。用于 sys_semop
		const CLONE_SYSVSEM = 1 << 18;
		/// 要求设置 tls
		const CLONE_SETTLS = 1 << 19;
		/// 要求在父任务的一个地址写入子任务的 tid
		const CLONE_PARENT_SETTID = 1 << 20;
		/// 要求将子任务的一个地址清零。这个地址会被记录下来，当子任务退出时会触发此处的 futex
		const CLONE_CHILD_CLEARTID = 1 << 21;
		/// 历史遗留的 flag，现在按 linux 要求应忽略
		const CLONE_DETACHED = 1 << 22;
		/// 与 sys_ptrace 相关，目前未用到
		const CLONE_UNTRACED = 1 << 23;
		/// 要求在子任务的一个地址写入子任务的 tid
		const CLONE_CHILD_SETTID = 1 << 24;
	}
}

impl Thread{
/// task exits and submit an exit code
	pub unsafe fn sys_exit(&self,exit_code: i32)->isize{
		let proc = &mut self.proc.inner.lock();
		proc.state = ProcessState::ZOMBIE;
		proc.exit_code = exit_code as isize;
		if PRINT_SYSCALL{
			println!("[exit] proc {} exited with code {}.",proc.pid,exit_code);
		}
		if let Some(nuclear)=proc.parent.as_ref(){
			let mut x=nuclear.inner.lock();
			x.children.turn_into_zombie(proc.pid);
		}else{
			shutdown();
			// println!("init exited.");
		}
		self.inner.exclusive_access().exit=true;
		0
	}

	pub unsafe fn sys_getpid(& self) -> isize {
		self.proc.pid as isize
	}
	pub unsafe fn sys_getppid(&self) -> isize {
		self.proc.inner.lock().parent.as_ref().unwrap().pid as isize
	}
	

	pub unsafe fn sys_clone(&self,flags:usize,stack: usize,ptid:usize, tls:usize, ctid:usize) -> isize {
		if PRINT_SYSCALL {println!("[clone] flags:{} stack:{:#x},ptid:{:#x},tls:{}",flags,stack,ptid,tls);}
		let mut pcb = self.proc.inner.lock();
		let mut pcb =pcb.deref_mut();
		let pid=pcb.pid;
		let new_pid= PID_ALLOCATOR.alloc_pid();
		let flags=CloneFlags::from_bits(flags as u32 & (!0x3f)).unwrap();
		if PRINT_SYSCALL {println!("[clone] pid:{} new_pid:{}",pid,new_pid);}

		let mut new_pcb=PCB::new();
		new_pcb.parent=Some(self.proc.clone());
		new_pcb.fd_manager=pcb.fd_manager.clone();
		// for fd in pcb.fd_manager.fd_array.clone(){
		// 	new_pcb.fd_manager.push(fd);
		// }
		new_pcb.memory_set=MemorySet::from_existed_user(&pcb.memory_set);
		// new_pcb.heap_pos = VirtAddr::from(pcb.memory_set.get_areas_end());
		new_pcb.heap_pos = pcb.heap_pos;
		new_pcb.mmap_pos = pcb.mmap_pos;
		// new_pcb.heap_pos.0 += PAGE_SIZE;
		new_pcb.trapframe_ppn = new_pcb
			.memory_set
			.translate(VirtAddr::from(TRAPFRAME).into())
			.unwrap()
			.ppn();
		let mut new_trapframe=(new_pcb.trapframe_ppn.get_mut() as *mut TrapFrame);
		*new_trapframe = *(pcb.trapframe_ppn.get_mut() as *mut TrapFrame);
		(*new_trapframe).x[10] = 0;

		(*new_trapframe).kernel_sp =
			TRAMPOLINE - KERNEL_STACK_SIZE * new_pid;
		KERNEL_SPACE.lock().insert_framed_area(
			(TRAMPOLINE - KERNEL_STACK_SIZE * (new_pid + 1)).into(),
			(TRAMPOLINE - KERNEL_STACK_SIZE * new_pid).into(),
			MapPermission::R | MapPermission::W,
		);
		if (stack != 0) {
			(*new_trapframe).x[2] = stack;
		}
		if flags.contains(CloneFlags::CLONE_SETTLS){
			(*new_trapframe).x[4]=tls;
		}
		
		new_pcb.context = pcb.context;
		new_pcb.context.ra = user_loop as usize;
		new_pcb.context.sp = TRAMPOLINE - KERNEL_STACK_SIZE * new_pid;
		new_pcb.state = ProcessState::READY;
		new_pcb.pid = new_pid;
		
		let new_proc=Arc::new(Process::new(new_pcb));
		pcb.children.alive.insert(new_pid, new_proc.clone());
		
		let (r,t)=async_task::spawn(user_loop(Arc::new(Thread::new(new_proc.clone()))), |runnable|{TASK_QUEUE.push(runnable);});
		r.schedule();
		t.detach();
		return new_pid as isize;
	}

	pub unsafe fn sys_exec(& self,buf: *mut u8, argv: usize) -> isize {
		let pcb=self.proc.inner.lock();
		let path = translate_str(
				pcb
				.memory_set
				.token(),
			buf,
		);

		let (dir,n)= self.get_abs_path(path);
		let mut path=format!("{}{}",dir,n);

		let mut argvs:Vec<String>=Vec::new();
		let mut argc=0;

		loop {
			let argv_i_ptr = *(self.translate(argv + argc * 8) as *mut usize);
			if (argv_i_ptr == 0) {
				break;
			}
			let argv_i = argv_i_ptr as *mut u8;
            let mut s = translate_str(pcb.memory_set.token(), argv_i);
			argvs.push(s);
			argc+=1;
		}

		if path.ends_with(".sh"){
			argvs.insert(0, "sh".to_string());
			argvs.insert(0, "busybox".to_string());
			path="/busybox".to_string();
		}

		if let Some(inode)=GLOBAL_DENTRY_CACHE.get(&path){
			let mut data=inode.lock();
			let data=data.file_data();
			return match ElfFile::new(&data[..]){
				Ok(elf_file)=> self.exec_from_elf(&elf_file, argvs),
				Err(e)=> {
					println!("[execve] {} : exec error.", path);
					self.sys_exit(-1);
					-1
				},
			}
		}else{
			println!("[execve] {} : not found.", path);
			self.sys_exit(-1);
			return -1;
		}

		// extern "C" {
		// 	fn _app_num();
		// }
		// let num = (_app_num as usize as *const usize).read_volatile();
		// let range = ((0..num).find(|&i| APP_NAMES[i] == path).map(get_location));
		// if (range == None) {
		// 	println!("[execve] {} : not found.", path);
		// 	self.sys_exit(-1);
		// 	return -1;
		// }

		// let (start, end) = range.unwrap();

		// let elf_file: Result<ElfFile, &str> =
		// 	ElfFile::new(slice::from_raw_parts(start as *const u8, end - start));
		// match elf_file {
		// 	Ok(elf) => self.exec_from_elf(&elf, argv),
		// 	Err(e) => -1,
		// }
	}

	pub async fn async_yield(){
		YieldFuture(false).await
	}

	pub async unsafe fn sys_waitpid(&self, pid: isize, status:UserPtr<isize,Out>, options: usize) -> isize {
		let mut pcb_lock=self.proc.inner.lock();
		let mut pcb=pcb_lock.deref_mut();
		
		if PRINT_SYSCALL {println!("[waitpid] {} is waiting {} ,flag={}.",pcb.pid,pid,options);}
		let nowpid = pcb.pid;
		if pcb.children.alive.len()+pcb.children.zombie.len() ==0 {
			if options > 0{
				return 0;
			}
			return -1;
		}
		if (pid == -1) {
			loop {
				let pid={
					let mut children= &mut pcb.children.zombie;
					self.proc.inner.force_unlock();
						
						while children.is_empty() {
							if options > 0{
								return 0;
							}
							Thread::async_yield().await;
						}

					let mut pcb_lock = self.proc.inner.lock();
					let (pid,process) = children.first_key_value().unwrap();
					if (status.as_usize() as usize != 0) {
						let status=status.raw_ptr_mut();
						*status = (process.inner.lock().exit_code << 8) | (0);
					}
					// println!("{} cleand {}",pcb.pid,*pid);
					*pid
				};
				let mut children= &mut pcb.children.zombie;
				children.remove_entry(&pid);
				return pid as isize;
			}
		} else {
			let mut children= &mut pcb.children.zombie;
			if let Some(process) = children.get(&(pid as usize)){
				if (status.as_usize() as usize != 0) {
					let status=status.raw_ptr_mut();
					*status = (process.inner.lock().exit_code << 8) | (0);
				}
				children.remove(&(pid as usize) );
			}else{
				return -1;
			}
		}
		0
	}
}


lazy_static! {
    ///All of app's name
    static ref APP_NAMES: Vec<&'static str> = unsafe{
        extern "C" {
            fn _app_num();
            fn _app_names();
        }
        let num_app = (_app_num as usize as *const usize).read_volatile();
        let mut start = _app_names as usize as *const u8;
        let mut v = Vec::new();
        for _ in 0..num_app {
            let mut end = start;
            while end.read_volatile() != b'\0' {
                end = end.add(1);
            }
            let slice = core::slice::from_raw_parts(start, end as usize - start as usize);
            let str = core::str::from_utf8(slice).unwrap();
            v.push(str);
            start = end.add(1);
        }
        v
    };
}

fn get_location(id: usize) -> (usize, usize) {
    extern "C" {
        fn _app_num();
    }
    unsafe {
        let start = (_app_num as usize as *const usize)
            .add(id + 1)
            .read_volatile();
        let end = (_app_num as usize as *const usize)
            .add(id + 2)
            .read_volatile();
        (start, end)
    }
}