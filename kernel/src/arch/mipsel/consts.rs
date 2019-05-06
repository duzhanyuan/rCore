/// Platform specific constants
///
pub use super::board::consts::*;

pub const KERNEL_OFFSET: usize = 0x80000000;

pub const MEMORY_OFFSET: usize = 0x8000_0000;

pub const USER_STACK_OFFSET: usize = 0x70000000 - USER_STACK_SIZE;
pub const USER_STACK_SIZE: usize = 0x10000;
pub const USER32_STACK_OFFSET: usize = 0x70000000 - USER_STACK_SIZE;

pub const MAX_DTB_SIZE: usize = 0x2000;
