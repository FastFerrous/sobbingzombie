use rand::{Rng, RngExt};
use sozo_api::BusMessage;

/*
* INBOUND_MESSAGE_SIZE  - Maximum packet length limitation
* MAXIMUM_DATA_SIZE     - Maximum data or padding length
*/
const INBOUND_MESSAGE_SIZE: usize = 8192;
pub const MAXIMUM_DATA_SIZE: u32 = 4096;

#[repr(C, packed)]
pub struct PacketHeader {
    total_size: u64,
    pub identity: u32,
    pub data_len: u32,
    pub pad_len: u32,
}

impl PacketHeader {
    pub fn hdr_from_bytes(bytes: &[u8]) -> Result<PacketHeader, ()> {
        if size_of::<PacketHeader>() != bytes.len() {
            return Err(());
        }

        /* macro utilized to parse the bytes via type size and extract the required values of the header structure */
        let mut pos = 0;
        macro_rules! extract {
            ($t:ty) => {{
                let size = size_of::<$t>();
                let val = <$t>::from_be_bytes(bytes[pos..pos + size].try_into().map_err(|_| ())?);
                pos += size;
                val
            }};
        }

        let pkt_header = PacketHeader {
            total_size: extract!(u64),
            identity: extract!(u32),
            data_len: extract!(u32),
            pad_len: extract!(u32),
        };

        /* perform header validation */
        let calculated_size =
            size_of::<PacketHeader>() as u64 + (pkt_header.data_len + pkt_header.pad_len) as u64;

        if INBOUND_MESSAGE_SIZE < pkt_header.total_size as usize
            || pkt_header.total_size != calculated_size
        {
            return Err(());
        }

        if MAXIMUM_DATA_SIZE < pkt_header.data_len || MAXIMUM_DATA_SIZE < pkt_header.pad_len {
            return Err(());
        }

        Ok(pkt_header)
    }

    pub fn craft_packet(msg: BusMessage) -> Result<Vec<u8>, ()> {
        /* extract remote identity that will be used by c2 to determine where packet needs to route */
        let identity = match msg.remote {
            Some(remote) => remote,
            None => return Err(()),
        };

        /* get random garbage data for padding */
        let padding = match Self::get_padding(msg.msg.len() as f32) {
            Ok(padding) => padding,
            Err(_) => return Err(()),
        };

        let total_size =
            size_of::<PacketHeader>() as u64 + msg.msg.len() as u64 + padding.len() as u64;

        let mut packet: Vec<u8> = Vec::new();
        if packet.try_reserve(total_size as usize).is_err() {
            return Err(());
        }

        packet.extend_from_slice(total_size.to_be_bytes().as_ref());
        packet.extend_from_slice(&identity.0.to_be_bytes());
        packet.extend_from_slice((msg.msg.len() as u32).to_be_bytes().as_ref());
        packet.extend_from_slice((padding.len() as u32).to_be_bytes().as_ref());
        packet.extend_from_slice(msg.msg.as_ref());
        packet.extend_from_slice(&padding);

        Ok(packet)
    }

    fn get_padding(data_len: f32) -> Result<Vec<u8>, ()> {
        /* calculate random padding length based off .05% - .15% of the provided data_len */
        let mut rng = rand::rng();
        let min_pad = ((data_len * 0.05) as usize).max(1);
        let max_pad = ((data_len * 0.15) as usize).max(min_pad + 1);
        let pad_len = rng.random_range(min_pad..=max_pad);

        /* attempt to allocate the vector and fill with rng bytes */
        let mut padding: Vec<u8> = Vec::new();
        if padding.try_reserve(pad_len).is_err() {
            return Err(());
        }

        padding.resize(pad_len, 0);
        rng.fill_bytes(&mut padding);

        Ok(padding)
    }
}
