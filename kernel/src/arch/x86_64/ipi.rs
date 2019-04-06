// Interface for inter-processor interrupt.
// This module wraps inter-processor interrupt into a broadcast-calling style.

use apic::{XApic, LAPIC_ADDR, LocalApic};
use crate::consts::KERNEL_OFFSET;
use lazy_static::*;
use crate::sync::{SpinLock as Mutex, Semaphore};
use alloc::sync::Arc;
use alloc::boxed::Box;
use rcore_memory::Page;
use x86_64::instructions::tlb;
use x86_64::VirtAddr;

struct IPIInvoke<'a,A>(&'a (Fn(&A)->()), &'a A);

lazy_static! {
    static ref IPI_INVOKE_LOCK: Mutex<()>=Mutex::new(());
}

pub trait InvokeEventHandle{
    fn call(&self);
}

struct InvokeEvent<A: 'static> {
    function: fn(&A)->(),
    argument: Arc<A>,
    done_semaphore: Arc<Semaphore>
}

impl<A> InvokeEventHandle for InvokeEvent<A>{
    fn call(&self){
        let arg_ref=self.argument.as_ref();
        (self.function)(arg_ref);
        println!("Release!");
        self.done_semaphore.release();
        println!("Released!");
    }
}

pub type IPIEventItem=Box<InvokeEventHandle>;

// TODO: something fishy is going on here...
// In fact, the argument lives as long as the Arc.
fn createItem<A: 'static>(f: fn(&A)->(), arg: &Arc<A>, sem: &Arc<Semaphore>)->IPIEventItem{
    Box::new(InvokeEvent{
        function: f,
        argument: arg.clone(),
        done_semaphore: sem.clone()
    })
}
unsafe fn get_apic()->XApic{
    let mut lapic = unsafe { XApic::new(KERNEL_OFFSET + LAPIC_ADDR) };
    lapic
}
pub fn invoke_on_allcpu<A: 'static>(f: fn(&A)->(), arg: A , wait: bool){
    println!("Step 1");
    use super::interrupt::consts::IPIFuncCall;
    let mut apic=unsafe{ get_apic()};
    let sem=Arc::new(Semaphore::new(0));
    let arcarg=Arc::new(arg);
    let mut cpu_count=0;
    println!("Step 2");
    super::gdt::Cpu::foreach(|cpu| {
        let id=cpu.get_id();
        println!("Sending interrupt to cpu {} from {}", id, super::cpu::id());
        cpu_count+=1;
        cpu.notify_event(createItem(f, &arcarg, &sem));
        apic.send_ipi(id as u8, IPIFuncCall);
    });
    if wait{
        for _ in 0..cpu_count{
            println!("Acquire!");
            sem.acquire();
            println!("Acquired!");
        }
    }
}

// Examples of such cases.

pub fn tlb_shootdown(tuple: &(usize, usize)){
    let (start_addr, end_addr)=*tuple;
    for p in Page::range_of(start_addr,end_addr){
        tlb::flush(VirtAddr::new(p.start_address() as u64));

    }
}