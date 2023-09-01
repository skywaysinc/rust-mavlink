//! Implements primitives to allow a fast routing of Mavlink messages.
//!
//!

use crate::connection::direct_serial::SerialConnection;
use crate::connection::tcp::TcpConnection;
use crate::connection::udp::UdpConnection;
use crate::read_raw_message;
use crate::read_v1_raw_message;
use crate::read_v2_raw_message;
use crate::CommonMessageRaw;
use crate::MAVLinkV1MessageRaw;
use crate::MAVLinkV2MessageRaw;
use crate::Message;
use core::cell::RefCell;
use log::debug;
use serialport::SerialPort;
use std::collections::HashMap;
use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::Mutex;

pub enum MAVLinkMessageRaw {
    V1(MAVLinkV1MessageRaw),
    V2(MAVLinkV2MessageRaw),
}

impl MAVLinkMessageRaw {
    pub fn get_target_system_component_ids<M: Message>(&self) -> (u8, u8) {
        let msg_id = self.message_id();
        let (target_system_id_offset, target_component_id_offset) =
            M::target_offsets_from_id(msg_id);

        match (target_system_id_offset, target_component_id_offset) {
            (None, None) => (0u8, 0u8),
            (None, Some(_)) => todo!(), // should not happen.
            (Some(target_system_id_offset), None) => {
                // Mavlink v2 protocol assumes trailing zeros for anything over the communicated
                // length.
                if target_system_id_offset >= self.payload_length().into() {
                    (0, 0)
                } else {
                    let target_system_id = self.payload()[target_system_id_offset];
                    (target_system_id, 0)
                }
            }
            (Some(target_system_id_offset), Some(target_component_id_offset)) => {
                // Mavlink v2 protocol assumes trailing zeros for anything over the communicated
                // length.
                let mut target_system_id = 0;
                let mut target_component_id = 0;
                if target_system_id_offset < self.payload_length().into() {
                    target_system_id = self.payload()[target_system_id_offset];
                }
                if target_component_id_offset < self.payload_length().into() {
                    target_component_id = self.payload()[target_component_id_offset];
                }
                (target_system_id, target_component_id)
            }
        }
    }
}
/// TODO(gbin): There must be a more elegant way to do this
impl CommonMessageRaw for MAVLinkMessageRaw {
    #[inline]
    fn message_id(&self) -> u32 {
        match self {
            Self::V1(m) => m.message_id(),
            Self::V2(m) => m.message_id(),
        }
    }

    #[inline]
    fn system_id(&self) -> u8 {
        match self {
            Self::V1(m) => m.system_id(),
            Self::V2(m) => m.system_id(),
        }
    }

    #[inline]
    fn component_id(&self) -> u8 {
        match self {
            Self::V1(m) => m.component_id(),
            Self::V2(m) => m.component_id(),
        }
    }

    #[inline]
    fn len(&self) -> usize {
        match self {
            Self::V1(m) => m.len(),
            Self::V2(m) => m.len(),
        }
    }

    #[inline]
    fn full(&self) -> &[u8] {
        match self {
            Self::V1(m) => m.full(),
            Self::V2(m) => m.full(),
        }
    }

    #[inline]
    fn full_mut(&mut self) -> &mut [u8] {
        match self {
            Self::V1(m) => &mut m.0,
            Self::V2(m) => &mut m.0,
        }
    }

    fn payload_length(&self) -> usize {
        match self {
            Self::V1(m) => m.payload_length(),
            Self::V2(m) => m.payload_length(),
        }
    }

    fn payload(&self) -> &[u8] {
        match self {
            Self::V1(m) => m.payload(),
            Self::V2(m) => m.payload(),
        }
    }

    fn get_version(&self) -> crate::MavlinkVersion {
        match self {
            Self::V1(_) => crate::MavlinkVersion::V1,
            Self::V2(_) => crate::MavlinkVersion::V2,
        }
    }
}

/// A RawConnection is a contract for a MavConnection with a
/// couple of functions that allow a bypass of the creation/parsing of messages for fast routing
/// between mavlink connections
/// The Message generic is necessary as we check the message validity from its CRC which is Mavlink
/// version dependent.
pub trait RawConnection<M: Message>
where
    Self: Sync + Send,
{
    fn raw_write(&self, raw_msg: &mut MAVLinkMessageRaw) -> io::Result<usize>;
    fn raw_read(&self) -> io::Result<MAVLinkMessageRaw>;
    fn connection_id(&self) -> String;
}

pub struct ThrottledConnection<M: Message> {
    connection: Box<dyn RawConnection<M>>,
    throttle: std::time::Duration,
    last_writes: Mutex<RefCell<HashMap<u32, std::time::Instant>>>,
}

impl<M: Message> ThrottledConnection<M> {
    pub fn new(connection: Box<dyn RawConnection<M>>, throttle: std::time::Duration) -> Self {
        Self {
            connection,
            throttle,
            last_writes: Mutex::new(RefCell::new(HashMap::new())),
        }
    }
}

impl<M: Message> RawConnection<M> for ThrottledConnection<M> {
    fn raw_write(&self, raw_msg: &mut MAVLinkMessageRaw) -> io::Result<usize> {
        let msg_id = raw_msg.message_id();
        let (target_system_id, target_component_id) =
            raw_msg.get_target_system_component_ids::<M>();
        // If this is not a global broadcast, we don't throttle
        if target_system_id != 0 || target_component_id != 0 {
            return self.connection.raw_write(raw_msg);
        }
        let now = std::time::Instant::now();
        let last_writes = self.last_writes.lock().unwrap();
        let last_write = last_writes.borrow().get(&msg_id).cloned().unwrap_or(now);
        if now - last_write > self.throttle {
            last_writes.borrow_mut().insert(msg_id, now);
            return self.connection.raw_write(raw_msg);
        }

        Ok(0)
    }

    fn raw_read(&self) -> io::Result<MAVLinkMessageRaw> {
        self.connection.raw_read()
    }

    fn connection_id(&self) -> String {
        self.connection.connection_id() + " (throttled)"
    }
}

impl<M: Message> RawConnection<M> for UdpConnection {
    fn raw_write(&self, msg: &mut MAVLinkMessageRaw) -> io::Result<usize> {
        let mut guard = self.writer.lock().unwrap();
        let state = &mut *guard;
        let bf = std::time::Instant::now();
        state.sequence = state.sequence.wrapping_add(1);

        let len = if let Some(addr) = state.dest {
            state.socket.send_to(msg.full(), addr)?
        } else {
            0
        };
        if bf.elapsed().as_millis() > 100 {
            debug!("Took too long to write UDP: {}ms", bf.elapsed().as_millis());
        }

        Ok(len)
    }

    fn raw_read(&self) -> io::Result<MAVLinkMessageRaw> {
        let mut guard = self.reader.lock().unwrap();
        let state = &mut *guard;
        loop {
            debug!("Looping...");
            if state.recv_buf.len() == 0 {
                // debug!("UDP: Waiting for data");
                let (len, src) = match state.socket.recv_from(state.recv_buf.reset()) {
                    Ok((len, src)) => (len, src),
                    Err(error) => {
                        return Err(error);
                    }
                };
                // debug!("set_len");
                state.recv_buf.set_len(len);

                if self.server {
                    //debug!("writer");
                    self.writer.lock().unwrap().dest = Some(src);
                }
            }
            if state.recv_buf.slice()[0] == crate::MAV_STX {
                let Ok(msg) = read_v1_raw_message(&mut state.recv_buf) else {
                    //debug!("1st continue");
                    continue;
                };
                return Ok(MAVLinkMessageRaw::V1(msg));
            } else {
                if state.recv_buf.slice()[0] != crate::MAV_STX_V2 {
                    //debug!("2st continue");
                    state.recv_buf.reset();
                    continue;
                }
                let Ok(msg) = read_v2_raw_message(&mut state.recv_buf) else {
                    //debug!("3st continue");
                    continue;
                };
                if !msg.has_valid_crc::<M>() {
                    //debug!("4st continue");
                    continue;
                }
                //debug!("return message");
                return Ok(MAVLinkMessageRaw::V2(msg));
            }
        }
    }

    fn connection_id(&self) -> String {
        self.id.clone()
    }
}

impl<M: Message> RawConnection<M> for SerialConnection {
    fn raw_write(&self, msg: &mut MAVLinkMessageRaw) -> io::Result<usize> {
        let mut port = self.writer.lock().unwrap();
        let bf = std::time::Instant::now();
        let res = match (*port).write_all(msg.full()) {
            Ok(_) => Ok(msg.len()),
            Err(e) => Err(e),
        };

        if bf.elapsed().as_millis() > 100 {
            debug!(
                "Took too long to write serial: {}ms",
                bf.elapsed().as_millis()
            );
        }
        res
    }

    fn raw_read(&self) -> io::Result<MAVLinkMessageRaw> {
        let mut port = self.reader.lock().unwrap();

        loop {
            let Ok(msg) = read_raw_message::<Box<dyn SerialPort>, M>(&mut *port) else {
                continue;
            };
            match msg {
                // This is hard to generalize through a trait because of M
                MAVLinkMessageRaw::V1(v1) => {
                    if v1.has_valid_crc::<M>() {
                        return Ok(msg);
                    }
                }
                MAVLinkMessageRaw::V2(v2) => {
                    if v2.has_valid_crc::<M>() {
                        return Ok(msg);
                    }
                }
            };
        }
    }

    fn connection_id(&self) -> String {
        self.id.clone() // FIXME(gbin)
    }
}

impl<M: Message> RawConnection<M> for TcpConnection {
    fn raw_write(&self, msg: &mut MAVLinkMessageRaw) -> io::Result<usize> {
        let mut stream = self.writer.lock().unwrap();

        match stream.socket.write_all(msg.full()) {
            Ok(_) => Ok(msg.len()),
            Err(e) => Err(e),
        }
    }

    fn raw_read(&self) -> io::Result<MAVLinkMessageRaw> {
        let mut stream = self.reader.lock().expect("tcp read failure");
        loop {
            let Ok(msg) = read_raw_message::<TcpStream, M>(&mut *stream) else {
                continue;
            };

            match msg {
                // This is hard to generalize through a trait because of M
                MAVLinkMessageRaw::V1(v1) => {
                    if v1.has_valid_crc::<M>() {
                        return Ok(msg);
                    }
                }
                MAVLinkMessageRaw::V2(v2) => {
                    if v2.has_valid_crc::<M>() {
                        return Ok(msg);
                    }
                }
            };
        }
    }

    fn connection_id(&self) -> String {
        self.id.clone() //FIXME(gbin)
    }
}
