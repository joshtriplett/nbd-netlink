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

use anyhow::{bail, Context};
use neli::{
    consts::{NlFamily, NlmF},
    genl::Genlmsghdr,
    nl::Nlmsghdr,
    nlattr::Nlattr,
    socket::NlSocket,
};

// `neli::impl_var_trait` defines a pub enum, so wrap it in a private module to avoid exposing it.
mod private {
    use neli::{
        consts::{Cmd, NlAttrType},
        {impl_var, impl_var_base, impl_var_trait},
    };

    impl_var_trait!(
        NbdCmd, u8, Cmd,
        Unspec => 0,
        Connect => 1,
        Disconnect => 2,
        Reconfigure => 3,
        LinkDead => 4,
        Status => 5
    );

    impl_var_trait!(
        NbdAttr, u16, NlAttrType,
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

    impl_var_trait!(
        NbdSockItem, u16, NlAttrType,
        Unspec => 0,
        Item => 1
    );

    impl_var_trait!(
        NbdSock, u16, NlAttrType,
        Unspec => 0,
        Fd => 1
    );
}
use private::*;

const HAS_FLAGS: u64 = 1 << 0;
const READ_ONLY: u64 = 1 << 1;

const NBD_CFLAG_DISCONNECT_ON_CLOSE: u64 = 1 << 1;

/// An NBD netlink socket, usable to set up NBD devices.
pub struct NBD {
    nl: NlSocket,
    nbd_family: u16,
    _not_sync: std::marker::PhantomData<std::cell::Cell<()>>,
}

impl NBD {
    /// Create a new NBD netlink socket.
    ///
    /// This will open a netlink socket and attempt to resolve the NBD generic netlink family. If
    /// the kernel does not have `nbd` support, or if it has `nbd` built as a module and not
    /// loaded, this will result in an error.
    pub fn new() -> anyhow::Result<Self> {
        let mut nl = NlSocket::new(NlFamily::Generic, true)?;
        let nbd_family = nl
            .resolve_genl_family("nbd")
            .context("Could not resolve the NBD generic netlink family")?;
        Ok(Self { nl, nbd_family, _not_sync: std::marker::PhantomData })
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
        let mut sockets_attr = Nlattr::new(None, NbdAttr::Sockets, Vec::<u8>::new())?;
        for socket in sockets {
            sockets_attr.add_nested_attribute(&Nlattr::new(
                None,
                NbdSockItem::Item,
                Nlattr::new(None, NbdSock::Fd, socket.as_raw_fd())?,
            )?)?;
        }
        let attrs = vec![
            Nlattr::new(None, NbdAttr::SizeBytes, self.size_bytes)?,
            Nlattr::new(None, NbdAttr::BlockSizeBytes, self.block_size_bytes)?,
            Nlattr::new(None, NbdAttr::ServerFlags, self.server_flags)?,
            Nlattr::new(None, NbdAttr::ClientFlags, self.client_flags)?,
            sockets_attr,
        ];

        let genl_header = Genlmsghdr::new(NbdCmd::Connect, 1, attrs)?;
        let nl_header = Nlmsghdr::new(
            None,
            nbd.nbd_family,
            vec![NlmF::Request],
            None,
            None,
            genl_header,
        );
        nbd.nl.send_nl(nl_header)?;
        let response: Nlmsghdr<u16, Genlmsghdr<NbdCmd, NbdAttr>> = nbd.nl.recv_nl(None)?;
        if response.nl_type != nbd.nbd_family {
            bail!("Error connecting NBD device");
        }
        let handle = response.nl_payload.get_attr_handle();
        let index = handle.get_attr_payload_as::<u32>(NbdAttr::Index)?;
        Ok(index)
    }
}
