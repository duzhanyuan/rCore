//! Syscalls for process

use super::*;
use crate::fs::INodeExt;

/// Fork the current process. Return the child's PID.
pub fn sys_fork(tf: &TrapFrame) -> SysResult {
    let new_thread = current_thread().fork(tf);
    let pid = processor().manager().add(new_thread);
    info!("fork: {} -> {}", thread::current().id(), pid);
    Ok(pid)
}

/// Create a new thread in the current process.
/// The new thread's stack pointer will be set to `newsp`,
///   and thread pointer will be set to `newtls`.
/// The child tid will be stored at both `parent_tid` and `child_tid`.
/// This is partially implemented for musl only.
pub fn sys_clone(
    flags: usize,
    newsp: usize,
    parent_tid: *mut u32,
    child_tid: *mut u32,
    newtls: usize,
    tf: &TrapFrame,
) -> SysResult {
    info!(
        "clone: flags: {:#x}, newsp: {:#x}, parent_tid: {:?}, child_tid: {:?}, newtls: {:#x}",
        flags, newsp, parent_tid, child_tid, newtls
    );
    if flags == 0x4111 {
        warn!("sys_clone is calling sys_fork instead, ignoring other args");
        return sys_fork(tf);
    }
    if flags != 0x7d0f00 {
        warn!("sys_clone only support musl pthread_create");
        return Err(SysError::ENOSYS);
    }
    {
        let proc = process();
        proc.vm.check_write_ptr(parent_tid)?;
        proc.vm.check_write_ptr(child_tid)?;
    }
    let new_thread = current_thread().clone(tf, newsp, newtls, child_tid as usize);
    // FIXME: parent pid
    let tid = processor().manager().add(new_thread);
    info!("clone: {} -> {}", thread::current().id(), tid);
    unsafe {
        parent_tid.write(tid as u32);
        child_tid.write(tid as u32);
    }
    Ok(tid)
}

/// Wait for the process exit.
/// Return the PID. Store exit code to `wstatus` if it's not null.
pub fn sys_wait4(pid: isize, wstatus: *mut i32) -> SysResult {
    info!("wait4: pid: {}, code: {:?}", pid, wstatus);
    if !wstatus.is_null() {
        process().vm.check_write_ptr(wstatus)?;
    }
    #[derive(Debug)]
    enum WaitFor {
        AnyChild,
        Pid(usize),
    }
    let target = match pid {
        -1 | 0 => WaitFor::AnyChild,
        p if p > 0 => WaitFor::Pid(p as usize),
        _ => unimplemented!(),
    };
    loop {
        let mut proc = process();
        // check child_exit_code
        let find = match target {
            WaitFor::AnyChild => proc
                .child_exit_code
                .iter()
                .next()
                .map(|(&pid, &code)| (pid, code)),
            WaitFor::Pid(pid) => proc.child_exit_code.get(&pid).map(|&code| (pid, code)),
        };
        // if found, return
        if let Some((pid, exit_code)) = find {
            proc.child_exit_code.remove(&pid);
            if !wstatus.is_null() {
                unsafe {
                    wstatus.write(exit_code as i32);
                }
            }
            return Ok(pid);
        }
        // if not, check pid
        let children: Vec<_> = proc
            .children
            .iter()
            .filter_map(|weak| weak.upgrade())
            .collect();
        let invalid = match target {
            WaitFor::AnyChild => children.len() == 0,
            WaitFor::Pid(pid) => children
                .iter()
                .find(|p| p.lock().pid.get() == pid)
                .is_none(),
        };
        if invalid {
            return Err(SysError::ECHILD);
        }
        info!(
            "wait: thread {} -> {:?}, sleep",
            thread::current().id(),
            target
        );
        let condvar = proc.child_exit.clone();
        drop(proc); // must release lock of current process
        condvar._wait();
    }
}

pub fn sys_exec(
    name: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
    tf: &mut TrapFrame,
) -> SysResult {
    info!("exec: name: {:?}, argv: {:?} envp: {:?}", name, argv, envp);
    let proc = process();
    let _name = if name.is_null() {
        String::from("")
    } else {
        unsafe { proc.vm.check_and_clone_cstr(name)? }
    };

    if argv.is_null() {
        return Err(SysError::EINVAL);
    }
    // Check and copy args to kernel
    let mut args = Vec::new();
    unsafe {
        let mut current_argv = argv as *const *const u8;
        proc.vm.check_read_ptr(current_argv)?;
        while !(*current_argv).is_null() {
            let arg = proc.vm.check_and_clone_cstr(*current_argv)?;
            args.push(arg);
            current_argv = current_argv.add(1);
        }
    }
    info!("exec: args {:?}", args);

    // Read program file
    let path = args[0].as_str();
    let inode = proc.lookup_inode(path)?;
    let buf = inode.read_as_vec()?;

    // Make new Thread
    let iter = args.iter().map(|s| s.as_str());
    let mut thread = Thread::new_user(buf.as_slice(), iter);
    thread.proc.lock().clone_for_exec(&proc);

    // Activate new page table
    unsafe {
        thread.proc.lock().vm.activate();
    }

    // Modify the TrapFrame
    *tf = unsafe { thread.context.get_init_tf() };

    // Swap Context but keep KStack
    ::core::mem::swap(&mut current_thread().kstack, &mut thread.kstack);
    ::core::mem::swap(current_thread(), &mut *thread);

    Ok(0)
}

pub fn sys_yield() -> SysResult {
    thread::yield_now();
    Ok(0)
}

/// Kill the process
pub fn sys_kill(pid: usize, sig: usize) -> SysResult {
    info!(
        "kill: {} killed: {} with sig {}",
        thread::current().id(),
        pid,
        sig
    );
    let current_pid = process().pid.get().clone();
    if current_pid == pid {
        // killing myself
        sys_exit_group(sig);
    } else {
        if let Some(proc_arc) = PROCESSES.read().get(&pid).and_then(|weak| weak.upgrade()) {
            let proc = proc_arc.lock();
            // quit all threads
            for tid in proc.threads.iter() {
                processor().manager().exit(*tid, sig);
            }
            // notify parent and fill exit code
            // avoid deadlock
            let proc_parent = proc.parent.clone();
            let pid = proc.pid.get();
            drop(proc);
            if let Some(parent) = proc_parent {
                let mut parent = parent.lock();
                parent.child_exit_code.insert(pid, sig);
                parent.child_exit.notify_one();
            }
            Ok(0)
        } else {
            Err(SysError::EINVAL)
        }
    }
}

/// Get the current process id
pub fn sys_getpid() -> SysResult {
    info!("getpid");
    Ok(process().pid.get())
}

/// Get the current thread id
pub fn sys_gettid() -> SysResult {
    info!("gettid");
    // use pid as tid for now
    Ok(thread::current().id())
}

/// Get the parent process id
pub fn sys_getppid() -> SysResult {
    Ok(process().parent.as_ref().unwrap().lock().pid.get())
}

/// Exit the current thread
pub fn sys_exit(exit_code: usize) -> ! {
    let tid = thread::current().id();
    info!("exit: {}, code: {}", tid, exit_code);
    let mut proc = process();
    proc.threads.retain(|&id| id != tid);

    // for last thread,
    // notify parent and fill exit code
    // avoid deadlock
    let exit = proc.threads.len() == 0;
    let proc_parent = proc.parent.clone();
    let pid = proc.pid.get();
    drop(proc);
    if exit {
        if let Some(parent) = proc_parent {
            let mut parent = parent.lock();
            parent.child_exit_code.insert(pid, exit_code);
            parent.child_exit.notify_one();
        }
    }

    // perform futex wake 1
    // ref: http://man7.org/linux/man-pages/man2/set_tid_address.2.html
    // FIXME: do it in all possible ways a thread can exit
    //        it has memory access so we can't move it to Thread::drop?
    let clear_child_tid = current_thread().clear_child_tid;
    if clear_child_tid != 0 {
        unsafe {
            (clear_child_tid as *mut u32).write(0);
        }
        let queue = process().get_futex(clear_child_tid);
        queue.notify_one();
    }

    processor().manager().exit(tid, exit_code as usize);
    processor().yield_now();
    unreachable!();
}

/// Exit the current thread group (i.e. process)
pub fn sys_exit_group(exit_code: usize) -> ! {
    let proc = process();
    info!("exit_group: {}, code: {}", proc.pid, exit_code);

    // quit all threads
    for tid in proc.threads.iter() {
        processor().manager().exit(*tid, exit_code);
    }

    // notify parent and fill exit code
    // avoid deadlock
    let proc_parent = proc.parent.clone();
    let pid = proc.pid.get();
    drop(proc);
    if let Some(parent) = proc_parent {
        let mut parent = parent.lock();
        parent.child_exit_code.insert(pid, exit_code);
        parent.child_exit.notify_one();
    }

    processor().yield_now();
    unreachable!();
}

pub fn sys_nanosleep(req: *const TimeSpec) -> SysResult {
    process().vm.check_read_ptr(req)?;
    let time = unsafe { req.read() };
    info!("nanosleep: time: {:#?}", time);
    // TODO: handle spurious wakeup
    thread::sleep(time.to_duration());
    Ok(0)
}

pub fn sys_set_priority(priority: usize) -> SysResult {
    let pid = thread::current().id();
    processor().manager().set_priority(pid, priority as u8);
    Ok(0)
}
