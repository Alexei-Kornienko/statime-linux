#![forbid(unsafe_code)]

//! Event and General sockets for linux systems

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

use timestamped_socket::{
    interface::InterfaceName,
    networkaddress::{EthernetAddress, MacAddress},
    socket::{
        open_interface_ethernet, open_interface_udp4, open_interface_udp6, InterfaceTimestampMode,
        Open, Socket,
    },
};

const IPV4_PRIMARY_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
const IPV4_PDELAY_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 107);

const EVENT_PORT: u16 = 319;
const GENERAL_PORT: u16 = 320;

pub trait PtpTargetAddress {
    const PRIMARY_EVENT: Self;
    const PRIMARY_GENERAL: Self;
    const PDELAY_EVENT: Self;
    const PDELAY_GENERAL: Self;
}

impl PtpTargetAddress for SocketAddrV4 {
    const PRIMARY_EVENT: Self = SocketAddrV4::new(IPV4_PRIMARY_MULTICAST, EVENT_PORT);
    const PRIMARY_GENERAL: Self = SocketAddrV4::new(IPV4_PRIMARY_MULTICAST, GENERAL_PORT);
    const PDELAY_EVENT: Self = SocketAddrV4::new(IPV4_PDELAY_MULTICAST, EVENT_PORT);
    const PDELAY_GENERAL: Self = SocketAddrV4::new(IPV4_PDELAY_MULTICAST, GENERAL_PORT);
}

pub fn open_ipv4_event_socket(
    interface: InterfaceName,
    timestamping: InterfaceTimestampMode,
    bind_phc: Option<u32>,
) -> std::io::Result<Socket<SocketAddrV4, Open>> {
    let socket = open_interface_udp4(interface, EVENT_PORT, timestamping, bind_phc)?;
    socket.join_multicast(SocketAddrV4::new(IPV4_PRIMARY_MULTICAST, 0), interface)?;
    socket.join_multicast(SocketAddrV4::new(IPV4_PDELAY_MULTICAST, 0), interface)?;
    Ok(socket)
}

pub fn open_ipv4_general_socket(
    interface: InterfaceName,
) -> std::io::Result<Socket<SocketAddrV4, Open>> {
    let socket = open_interface_udp4(interface, GENERAL_PORT, InterfaceTimestampMode::None, None)?;
    socket.join_multicast(SocketAddrV4::new(IPV4_PRIMARY_MULTICAST, 0), interface)?;
    socket.join_multicast(SocketAddrV4::new(IPV4_PDELAY_MULTICAST, 0), interface)?;
    Ok(socket)
}
