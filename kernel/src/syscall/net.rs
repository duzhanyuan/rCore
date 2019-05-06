//! Syscalls for networking

use super::fs::IoVecs;
use super::*;
use crate::drivers::SOCKET_ACTIVITY;
use crate::fs::FileLike;
use crate::net::{
    Endpoint, LinkLevelEndpoint, NetlinkEndpoint, NetlinkSocketState, PacketSocketState,
    RawSocketState, Socket, TcpSocketState, UdpSocketState, SOCKETS,
};
use crate::sync::{MutexGuard, SpinNoIrq, SpinNoIrqLock as Mutex};
use alloc::boxed::Box;
use core::cmp::min;
use core::mem::size_of;
use smoltcp::wire::*;

pub fn sys_socket(domain: usize, socket_type: usize, protocol: usize) -> SysResult {
    let domain = AddressFamily::from(domain as u16);
    let socket_type = SocketType::from(socket_type as u8 & SOCK_TYPE_MASK);
    info!(
        "socket: domain: {:?}, socket_type: {:?}, protocol: {}",
        domain, socket_type, protocol
    );
    let mut proc = process();
    let socket: Box<dyn Socket> = match domain {
        AddressFamily::Internet | AddressFamily::Unix => match socket_type {
            SocketType::Stream => Box::new(TcpSocketState::new()),
            SocketType::Datagram => Box::new(UdpSocketState::new()),
            SocketType::Raw => Box::new(RawSocketState::new(protocol as u8)),
            _ => return Err(SysError::EINVAL),
        },
        AddressFamily::Packet => match socket_type {
            SocketType::Raw => Box::new(PacketSocketState::new()),
            _ => return Err(SysError::EINVAL),
        },
        AddressFamily::Netlink => match socket_type {
            SocketType::Raw => Box::new(NetlinkSocketState::new()),
            _ => return Err(SysError::EINVAL),
        },
        _ => return Err(SysError::EAFNOSUPPORT),
    };
    let fd = proc.get_free_fd();
    proc.files.insert(fd, FileLike::Socket(socket));
    Ok(fd)
}

pub fn sys_setsockopt(
    fd: usize,
    level: usize,
    optname: usize,
    optval: *const u8,
    optlen: usize,
) -> SysResult {
    info!(
        "setsockopt: fd: {}, level: {}, optname: {}",
        fd, level, optname
    );
    let mut proc = process();
    proc.vm.check_read_array(optval, optlen)?;
    let data = unsafe { slice::from_raw_parts(optval, optlen) };
    let socket = proc.get_socket(fd)?;
    socket.setsockopt(level, optname, data)
}

pub fn sys_getsockopt(
    fd: usize,
    level: usize,
    optname: usize,
    optval: *mut u8,
    optlen: *mut u32,
) -> SysResult {
    info!(
        "getsockopt: fd: {}, level: {}, optname: {} optval: {:?} optlen: {:?}",
        fd, level, optname, optval, optlen
    );
    let proc = process();
    proc.vm.check_write_ptr(optlen)?;
    match level {
        SOL_SOCKET => match optname {
            SO_SNDBUF => {
                proc.vm.check_write_array(optval, 4)?;
                unsafe {
                    *(optval as *mut u32) = crate::net::TCP_SENDBUF as u32;
                    *optlen = 4;
                }
                Ok(0)
            }
            SO_RCVBUF => {
                proc.vm.check_write_array(optval, 4)?;
                unsafe {
                    *(optval as *mut u32) = crate::net::TCP_RECVBUF as u32;
                    *optlen = 4;
                }
                Ok(0)
            }
            _ => Err(SysError::ENOPROTOOPT),
        },
        IPPROTO_TCP => match optname {
            TCP_CONGESTION => Ok(0),
            _ => Err(SysError::ENOPROTOOPT),
        },
        _ => Err(SysError::ENOPROTOOPT),
    }
}

pub fn sys_connect(fd: usize, addr: *const SockAddr, addr_len: usize) -> SysResult {
    info!(
        "sys_connect: fd: {}, addr: {:?}, addr_len: {}",
        fd, addr, addr_len
    );

    let mut proc = process();
    let endpoint = sockaddr_to_endpoint(&mut proc, addr, addr_len)?;
    let socket = proc.get_socket(fd)?;
    socket.connect(endpoint)?;
    Ok(0)
}

pub fn sys_sendto(
    fd: usize,
    base: *const u8,
    len: usize,
    _flags: usize,
    addr: *const SockAddr,
    addr_len: usize,
) -> SysResult {
    info!(
        "sys_sendto: fd: {} base: {:?} len: {} addr: {:?} addr_len: {}",
        fd, base, len, addr, addr_len
    );

    let mut proc = process();
    proc.vm.check_read_array(base, len)?;

    let slice = unsafe { slice::from_raw_parts(base, len) };
    let endpoint = if addr.is_null() {
        None
    } else {
        let endpoint = sockaddr_to_endpoint(&mut proc, addr, addr_len)?;
        info!("sys_sendto: sending to endpoint {:?}", endpoint);
        Some(endpoint)
    };
    let socket = proc.get_socket(fd)?;
    socket.write(&slice, endpoint)
}

pub fn sys_recvfrom(
    fd: usize,
    base: *mut u8,
    len: usize,
    flags: usize,
    addr: *mut SockAddr,
    addr_len: *mut u32,
) -> SysResult {
    info!(
        "sys_recvfrom: fd: {} base: {:?} len: {} flags: {} addr: {:?} addr_len: {:?}",
        fd, base, len, flags, addr, addr_len
    );

    let mut proc = process();
    proc.vm.check_write_array(base, len)?;

    let socket = proc.get_socket(fd)?;
    let mut slice = unsafe { slice::from_raw_parts_mut(base, len) };
    let (result, endpoint) = socket.read(&mut slice);

    if result.is_ok() && !addr.is_null() {
        let sockaddr_in = SockAddr::from(endpoint);
        unsafe {
            sockaddr_in.write_to(&mut proc, addr, addr_len)?;
        }
    }

    result
}

pub fn sys_recvmsg(fd: usize, msg: *mut MsgHdr, flags: usize) -> SysResult {
    info!("recvmsg: fd: {}, msg: {:?}, flags: {}", fd, msg, flags);
    let mut proc = process();
    proc.vm.check_read_ptr(msg)?;
    let hdr = unsafe { &mut *msg };
    let mut iovs = IoVecs::check_and_new(hdr.msg_iov, hdr.msg_iovlen, &proc.vm, true)?;

    let mut buf = iovs.new_buf(true);
    let socket = proc.get_socket(fd)?;
    let (result, endpoint) = socket.read(&mut buf);

    if let Ok(len) = result {
        // copy data to user
        iovs.write_all_from_slice(&buf[..len]);
        let sockaddr_in = SockAddr::from(endpoint);
        unsafe {
            sockaddr_in.write_to(&mut proc, hdr.msg_name, &mut hdr.msg_namelen as *mut u32)?;
        }
    }
    result
}

pub fn sys_bind(fd: usize, addr: *const SockAddr, addr_len: usize) -> SysResult {
    info!("sys_bind: fd: {} addr: {:?} len: {}", fd, addr, addr_len);
    let mut proc = process();

    let mut endpoint = sockaddr_to_endpoint(&mut proc, addr, addr_len)?;
    info!("sys_bind: fd: {} bind to {:?}", fd, endpoint);

    let socket = proc.get_socket(fd)?;
    socket.bind(endpoint)
}

pub fn sys_listen(fd: usize, backlog: usize) -> SysResult {
    info!("sys_listen: fd: {} backlog: {}", fd, backlog);
    // smoltcp tcp sockets do not support backlog
    // open multiple sockets for each connection
    let mut proc = process();

    let socket = proc.get_socket(fd)?;
    socket.listen()
}

pub fn sys_shutdown(fd: usize, how: usize) -> SysResult {
    info!("sys_shutdown: fd: {} how: {}", fd, how);
    let mut proc = process();

    let socket = proc.get_socket(fd)?;
    socket.shutdown()
}

pub fn sys_accept(fd: usize, addr: *mut SockAddr, addr_len: *mut u32) -> SysResult {
    info!(
        "sys_accept: fd: {} addr: {:?} addr_len: {:?}",
        fd, addr, addr_len
    );
    // smoltcp tcp sockets do not support backlog
    // open multiple sockets for each connection
    let mut proc = process();

    let socket = proc.get_socket(fd)?;
    let (new_socket, remote_endpoint) = socket.accept()?;

    let new_fd = proc.get_free_fd();
    proc.files.insert(new_fd, FileLike::Socket(new_socket));

    if !addr.is_null() {
        let sockaddr_in = SockAddr::from(remote_endpoint);
        unsafe {
            sockaddr_in.write_to(&mut proc, addr, addr_len)?;
        }
    }
    Ok(new_fd)
}

pub fn sys_getsockname(fd: usize, addr: *mut SockAddr, addr_len: *mut u32) -> SysResult {
    info!(
        "sys_getsockname: fd: {} addr: {:?} addr_len: {:?}",
        fd, addr, addr_len
    );

    let mut proc = process();

    if addr.is_null() {
        return Err(SysError::EINVAL);
    }

    let socket = proc.get_socket(fd)?;
    let endpoint = socket.endpoint().ok_or(SysError::EINVAL)?;
    let sockaddr_in = SockAddr::from(endpoint);
    unsafe {
        sockaddr_in.write_to(&mut proc, addr, addr_len)?;
    }
    Ok(0)
}

pub fn sys_getpeername(fd: usize, addr: *mut SockAddr, addr_len: *mut u32) -> SysResult {
    info!(
        "sys_getpeername: fd: {} addr: {:?} addr_len: {:?}",
        fd, addr, addr_len
    );

    // smoltcp tcp sockets do not support backlog
    // open multiple sockets for each connection
    let mut proc = process();

    if addr as usize == 0 {
        return Err(SysError::EINVAL);
    }

    let socket = proc.get_socket(fd)?;
    let remote_endpoint = socket.remote_endpoint().ok_or(SysError::EINVAL)?;
    let sockaddr_in = SockAddr::from(remote_endpoint);
    unsafe {
        sockaddr_in.write_to(&mut proc, addr, addr_len)?;
    }
    Ok(0)
}

impl Process {
    fn get_socket(&mut self, fd: usize) -> Result<&mut Box<dyn Socket>, SysError> {
        match self.get_file_like(fd)? {
            FileLike::Socket(socket) => Ok(socket),
            _ => Err(SysError::EBADF),
        }
    }
}

#[repr(C)]
pub struct SockAddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

#[repr(C)]
pub struct SockAddrUn {
    pub sun_family: u16,
    pub sun_path: [u8; 108],
}

#[repr(C)]
pub struct SockAddrLl {
    pub sll_family: u16,
    pub sll_protocol: u16,
    pub sll_ifindex: u32,
    pub sll_hatype: u16,
    pub sll_pkttype: u8,
    pub sll_halen: u8,
    pub sll_addr: [u8; 8],
}

#[repr(C)]
pub struct SockAddrNl {
    nl_family: u16,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

#[repr(C)]
pub union SockAddr {
    pub family: u16,
    pub addr_in: SockAddrIn,
    pub addr_un: SockAddrUn,
    pub addr_ll: SockAddrLl,
    pub addr_nl: SockAddrNl,
    pub addr_ph: SockAddrPlaceholder,
}

#[repr(C)]
pub struct SockAddrPlaceholder {
    pub family: u16,
    pub data: [u8; 14],
}

impl From<Endpoint> for SockAddr {
    fn from(endpoint: Endpoint) -> Self {
        if let Endpoint::Ip(ip) = endpoint {
            match ip.addr {
                IpAddress::Ipv4(ipv4) => SockAddr {
                    addr_in: SockAddrIn {
                        sin_family: AddressFamily::Internet.into(),
                        sin_port: u16::to_be(ip.port),
                        sin_addr: u32::to_be(u32::from_be_bytes(ipv4.0)),
                        sin_zero: [0; 8],
                    },
                },
                IpAddress::Unspecified => SockAddr {
                    addr_ph: SockAddrPlaceholder {
                        family: AddressFamily::Unspecified.into(),
                        data: [0; 14],
                    },
                },
                _ => unimplemented!("only ipv4"),
            }
        } else if let Endpoint::LinkLevel(link_level) = endpoint {
            SockAddr {
                addr_ll: SockAddrLl {
                    sll_family: AddressFamily::Packet.into(),
                    sll_protocol: 0,
                    sll_ifindex: link_level.interface_index as u32,
                    sll_hatype: 0,
                    sll_pkttype: 0,
                    sll_halen: 0,
                    sll_addr: [0; 8],
                },
            }
        } else if let Endpoint::Netlink(netlink) = endpoint {
            SockAddr {
                addr_nl: SockAddrNl {
                    nl_family: AddressFamily::Netlink.into(),
                    nl_pad: 0,
                    nl_pid: netlink.port_id,
                    nl_groups: netlink.multicast_groups_mask,
                },
            }
        } else {
            unimplemented!("only ip");
        }
    }
}

/// Convert sockaddr to endpoint
// Check len is long enough
fn sockaddr_to_endpoint(
    proc: &mut Process,
    addr: *const SockAddr,
    len: usize,
) -> Result<Endpoint, SysError> {
    if len < size_of::<u16>() {
        return Err(SysError::EINVAL);
    }
    proc.vm.check_read_array(addr as *const u8, len)?;
    unsafe {
        match AddressFamily::from((*addr).family) {
            AddressFamily::Internet => {
                if len < size_of::<SockAddrIn>() {
                    return Err(SysError::EINVAL);
                }
                let port = u16::from_be((*addr).addr_in.sin_port);
                let addr = IpAddress::from(Ipv4Address::from_bytes(
                    &u32::from_be((*addr).addr_in.sin_addr).to_be_bytes()[..],
                ));
                Ok(Endpoint::Ip((addr, port).into()))
            }
            AddressFamily::Unix => Err(SysError::EINVAL),
            AddressFamily::Packet => {
                if len < size_of::<SockAddrLl>() {
                    return Err(SysError::EINVAL);
                }
                Ok(Endpoint::LinkLevel(LinkLevelEndpoint::new(
                    (*addr).addr_ll.sll_ifindex as usize,
                )))
            }
            AddressFamily::Netlink => {
                if len < size_of::<SockAddrNl>() {
                    return Err(SysError::EINVAL);
                }
                Ok(Endpoint::Netlink(NetlinkEndpoint::new(
                    (*addr).addr_nl.nl_pid,
                    (*addr).addr_nl.nl_groups,
                )))
            }
            _ => Err(SysError::EINVAL),
        }
    }
}

impl SockAddr {
    /// Write to user sockaddr
    /// Check mutability for user
    unsafe fn write_to(
        self,
        proc: &mut Process,
        addr: *mut SockAddr,
        addr_len: *mut u32,
    ) -> SysResult {
        // Ignore NULL
        if addr.is_null() {
            return Ok(0);
        }

        proc.vm.check_write_ptr(addr_len)?;
        let max_addr_len = *addr_len as usize;
        let full_len = match AddressFamily::from(self.family) {
            AddressFamily::Internet => size_of::<SockAddrIn>(),
            AddressFamily::Packet => size_of::<SockAddrLl>(),
            AddressFamily::Netlink => size_of::<SockAddrNl>(),
            AddressFamily::Unix => return Err(SysError::EINVAL),
            _ => return Err(SysError::EINVAL),
        };

        let written_len = min(max_addr_len, full_len);
        if written_len > 0 {
            proc.vm.check_write_array(addr as *mut u8, written_len)?;
            let source = slice::from_raw_parts(&self as *const SockAddr as *const u8, written_len);
            let target = slice::from_raw_parts_mut(addr as *mut u8, written_len);
            target.copy_from_slice(source);
        }
        addr_len.write(full_len as u32);
        return Ok(0);
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct MsgHdr {
    msg_name: *mut SockAddr,
    msg_namelen: u32,
    msg_iov: *mut IoVec,
    msg_iovlen: usize,
    msg_control: usize,
    msg_controllen: usize,
    msg_flags: usize,
}

enum_with_unknown! {
    /// Address families
    pub doc enum AddressFamily(u16) {
        /// Unspecified
        Unspecified = 0,
        /// Unix domain sockets
        Unix = 1,
        /// Internet IP Protocol
        Internet = 2,
        /// Netlink
        Netlink = 16,
        /// Packet family
        Packet = 17,
    }
}

const SOCK_TYPE_MASK: u8 = 0xf;

enum_with_unknown! {
    /// Socket types
    pub doc enum SocketType(u8) {
        /// Stream
        Stream = 1,
        /// Datagram
        Datagram = 2,
        /// Raw
        Raw = 3,
    }
}

const IPPROTO_IP: usize = 0;
const IPPROTO_ICMP: usize = 1;
const IPPROTO_TCP: usize = 6;

const SOL_SOCKET: usize = 1;
const SO_SNDBUF: usize = 7;
const SO_RCVBUF: usize = 8;
const SO_LINGER: usize = 13;

const TCP_CONGESTION: usize = 13;

const IP_HDRINCL: usize = 3;
