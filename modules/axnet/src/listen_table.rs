use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec};
use core::ops::DerefMut;

use axerrno::{LinuxError, LinuxResult};
use axsync::Mutex;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::tcp::{self, SocketBuffer, State},
    wire::{IpAddress, IpEndpoint, IpListenEndpoint},
};

use crate::{
    SOCKET_SET,
    consts::{LISTEN_QUEUE_SIZE, TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
};

const PORT_NUM: usize = 65536;

struct ListenTableEntry {
    listen_endpoint: IpListenEndpoint,
    syn_queue: VecDeque<SocketHandle>,
}

impl ListenTableEntry {
    pub fn new(listen_endpoint: IpListenEndpoint) -> Self {
        Self {
            listen_endpoint,
            syn_queue: VecDeque::with_capacity(LISTEN_QUEUE_SIZE),
        }
    }

    #[inline]
    fn can_accept(&self, dst: IpAddress) -> bool {
        match self.listen_endpoint.addr {
            Some(addr) => addr == dst,
            None => true,
        }
    }
}

impl Drop for ListenTableEntry {
    fn drop(&mut self) {
        for &handle in &self.syn_queue {
            SOCKET_SET.remove(handle);
        }
    }
}

pub struct ListenTable {
    tcp: Box<[Arc<Mutex<Option<Box<ListenTableEntry>>>>]>,
}

impl ListenTable {
    pub fn new() -> Self {
        let tcp = unsafe {
            let mut buf = Box::new_uninit_slice(PORT_NUM);
            for i in 0..PORT_NUM {
                buf[i].write(Arc::default());
            }
            buf.assume_init()
        };
        Self { tcp }
    }

    pub fn can_listen(&self, port: u16) -> bool {
        self.tcp[port as usize].lock().is_none()
    }

    pub fn listen(&self, listen_endpoint: IpListenEndpoint) -> LinuxResult {
        let port = listen_endpoint.port;
        assert_ne!(port, 0);
        let mut entry = self.tcp[port as usize].lock();
        if entry.is_none() {
            *entry = Some(Box::new(ListenTableEntry::new(listen_endpoint)));
            Ok(())
        } else {
            warn!("socket already listening on port {port}");
            Err(LinuxError::EADDRINUSE)
        }
    }

    pub fn unlisten(&self, port: u16) {
        debug!("TCP socket unlisten on {}", port);
        *self.tcp[port as usize].lock() = None;
    }

    fn listen_entry(&self, port: u16) -> Arc<Mutex<Option<Box<ListenTableEntry>>>> {
        self.tcp[port as usize].clone()
    }

    pub fn can_accept(&self, port: u16) -> LinuxResult<bool> {
        if let Some(entry) = self.listen_entry(port).lock().as_ref() {
            Ok(entry.syn_queue.iter().any(|&handle| is_connected(handle)))
        } else {
            warn!("accept before listen");
            Err(LinuxError::EINVAL)
        }
    }

    pub fn accept(&self, port: u16) -> LinuxResult<(SocketHandle, (IpEndpoint, IpEndpoint))> {
        let entry = self.listen_entry(port);
        let mut table = entry.lock();
        let Some(entry) = table.deref_mut() else {
            warn!("accept before listen");
            return Err(LinuxError::EINVAL);
        };

        let syn_queue: &mut VecDeque<SocketHandle> = &mut entry.syn_queue;
        let idx = syn_queue
            .iter()
            .enumerate()
            .find_map(|(idx, &handle)| is_connected(handle).then_some(idx))
            .ok_or(LinuxError::EAGAIN)?; // wait for connection
        if idx > 0 {
            warn!(
                "slow SYN queue enumeration: index = {}, len = {}!",
                idx,
                syn_queue.len()
            );
        }
        let handle = syn_queue.swap_remove_front(idx).unwrap();
        // If the connection is reset, return ConnectionReset error
        // Otherwise, return the handle and the address tuple
        if is_closed(handle) {
            warn!("accept failed: connection reset");
            Err(LinuxError::ECONNRESET)
        } else {
            Ok((handle, get_addr_tuple(handle)))
        }
    }

    pub fn incoming_tcp_packet(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        sockets: &mut SocketSet<'_>,
    ) {
        if let Some(entry) = self.listen_entry(dst.port).lock().deref_mut() {
            if !entry.can_accept(dst.addr) {
                // not listening on this address
                return;
            }
            if entry.syn_queue.len() >= LISTEN_QUEUE_SIZE {
                // SYN queue is full, drop the packet
                warn!("SYN queue overflow!");
                return;
            }

            let mut socket = smoltcp::socket::tcp::Socket::new(
                SocketBuffer::new(vec![0; TCP_RX_BUF_LEN]),
                SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
            );
            if socket.listen(entry.listen_endpoint).is_ok() {
                let handle = sockets.add(socket);
                debug!(
                    "TCP socket {}: prepare for connection {} -> {}",
                    handle, src, entry.listen_endpoint
                );
                entry.syn_queue.push_back(handle);
            }
        }
    }
}

fn is_connected(handle: SocketHandle) -> bool {
    SOCKET_SET.with_socket::<tcp::Socket, _, _>(handle, |socket| {
        !matches!(socket.state(), State::Listen | State::SynReceived)
    })
}

fn is_closed(handle: SocketHandle) -> bool {
    SOCKET_SET
        .with_socket::<tcp::Socket, _, _>(handle, |socket| matches!(socket.state(), State::Closed))
}

fn get_addr_tuple(handle: SocketHandle) -> (IpEndpoint, IpEndpoint) {
    SOCKET_SET.with_socket::<tcp::Socket, _, _>(handle, |socket| {
        (
            socket.local_endpoint().unwrap(),
            socket.remote_endpoint().unwrap(),
        )
    })
}
