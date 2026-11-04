//! Match a live UNIX socket's kernel inode to its bound filesystem identity.
//! /proc/net/unix supplies the name, but a name alone cannot establish ownership.
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub(super) fn filesystem_identities() -> anyhow::Result<HashMap<u32, (u32, u32)>> {
    // Linux UAPI: unix_diag_req, unix_diag_msg, UNIX_DIAG_VFS.
    // SAFETY: scalar socket arguments; returned descriptor is owned below.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_SOCK_DIAG,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: socket returned a fresh descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: 100_000,
    };
    // SAFETY: pointer/length describe the live timeval object.
    if unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&timeout as *const libc::timeval).cast(),
            std::mem::size_of_val(&timeout) as _,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut request = Vec::new();
    request.extend_from_slice(&40u32.to_ne_bytes());
    request.extend_from_slice(&20u16.to_ne_bytes()); // SOCK_DIAG_BY_FAMILY
    request.extend_from_slice(&0x301u16.to_ne_bytes()); // REQUEST | DUMP
    request.extend_from_slice(&1u32.to_ne_bytes());
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.extend_from_slice(&[libc::AF_UNIX as u8, 0, 0, 0]);
    request.extend_from_slice(&u32::MAX.to_ne_bytes());
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.extend_from_slice(&2u32.to_ne_bytes()); // UDIAG_SHOW_VFS
    request.extend_from_slice(&[0xff; 8]);
    // SAFETY: zeroed sockaddr_nl is initialised with AF_NETLINK and kernel pid 0.
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as u16;
    // SAFETY: buffers and sockaddr lengths refer to live allocated objects.
    if unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            request.as_ptr().cast(),
            request.len(),
            0,
            (&address as *const libc::sockaddr_nl).cast(),
            std::mem::size_of_val(&address) as _,
        )
    } != request.len() as isize
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut result = HashMap::new();
    let mut buffer = vec![0u8; 65536];
    for _ in 0..64 {
        // SAFETY: the buffer is writable for its full length.
        let count = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_TRUNC,
            )
        };
        anyhow::ensure!(
            count > 0 && count as usize <= buffer.len(),
            "socket identity inspection incomplete"
        );
        let mut offset = 0;
        while offset + 16 <= count as usize {
            let bytes = &buffer[offset..count as usize];
            let length = u32::from_ne_bytes(bytes[..4].try_into()?) as usize;
            let kind = u16::from_ne_bytes(bytes[4..6].try_into()?);
            let flags = u16::from_ne_bytes(bytes[6..8].try_into()?);
            anyhow::ensure!(
                length >= 16 && length <= bytes.len() && flags & 0x10 == 0,
                "socket identity dump interrupted"
            );
            if kind == 3 {
                return Ok(result);
            } // NLMSG_DONE
            anyhow::ensure!(kind != 2, "socket identity query refused");
            if kind == 20 && length >= 32 {
                let inode = u32::from_ne_bytes(bytes[20..24].try_into()?);
                let mut attribute = 32;
                while attribute + 4 <= length {
                    let size =
                        u16::from_ne_bytes(bytes[attribute..attribute + 2].try_into()?) as usize;
                    let tag = u16::from_ne_bytes(bytes[attribute + 2..attribute + 4].try_into()?);
                    anyhow::ensure!(
                        size >= 4 && attribute + size <= length,
                        "invalid socket identity attribute"
                    );
                    if tag == 1 && size == 12 {
                        result.insert(
                            inode,
                            (
                                u32::from_ne_bytes(bytes[attribute + 4..attribute + 8].try_into()?),
                                u32::from_ne_bytes(
                                    bytes[attribute + 8..attribute + 12].try_into()?,
                                ),
                            ),
                        );
                    }
                    attribute += (size + 3) & !3;
                }
            }
            offset += (length + 3) & !3;
        }
    }
    anyhow::bail!("socket identity inspection bound exhausted")
}

pub(super) fn encoded_device(device: u64) -> u32 {
    let major = libc::major(device) as u32;
    let minor = libc::minor(device) as u32;
    // unix_diag_vfs uses the kernel dev_t layout (12-bit major, 20-bit minor).
    (major << 20) | minor
}
