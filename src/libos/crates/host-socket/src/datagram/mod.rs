//! Datagram sockets.
mod generic;
mod netlink; // Netlink is a datagram-oriented service and thus can reuse most of the code here
mod receiver;
mod sender;

use self::receiver::Receiver;
use self::sender::Sender;
use crate::common::{do_bind, Common};
use crate::ioctl::*;
use crate::prelude::*;
use crate::runtime::Runtime;
use crate::sockopt::*;

pub use generic::DatagramSocket;
pub use netlink::NetlinkSocket;

const MAX_BUF_SIZE: usize = 64 * 1024;
