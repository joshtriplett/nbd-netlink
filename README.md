`nbd-netlink` supports setting up an NBD device for a specified socket and
parameters, using the Linux kernel's netlink interface to NBD. Unlike the
`ioctl`-based interface, the netlink interface can hand off a socket to the
kernel without leaving a thread or process running.

# Example

```rust
use std::net::{Ipv4Addr, TcpStream};
use nbd_netlink::{NBD, NBDConnect};
let nbd_socket = TcpStream::connect((Ipv4Addr::LOCALHOST, 10809))?;
nbd_socket.set_nodelay(true);
let mut nbd = NBD::new()?;
let index = NBDConnect::new()
    .size_bytes(1048576)
    .read_only(true)
    .connect(&mut nbd, &[nbd_socket])?;
```
