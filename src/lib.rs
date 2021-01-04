//! `nbd-netlink` supports setting up an NBD device for a specified socket and parameters, using
//! the Linux kernel's netlink interface to NBD. Unlike the `ioctl`-based interface, the netlink
//! interface can hand off a socket to the kernel without leaving a thread or process running.
//!
//! # Example
//!
//! ```no_run
//! use std::net::{Ipv4Addr, TcpStream};
//! use nbd_netlink::{NBD, NBDConnect};
//! let nbd_socket = TcpStream::connect((Ipv4Addr::LOCALHOST, 10809))?;
//! nbd_socket.set_nodelay(true);
//! let mut nbd = NBD::new()?;
//! let index = NBDConnect::new()
//!     .size_bytes(1048576)
//!     .read_only(true)
//!     .connect(&mut nbd, &[nbd_socket])?;
//! # Ok::<(), anyhow::Error>(())
//! ```
#![deny(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!("Netlink only works on Linux");

use std::os::unix::io::AsRawFd;

use anyhow::{anyhow, Context};
use neli::{
    consts::genl::NlAttrType,
    consts::nl::{NlmF, NlmFFlags},
    consts::socket::NlFamily,
    err::NlError,
    genl::{Genlmsghdr, Nlattr},
    impl_var,
    nl::{NlPayload, Nlmsghdr},
    socket::NlSocketHandle,
    types::{Buffer, GenlBuffer},
    Nl,
};

impl_var!(
    NbdCmd, u8,
    Unspec => 0,
    Connect => 1,
    Disconnect => 2,
    Reconfigure => 3,
    LinkDead => 4,
    Status => 5
);
impl neli::consts::genl::Cmd for NbdCmd {}

impl_var!(
    NbdAttr, u16,
    Unspec => 0,
    Index => 1,
    SizeBytes => 2,
    BlockSizeBytes => 3,
    Timeout => 4,
    ServerFlags => 5,
    ClientFlags => 6,
    Sockets => 7,
    DeadConnTimeout => 8,
    DeviceList => 9
);
impl NlAttrType for NbdAttr {}

impl_var!(
    NbdSockItem, u16,
    Unspec => 0,
    Item => 1
);
impl NlAttrType for NbdSockItem {}

impl_var!(
    NbdSock, u16,
    Unspec => 0,
    Fd => 1
);
impl NlAttrType for NbdSock {}

const HAS_FLAGS: u64 = 1 << 0;
const READ_ONLY: u64 = 1 << 1;
const CAN_MULTI_CONN: u64 = 1 << 8;

const NBD_CFLAG_DISCONNECT_ON_CLOSE: u64 = 1 << 1;

/// An NBD netlink socket, usable to set up NBD devices.
pub struct NBD {
    nl: NlSocketHandle,
    nbd_family: u16,
}

impl NBD {
    /// Create a new NBD netlink socket.
    ///
    /// This will open a netlink socket and attempt to resolve the NBD generic netlink family. If
    /// the kernel does not have `nbd` support, or if it has `nbd` built as a module and not
    /// loaded, this will result in an error.
    pub fn new() -> anyhow::Result<Self> {
        let mut nl = NlSocketHandle::new(NlFamily::Generic)?;
        let nbd_family = nl
            .resolve_genl_family("nbd")
            .context("Could not resolve the NBD generic netlink family")?;
        Ok(Self { nl, nbd_family })
    }
}

/// A builder for an NBD connect call.
pub struct NBDConnect {
    size_bytes: u64,
    block_size_bytes: u64,
    server_flags: u64,
    client_flags: u64,
}

impl NBDConnect {
    /// Create a new NBDConnect builder.
    pub fn new() -> Self {
        Self {
            size_bytes: 0,
            block_size_bytes: 4096,
            server_flags: HAS_FLAGS,
            client_flags: 0,
        }
    }

    /// Set the size for the NBD device, in bytes. Defaults to 0 if not specified.
    pub fn size_bytes(&mut self, bytes: u64) -> &mut Self {
        self.size_bytes = bytes;
        self
    }

    /// Set the minimum block size for the NBD device, in bytes. Defaults to 4096 if not specified.
    pub fn block_size(&mut self, bytes: u64) -> &mut Self {
        self.block_size_bytes = bytes;
        self
    }

    /// Set the device as read-only.
    pub fn read_only(&mut self, read_only: bool) -> &mut Self {
        if read_only {
            self.server_flags |= READ_ONLY;
        } else {
            self.server_flags &= !READ_ONLY;
        }
        self
    }

    /// Set the device as allowing multiple concurrent socket connections.
    pub fn can_multi_conn(&mut self, can_multi_conn: bool) -> &mut Self {
        if can_multi_conn {
            self.server_flags |= CAN_MULTI_CONN;
        } else {
            self.server_flags &= !CAN_MULTI_CONN;
        }
        self
    }

    /// Set the device to disconnect the NBD connection when closed for the last time.
    pub fn disconnect_on_close(&mut self, disconnect_on_close: bool) -> &mut Self {
        if disconnect_on_close {
            self.client_flags |= NBD_CFLAG_DISCONNECT_ON_CLOSE;
        } else {
            self.client_flags &= !NBD_CFLAG_DISCONNECT_ON_CLOSE;
        }
        self
    }

    /// Tell the kernel to connect an NBD device to the specified sockets.
    ///
    /// Returns the index of the newly connected NBD device.
    pub fn connect<'a>(
        &self,
        nbd: &mut NBD,
        sockets: impl IntoIterator<Item = &'a (impl AsRawFd + 'a)>,
    ) -> anyhow::Result<u32> {
        fn attr<T: NlAttrType, P: Nl>(t: T, p: P) -> Result<Nlattr<T, Buffer>, NlError> {
            Nlattr::new(None, false, false, t, p)
        }
        let mut sockets_attr = Nlattr::new(None, true, false, NbdAttr::Sockets, Buffer::new())?;
        for socket in sockets {
            sockets_attr.add_nested_attribute(&Nlattr::new(
                None,
                true,
                false,
                NbdSockItem::Item,
                attr(NbdSock::Fd, socket.as_raw_fd())?,
            )?)?;
        }
        let mut attrs = GenlBuffer::new();
        attrs.push(attr(NbdAttr::SizeBytes, self.size_bytes)?);
        attrs.push(attr(NbdAttr::BlockSizeBytes, self.block_size_bytes)?);
        attrs.push(attr(NbdAttr::ServerFlags, self.server_flags)?);
        attrs.push(attr(NbdAttr::ClientFlags, self.client_flags)?);
        attrs.push(sockets_attr);

        let genl_header = Genlmsghdr::new(NbdCmd::Connect, 1, attrs);
        let nl_header = Nlmsghdr::new(
            None,
            nbd.nbd_family,
            NlmFFlags::new(&[NlmF::Request]),
            None,
            None,
            NlPayload::Payload(genl_header),
        );
        nbd.nl.send(nl_header)?;
        let response: Nlmsghdr<u16, Genlmsghdr<NbdCmd, NbdAttr>> = nbd
            .nl
            .recv()?
            .ok_or_else(|| anyhow!("Error connecting NBD device: No response received"))?;
        let handle = response.get_payload()?.get_attr_handle();
        let index = handle.get_attr_payload_as::<u32>(NbdAttr::Index)?;
        Ok(index)
    }
}
