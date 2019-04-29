use crate::rcore_fs::util::*;
use crate::rcore_fs::vfs::Timespec;

pub mod std_impl;

/// A current time provider
pub trait TimeProvider: Send + Sync {
    fn current_time(&self) -> Timespec;
}

/// Interface for FS to read & write
///     TODO: use std::io::{Read, Write}
pub trait Device: Send + Sync {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize>;
    fn write_at(&self, offset: usize, buf: &[u8]) -> Option<usize>;
}

/// Device which can only R/W in blocks
pub trait BlockDevice: Send + Sync {
    const BLOCK_SIZE_LOG2: u8;
    fn read_at(&self, block_id: usize, buf: &mut [u8]) -> bool;
    fn write_at(&self, block_id: usize, buf: &[u8]) -> bool;
}

macro_rules! try0 {
    ($len:expr, $res:expr) => {
        if !$res {
            return Some($len);
        }
    };
}

/// Helper functions to R/W BlockDevice in bytes
impl<T: BlockDevice> Device for T {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        let iter = BlockIter {
            begin: offset,
            end: offset + buf.len(),
            block_size_log2: Self::BLOCK_SIZE_LOG2,
        };

        // For each block
        for range in iter {
            let len = range.origin_begin() - offset;
            let buf = &mut buf[range.origin_begin() - offset..range.origin_end() - offset];
            if range.is_full() {
                // Read to target buf directly
                try0!(len, BlockDevice::read_at(self, range.block, buf));
            } else {
                use core::mem::uninitialized;
                let mut block_buf: [u8; 4096] = unsafe { uninitialized() };
                assert!(Self::BLOCK_SIZE_LOG2 <= 12);
                // Read to local buf first
                try0!(len, BlockDevice::read_at(self, range.block, &mut block_buf));
                // Copy to target buf then
                buf.copy_from_slice(&mut block_buf[range.begin..range.end]);
            }
        }
        Some(buf.len())
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Option<usize> {
        let iter = BlockIter {
            begin: offset,
            end: offset + buf.len(),
            block_size_log2: Self::BLOCK_SIZE_LOG2,
        };

        // For each block
        for range in iter {
            let len = range.origin_begin() - offset;
            let buf = &buf[range.origin_begin() - offset..range.origin_end() - offset];
            if range.is_full() {
                // Write to target buf directly
                try0!(len, BlockDevice::write_at(self, range.block, buf));
            } else {
                use core::mem::uninitialized;
                let mut block_buf: [u8; 4096] = unsafe { uninitialized() };
                assert!(Self::BLOCK_SIZE_LOG2 <= 12);
                // Read to local buf first
                try0!(len, BlockDevice::read_at(self, range.block, &mut block_buf));
                // Write to local buf
                block_buf[range.begin..range.end].copy_from_slice(buf);
                // Write back to target buf
                try0!(len, BlockDevice::write_at(self, range.block, &block_buf));
            }
        }
        Some(buf.len())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Mutex;

    impl BlockDevice for Mutex<[u8; 16]> {
        const BLOCK_SIZE_LOG2: u8 = 2;
        fn read_at(&self, block_id: usize, buf: &mut [u8]) -> bool {
            if block_id >= 4 {
                return false;
            }
            let begin = block_id << 2;
            buf[..4].copy_from_slice(&mut self.lock().unwrap()[begin..begin + 4]);
            true
        }
        fn write_at(&self, block_id: usize, buf: &[u8]) -> bool {
            if block_id >= 4 {
                return false;
            }
            let begin = block_id << 2;
            self.lock().unwrap()[begin..begin + 4].copy_from_slice(&buf[..4]);
            true
        }
    }

    #[test]
    fn read() {
        let buf: Mutex<[u8; 16]> =
            Mutex::new([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        let mut res: [u8; 6] = [0; 6];

        // all inside
        let ret = Device::read_at(&buf, 3, &mut res);
        assert_eq!(ret, Some(6));
        assert_eq!(res, [3, 4, 5, 6, 7, 8]);

        // partly inside
        let ret = Device::read_at(&buf, 11, &mut res);
        assert_eq!(ret, Some(5));
        assert_eq!(res, [11, 12, 13, 14, 15, 8]);

        // all outside
        let ret = Device::read_at(&buf, 16, &mut res);
        assert_eq!(ret, Some(0));
        assert_eq!(res, [11, 12, 13, 14, 15, 8]);
    }

    #[test]
    fn write() {
        let buf: Mutex<[u8; 16]> = Mutex::new([0; 16]);
        let res: [u8; 6] = [3, 4, 5, 6, 7, 8];

        // all inside
        let ret = Device::write_at(&buf, 3, &res);
        assert_eq!(ret, Some(6));
        assert_eq!(
            *buf.lock().unwrap(),
            [0, 0, 0, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0]
        );

        // partly inside
        let ret = Device::write_at(&buf, 11, &res);
        assert_eq!(ret, Some(5));
        assert_eq!(
            *buf.lock().unwrap(),
            [0, 0, 0, 3, 4, 5, 6, 7, 8, 0, 0, 3, 4, 5, 6, 7]
        );

        // all outside
        let ret = Device::write_at(&buf, 16, &res);
        assert_eq!(ret, Some(0));
        assert_eq!(
            *buf.lock().unwrap(),
            [0, 0, 0, 3, 4, 5, 6, 7, 8, 0, 0, 3, 4, 5, 6, 7]
        );
    }
}
